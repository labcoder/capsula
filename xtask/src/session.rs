//! Bounded, explicitly approved input sessions against owned fixture/product processes.
#![expect(
    clippy::print_stdout,
    reason = "session output contains only counters and pass checks"
)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use windows::Win32::Foundation::RECT;

use crate::platform::{
    self,
    input_session::{
        self as native, FixtureEscMode, InputEdge, Layout, Layouts, Session, Target, Window,
        WindowKind,
    },
};

mod diagnostics;
use diagnostics::pair_paints;
pub(crate) use diagnostics::{Mark, Record};

#[derive(Debug)]
pub(crate) struct PhaseContext<'a> {
    pub(crate) dock: RECT,
    pub(crate) overlay: RECT,
    pub(crate) observer: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Keyboard,
    Hover,
    Focus,
    Layouts,
    Esc,
}

#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub(crate) attempts: u32,
    pub(crate) sent: u32,
    pub(crate) behavior_pass: u32,
    pub(crate) behavior_fail: u32,
    altgr_windows: Vec<(i64, i64)>,
}

#[derive(Debug)]
pub(crate) struct Diagnostics {
    path: PathBuf,
}

impl Diagnostics {
    pub(crate) fn in_directory(directory: &Path) -> Self {
        Self {
            path: directory.join("milestones.log"),
        }
    }

    pub(crate) fn argument(&self) -> OsString {
        let mut argument = OsString::from("--input-diagnostics=");
        argument.push(&self.path);
        argument
    }

    fn read(&self) -> Result<Vec<Record>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        text.split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| Record::parse(line.trim_end_matches(['\r', '\n'])))
            .collect()
    }

    pub(crate) fn canvas_pairs(&self) -> Result<Vec<(i64, i64)>> {
        let (pairs, _) = pair_paints(&self.read()?)?;
        Ok(pairs
            .into_iter()
            .map(|(start, paint, _)| (start, paint))
            .collect())
    }

    pub(crate) fn await_started(&self, session: &Session) -> Result<()> {
        self.observe(session, 0, |records| {
            records
                .iter()
                .any(|record| matches!(record.mark, Mark::EscapeState { .. }))
        })?;
        Ok(())
    }

    pub(crate) fn observe(
        &self,
        session: &Session,
        since: i64,
        ready: impl Fn(&[Record]) -> bool,
    ) -> Result<Vec<Record>> {
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            session.guard()?;
            let records = self
                .read()?
                .into_iter()
                .filter(|record| record.qpc >= since)
                .collect::<Vec<_>>();
            if ready(&records) {
                return Ok(records);
            }
            ensure!(
                Instant::now() < until,
                "near-live input diagnostic acknowledgement timed out"
            );
            session.wait(Duration::from_millis(1))?;
        }
    }
}

pub(crate) fn check_flushed(
    phase: Phase,
    contested: FixtureEscMode,
    diagnostics: &Diagnostics,
    counts: &mut Counts,
) -> Result<()> {
    let records = diagnostics.read()?;
    ensure!(!records.is_empty(), "no input diagnostics were flushed");
    let count = |mark: Mark| records.iter().filter(|record| record.mark == mark).count();
    if phase == Phase::Keyboard && counts.sent == 101 {
        ensure!(
            records
                .iter()
                .filter(|record| matches!(record.mark, Mark::Applied { .. }))
                .count()
                == 101
                && records
                    .iter()
                    .filter(|record| matches!(record.mark, Mark::Painted(_)))
                    .count()
                    == 101
                && count(Mark::Resting) == 101
                && count(Mark::Hold(true)) == 1
                && count(Mark::HoldEnded(true)) == 1
                && records.iter().all(|record| !matches!(
                    record.mark,
                    Mark::Opened(_) | Mark::Hold(false) | Mark::HoldEnded(false)
                )),
            "flushed diagnostics do not match 100 short chords plus one excluded/restored hold"
        );
    }
    if phase == Phase::Layouts {
        check_layout_diagnostics(&records, counts)?;
    }
    if phase == Phase::Focus {
        let actions = records
            .iter()
            .filter(|record| {
                matches!(
                    record.mark,
                    Mark::EscapeStop | Mark::EscapeClear | Mark::EscapeDismiss | Mark::EscapeIgnore
                )
            })
            .count();
        ensure!(
            actions == 2
                && records
                    .iter()
                    .any(|record| record.mark == Mark::EscapeDismiss),
            "ordinary fixture Escape was consumed by the product or the focused-panel dismissal action was missing"
        );
    }
    if phase == Phase::Esc && !contested.is_reserved() {
        ensure!(
            count(Mark::EscapeIgnore) == 1,
            "scoped Esc fixture did not consume exactly one Escape"
        );
    }
    let applied = records
        .iter()
        .filter_map(|record| match record.mark {
            Mark::Applied { us, .. } => Some(us),
            _ => None,
        })
        .collect::<Vec<_>>();
    let painted = records
        .iter()
        .filter_map(|record| match record.mark {
            Mark::Painted(us) => Some(us),
            _ => None,
        })
        .max();
    let (pairs, missing) = pair_paints(&records)?;
    println!(
        "canvas_paired_samples={} missing_paint_samples={missing}",
        pairs.len()
    );
    for (start_qpc, paint_qpc, painted_us) in pairs {
        println!(
            "canvas_pair start_qpc={start_qpc} paint_qpc={paint_qpc} painted_us={painted_us}; NOT display/present latency"
        );
    }
    println!(
        "diagnostic applied_us_samples={applied:?} excludes OS queue; painted_us_max={painted:?} is Canvas draw callback, NOT display/present latency"
    );
    Ok(())
}

fn check_layout_diagnostics(records: &[Record], counts: &mut Counts) -> Result<()> {
    let count = |mark: Mark| records.iter().filter(|record| record.mark == mark).count();
    let quiet = records.iter().all(|record| match record.mark {
        Mark::Applied { start_qpc, .. } => !counts
            .altgr_windows
            .iter()
            .any(|(start, end)| (*start..*end).contains(&start_qpc)),
        Mark::Hold(_) | Mark::Opened(_) => false,
        _ => true,
    });
    counts.behavior_pass += u32::from(quiet);
    counts.behavior_fail += u32::from(!quiet);
    ensure!(
        quiet,
        "AltGr-only windows emitted Listening or layouts emitted an unintended hold/input opening"
    );
    if counts.sent == 4 {
        ensure!(
            counts.altgr_windows.len() == 2
                && records
                    .iter()
                    .filter(|record| matches!(record.mark, Mark::Applied { .. }))
                    .count()
                    == 2
                && count(Mark::Resting) == 2,
            "positive left-modifier chords did not complete under both layouts"
        );
    }
    for (start, end) in &counts.altgr_windows {
        println!("altgr_only_window start_qpc={start} next_left_chord_qpc={end}");
    }
    Ok(())
}

fn exercise_escape(
    contested: FixtureEscMode,
    session: &mut Session,
    counts: &mut Counts,
    diagnostics: &Diagnostics,
    context: &PhaseContext<'_>,
) -> Result<()> {
    diagnostics.observe(session, 0, |records| {
        records.iter().any(|record| {
            record.mark
                == (Mark::EscapeState {
                    wanted: true,
                    stop: contested.is_reserved(),
                })
        })
    })?;
    if contested.is_reserved() {
        let visible = session.panel_visible()?;
        counts.behavior_fail += u32::from(!visible);
        ensure!(
            visible,
            "contested Escape fallback panel was not visible before any pointer movement"
        );
        counts.attempts += 1;
        let [enter, leave] = session.hover_points(context.dock, context.overlay)?;
        session.move_pointer(enter, Target::Product)?;
        session.wait(Duration::from_millis(100))?;
        session.move_pointer(leave, Target::Edit)?;
        counts.sent += 1;
        session.wait(Duration::from_millis(300))?;
        let visible = session.panel_visible()?;
        counts.behavior_pass += u32::from(visible);
        counts.behavior_fail += u32::from(!visible);
        ensure!(
            visible,
            "contested Escape fallback panel did not persist after pointer leave"
        );
        println!(
            "contested registration stop_flag=true panel_visible_before_movement=true panel_persisted_after_leave=true fallback_button_click=unverified; no guessed click injected"
        );
        return Ok(());
    }
    let (_, before) = session.fixture.counts()?;
    counts.attempts += 1;
    session.tap(0x01)?;
    let since = session
        .last_qpc
        .context("scoped Escape injection QPC missing")?;
    counts.sent += 1;
    diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| record.mark == Mark::EscapeIgnore)
            && records.iter().any(|record| {
                record.mark
                    == (Mark::EscapeState {
                        wanted: false,
                        stop: false,
                    })
            })
    })?;
    let (_, consumed) = session.fixture.counts()?;
    ensure!(
        consumed == before,
        "registered scoped Escape leaked to the fixture"
    );
    counts.attempts += 1;
    session.tap(0x01)?;
    counts.sent += 1;
    session.wait(Duration::from_millis(100))?;
    let (_, after) = session.fixture.counts()?;
    let passed = after == before + 1;
    counts.behavior_pass += u32::from(passed);
    counts.behavior_fail += u32::from(!passed);
    ensure!(
        passed,
        "ordinary Escape did not reach the fixture after global registration released"
    );
    Ok(())
}

pub(crate) fn stop_product(
    shell: Option<&mut Child>,
    shell_window: Option<Window>,
    helpers: Vec<(&str, platform::Process, u32)>,
    end: Instant,
) -> Result<()> {
    let mut cleanup = Vec::new();
    if let Some(window) = shell_window {
        cleanup.push(window.close());
    }
    if let Some(child) = shell {
        cleanup.push(wait_child(child, end));
    }
    for (role, process, expected) in helpers {
        cleanup.push((|| {
            loop {
                if let Some(code) = process.exit_code()? {
                    println!("owned {role} pid {} exit={code}", process.pid);
                    ensure!(
                        code == expected,
                        "owned helper exit code differs from graceful cleanup expectation"
                    );
                    return Ok(());
                }
                ensure!(
                    Instant::now() < end,
                    "owned helper cleanup deadline exceeded; no force kill"
                );
                sleep(Duration::from_millis(10));
            }
        })());
    }
    combine(cleanup)
}

pub(crate) fn owned_helpers(shell_pid: u32) -> Result<Vec<(&'static str, platform::Process, u32)>> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let processes = platform::processes()?;
        let pair = processes
            .iter()
            .find(|entry| !entry.shell && entry.parent == shell_pid)
            .and_then(|brain| {
                processes
                    .iter()
                    .find(|entry| !entry.shell && entry.parent == brain.pid)
                    .map(|worker| (brain.pid, worker.pid))
            });
        if let Some((brain, worker)) = pair {
            ensure!(
                processes.len() == 3,
                "product process set changed before input session; refusing contaminated run"
            );
            return Ok(vec![
                ("brain", platform::Process::open(brain)?, 0),
                ("screen-worker", platform::Process::open(worker)?, 1),
            ]);
        }
        ensure!(
            Instant::now() < until,
            "owned brain/screen-worker startup timed out"
        );
        sleep(Duration::from_millis(10));
    }
}

pub(crate) fn discover(child: &mut Child, kind: WindowKind) -> Result<Window> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        let found = native::owned_window(child, kind);
        if found.is_ok() || Instant::now() >= end || child.try_wait()?.is_some() {
            return found;
        }
        sleep(Duration::from_millis(10));
    }
}

pub(crate) fn wait_child(child: &mut Child, end: Instant) -> Result<()> {
    loop {
        if let Some(status) = child.try_wait()? {
            println!("owned pid {} exit={status}", child.id());
            ensure!(
                status.success(),
                "owned session process exited unsuccessfully"
            );
            return Ok(());
        }
        ensure!(
            Instant::now() < end,
            "owned process cleanup deadline exceeded; no force kill; bounded child autoexit remains armed"
        );
        sleep(Duration::from_millis(10));
    }
}

pub(crate) fn combine(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    let errors = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn short_chord(session: &mut Session) -> Result<i64> {
    session.modifier(0x1d, InputEdge::Down)?;
    session.modifier(0x38, InputEdge::Down)?;
    let make_qpc = session.last_qpc.context("chord make QPC missing")?;
    session.wait(Duration::from_millis(50))?;
    session.modifier(0x38, InputEdge::Up)?;
    let break_qpc = session.last_qpc;
    session.modifier(0x1d, InputEdge::Up)?;
    println!("chord injection_make_qpc={make_qpc:?} injection_break_qpc={break_qpc:?}");
    Ok(make_qpc)
}

pub(crate) fn exercise(
    phase: Phase,
    contested: FixtureEscMode,
    session: &mut Session,
    counts: &mut Counts,
    diagnostics: &Diagnostics,
    context: &PhaseContext<'_>,
) -> Result<()> {
    println!("phase_observer={}", context.observer.display());
    let result = (|| {
        if phase != Phase::Esc {
            session.wait(Duration::from_millis(100))?;
        }
        match phase {
            Phase::Keyboard => keyboard(session, counts, diagnostics),
            Phase::Hover => hover(session, counts, context),
            Phase::Focus => focus(session, counts, diagnostics),
            Phase::Layouts => layouts(session, counts, diagnostics),
            Phase::Esc => exercise_escape(contested, session, counts, diagnostics, context),
        }
    })();
    let release = session.release_injected();
    println!(
        "SendInput accepted_batches={} first_qpc={:?} last_qpc={:?} qpc_frequency={}; injection timestamps only, NOT WM_INPUT-to-present latency",
        session.batches, session.first_qpc, session.last_qpc, session.qpc_frequency
    );
    combine([result, release])
}

fn keyboard(session: &mut Session, counts: &mut Counts, diagnostics: &Diagnostics) -> Result<()> {
    for index in 0..100 {
        session.wait(Duration::from_micros(1000 + (index * 997) % 6944))?;
        counts.attempts += 1;
        let since = short_chord(session)?;
        counts.sent += 1;
        session.wait(Duration::from_millis(325))?;
        let observed = diagnostics.observe(session, since, |records| {
            records
                .iter()
                .any(|record| matches!(record.mark, Mark::Applied { .. }))
                && records
                    .iter()
                    .any(|record| matches!(record.mark, Mark::Painted(_)))
                && records.iter().any(|record| record.mark == Mark::Resting)
        });
        let records = match observed {
            Ok(records) => records,
            Err(error) => {
                counts.behavior_fail += 1;
                println!(
                    "keyboard attempt={} failed: incomplete UI acknowledgement, sample retained as failed",
                    counts.attempts
                );
                return Err(error);
            }
        };
        let passed = records
            .iter()
            .filter(|record| matches!(record.mark, Mark::Applied { .. }))
            .count()
            == 1
            && records
                .iter()
                .filter(|record| matches!(record.mark, Mark::Painted(_)))
                .count()
                == 1
            && records
                .iter()
                .filter(|record| record.mark == Mark::Resting)
                .count()
                == 1
            && records
                .iter()
                .all(|record| !matches!(record.mark, Mark::Hold(_) | Mark::Opened(_)));
        counts.behavior_pass += u32::from(passed);
        counts.behavior_fail += u32::from(!passed);
        ensure!(
            passed,
            "short chord UI milestones were duplicated, missing or included an unintended hold/input opening"
        );
    }
    counts.attempts += 1;
    session.modifier(0x1d, InputEdge::Down)?;
    session.modifier(0x38, InputEdge::Down)?;
    let since = session.last_qpc.context("hold make QPC missing")?;
    session.wait(Duration::from_millis(350))?;
    let held = diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| matches!(record.mark, Mark::Hold(_)))
    })?;
    let excluded = held.iter().any(|record| record.mark == Mark::Hold(true));
    counts.behavior_fail += u32::from(!excluded);
    ensure!(excluded, "hold did not exclude owned capture surfaces");
    session.modifier(0x38, InputEdge::Up)?;
    session.modifier(0x1d, InputEdge::Up)?;
    counts.sent += 1;
    let ended = diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| matches!(record.mark, Mark::HoldEnded(_)))
            && records.iter().any(|record| record.mark == Mark::Resting)
    })?;
    let restored = ended
        .iter()
        .any(|record| record.mark == Mark::HoldEnded(true));
    counts.behavior_pass += u32::from(restored);
    counts.behavior_fail += u32::from(!restored);
    ensure!(
        restored,
        "hold release did not restore owned capture surfaces"
    );
    Ok(())
}

fn hover(session: &mut Session, counts: &mut Counts, context: &PhaseContext<'_>) -> Result<()> {
    let [enter, leave] = session.hover_points(context.dock, context.overlay)?;
    for index in 0..100 {
        session.wait(Duration::from_micros(1000 + (index * 997) % 6944))?;
        counts.attempts += 1;
        session.move_pointer(enter, Target::Product)?;
        let enter_qpc = session.last_qpc;
        session.wait(Duration::from_millis(100))?;
        let entered = session.panel_visible()?;
        counts.behavior_fail += u32::from(!entered);
        ensure!(
            entered,
            "hover entry did not make an owned panel HWND visible"
        );
        session.move_pointer(leave, Target::Edit)?;
        let leave_qpc = session.last_qpc;
        let leave_at = Instant::now();
        let until = leave_at + Duration::from_millis(300);
        while session.panel_visible()? && Instant::now() < until {
            session.wait(Duration::from_millis(5))?;
        }
        let observed_leave = leave_at.elapsed();
        let left = !session.panel_visible()?;
        counts.behavior_fail += u32::from(!left);
        ensure!(left, "hover leave did not hide the owned panel HWND");
        counts.sent += 1;
        counts.behavior_pass += 1;
        println!(
            "hover attempt={} injection_enter_qpc={enter_qpc:?} injection_leave_qpc={leave_qpc:?} observed_hwnd_leave_ms={:.3}; NOT paint/present latency",
            counts.attempts,
            observed_leave.as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

fn focus(session: &mut Session, counts: &mut Counts, diagnostics: &Diagnostics) -> Result<()> {
    counts.attempts += 1;
    let since = short_chord(session)?;
    session.wait(Duration::from_millis(50))?;
    short_chord(session)?;
    let records = diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| matches!(record.mark, Mark::Opened(_)))
    })?;
    if !records
        .iter()
        .any(|record| record.mark == Mark::Opened(true))
        || !session.product()?.is_foreground()
    {
        counts.behavior_fail += 1;
        bail!("completed short double tap did not foreground the product; no text injected");
    }
    session.tap(0x1e)?;
    session.tap(0x01)?;
    diagnostics.observe(session, since, |records| {
        records.iter().any(|record| record.mark == Mark::Dismissed)
            && records
                .iter()
                .any(|record| record.mark == Mark::EscapeDismiss)
    })?;
    if !session.fixture.is_foreground() {
        counts.behavior_fail += 1;
        bail!("Escape did not return foreground to the fixture");
    }
    session.wait(Duration::from_millis(325))?;
    let reopened_since = short_chord(session)?;
    session.wait(Duration::from_millis(50))?;
    short_chord(session)?;
    let reopened = diagnostics.observe(session, reopened_since, |records| {
        records
            .iter()
            .any(|record| matches!(record.mark, Mark::Opened(_)))
            && records
                .iter()
                .any(|record| matches!(record.mark, Mark::Draft { .. }))
    })?;
    if !reopened
        .iter()
        .any(|record| record.mark == Mark::Opened(true))
        || !session.product()?.is_foreground()
        || !reopened.iter().any(|record| {
            record.mark
                == (Mark::Draft {
                    nonempty: true,
                    preserved: true,
                })
        })
    {
        counts.behavior_fail += 1;
        bail!("draft reopen was unfocused, not foreground, empty or not preserved");
    }
    session.tap(0x01)?;
    diagnostics.observe(session, reopened_since, |records| {
        records.iter().any(|record| record.mark == Mark::Dismissed)
            && records
                .iter()
                .any(|record| record.mark == Mark::EscapeDismiss)
    })?;
    ensure!(
        session.fixture.is_foreground(),
        "draft dismissal did not return foreground to the fixture"
    );
    let (_, before) = session.fixture.counts()?;
    session.tap(0x01)?;
    counts.sent += 1;
    session.wait(Duration::from_millis(100))?;
    let (_, after) = session.fixture.counts()?;
    if after != before + 1 {
        counts.behavior_fail += 1;
        bail!("ordinary Escape did not reach the fixture");
    }
    counts.behavior_pass += 1;
    Ok(())
}

fn layouts(session: &mut Session, counts: &mut Counts, diagnostics: &Diagnostics) -> Result<()> {
    let layouts = Layouts::capture(session);
    let exercise = (|| {
        for selection in [Layout::German, Layout::Spanish] {
            counts.attempts += 1;
            Layouts::load(session, selection)?;
            session.clear_fixture()?;
            session.modifier(0x138, InputEdge::Down)?;
            let altgr_start = session.last_qpc.context("AltGr injection QPC missing")?;
            session.tap(match selection {
                Layout::German => 0x10,
                Layout::Spanish => 0x03,
            })?;
            session.tap(0x12)?;
            session.modifier(0x138, InputEdge::Up)?;
            counts.sent += 1;
            session.wait(Duration::from_millis(100))?;
            if session.expected_layout_text()? {
                counts.behavior_pass += 1;
            } else {
                counts.behavior_fail += 1;
            }
            counts.attempts += 1;
            let since = short_chord(session)?;
            counts.altgr_windows.push((altgr_start, since));
            counts.sent += 1;
            diagnostics.observe(session, since, |records| {
                records
                    .iter()
                    .any(|record| matches!(record.mark, Mark::Applied { .. }))
                    && records.iter().any(|record| record.mark == Mark::Resting)
            })?;
            counts.behavior_pass += 1;
        }
        ensure!(
            counts.behavior_fail == 0,
            "one or more expected @/euro fixture checks failed"
        );
        Ok(())
    })();
    let release = session.release_injected();
    let restore = layouts.restore(session);
    combine([exercise, release, restore])?;
    Ok(())
}
