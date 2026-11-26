//! Bounded, explicitly approved input sessions against owned fixture/product processes.
#![expect(
    clippy::print_stdout,
    reason = "session output contains only counters and pass checks"
)]

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::RECT;

use crate::platform::{
    self,
    input_session::{
        self as native, FixtureEscMode, InputEdge, Layout, Layouts, Session, Target, Window,
        WindowKind,
    },
};

mod diagnostics;
mod tree;
pub(crate) use diagnostics::{Mark, Record};
pub(crate) use tree::{ROLES, Startup, product_tree, unchanged};

#[derive(Debug)]
pub(crate) struct PhaseContext<'a> {
    pub(crate) dock: RECT,
    pub(crate) overlay: RECT,
    pub(crate) observer: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Hover,
    Focus,
    Layouts,
    /// One numbered batch of the Listening row.
    Listening(u8),
    Esc,
}

/// One hold that reached Listening: its chord's and its recognition's native callback QPCs, and
/// the QPCs at which the line took and left the Listening colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Heard {
    pub(crate) key_down: i64,
    pub(crate) recognized: i64,
    pub(crate) listening: i64,
    pub(crate) resting: i64,
}

#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub(crate) attempts: u32,
    pub(crate) sent: u32,
    pub(crate) behavior_pass: u32,
    pub(crate) behavior_fail: u32,
    /// From each `AltGr` press to the next left Ctrl + left Alt make, by injection QPC.
    altgr_windows: Vec<(i64, i64)>,
}

#[derive(Debug)]
pub(crate) struct Diagnostics {
    path: PathBuf,
    pub(crate) hover_injections: RefCell<Vec<i64>>,
    pub(crate) heard: RefCell<Vec<Heard>>,
}

impl Diagnostics {
    pub(crate) fn in_directory(directory: &Path) -> Self {
        Self {
            path: directory.join("milestones.log"),
            hover_injections: RefCell::default(),
            heard: RefCell::default(),
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

    pub(crate) fn feel_summary(&self) -> Result<String> {
        let text = fs::read_to_string(&self.path)?;
        ensure!(
            text.ends_with('\n'),
            "feel milestones are empty or truncated"
        );
        let records = text
            .lines()
            .map(Record::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(diagnostics::summarize_feel(&records))
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
        self.observe_within(session, since, Duration::from_secs(1), ready)
    }

    fn observe_within(
        &self,
        session: &Session,
        since: i64,
        limit: Duration,
        ready: impl Fn(&[Record]) -> bool,
    ) -> Result<Vec<Record>> {
        let until = Instant::now() + limit;
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
) -> Result<()> {
    let records = diagnostics.read()?;
    ensure!(!records.is_empty(), "no input diagnostics were flushed");
    let count = |mark: Mark| records.iter().filter(|record| record.mark == mark).count();
    if phase == Phase::Layouts {
        let hold = |recognized: bool| {
            records
                .iter()
                .filter(|record| matches!(record.mark, Mark::Hold { recognized: seen, .. } if seen == recognized))
                .count()
        };
        ensure!(
            hold(false) == 2
                && hold(true) == 2
                && !records
                    .iter()
                    .any(|record| matches!(record.mark, Mark::Opened(_))),
            "layouts needs exactly its two chords and two holds and no typing box"
        );
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

/// Waits up to 3 s for the brain and every worker of the launch `shell_pid` started, and holds
/// each with the exit code a graceful quit gives it.
pub(crate) fn owned_helpers(shell_pid: u32) -> Result<Vec<(&'static str, platform::Process, u32)>> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match product_tree(shell_pid, &platform::processes()?, platform::helper_role)? {
            Startup::Complete(tree) => {
                return tree[1..]
                    .iter()
                    .zip(&ROLES[1..])
                    .map(|(&(role, pid), &(_, code))| {
                        Ok((role, platform::Process::open(pid)?, code))
                    })
                    .collect();
            }
            Startup::Missing(role) => ensure!(
                Instant::now() < until,
                "the launch's {role} didn't start within 3s"
            ),
        }
        sleep(Duration::from_millis(10));
    }
}

pub(crate) fn discover(child: &mut Child, kind: WindowKind) -> Result<Window> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        let found = native::owned_window(child, kind);
        if found.is_ok() {
            return found;
        }
        if let Some(status) = child.try_wait()? {
            let code = native::FIXTURE_PLACEMENT_REFUSED;
            ensure!(
                !matches!(kind, WindowKind::Fixture) || status.code() != Some(code),
                "fixture startup refused (exit code {code}): no verified frame clear of the cursor"
            );
            return found;
        }
        if Instant::now() >= end {
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

/// How long a terminated owned process gets to be seen exiting.
const REAP_LIMIT: Duration = Duration::from_secs(1);

/// Ends an owned child within `grace` plus [`REAP_LIMIT`], whatever the session deadline: it may
/// exit on its own during `grace`, and is then terminated through its handle.
pub(crate) fn end_child(child: &mut Child, grace: Duration) -> Result<()> {
    if let Some(status) = crate::live::wait_bounded(child, grace)? {
        println!("owned pid {} exit={status}", child.id());
        return Ok(());
    }
    child.kill()?;
    let status = crate::live::wait_bounded(child, REAP_LIMIT)?
        .context("the terminated owned process didn't exit")?;
    println!("owned pid {} terminated exit={status}", child.id());
    Ok(())
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
            Phase::Hover => hover_panel(session, counts, context, diagnostics),
            Phase::Focus => focus(session, counts, diagnostics),
            Phase::Layouts => layouts(session, counts, diagnostics),
            Phase::Listening(_) => listening(session, counts, diagnostics),
            Phase::Esc => exercise_escape(contested, session, counts, diagnostics, context),
        }
    })();
    let release = session.release_injected();
    println!(
        "SendInput accepted_batches={} first_qpc={:?} last_qpc={:?} qpc_frequency={}; injection timestamps only, NOT native-callback-to-present latency",
        session.batches, session.first_qpc, session.last_qpc, session.qpc_frequency
    );
    combine([result, release])
}

fn hover_panel(
    session: &mut Session,
    counts: &mut Counts,
    context: &PhaseContext<'_>,
    diagnostics: &Diagnostics,
) -> Result<()> {
    let [enter, leave] = session.hover_points(context.dock, context.overlay)?;
    for index in 0..100 {
        session.wait(Duration::from_micros(1000 + (index * 997) % 6944))?;
        counts.attempts += 1;
        session.move_pointer(enter, Target::Product)?;
        let enter_qpc = session.last_qpc;
        diagnostics
            .hover_injections
            .borrow_mut()
            .push(enter_qpc.context("enter SendInput QPC missing")?);
        session.wait(Duration::from_millis(100))?;
        let entered = session.panel_visible()?;
        counts.behavior_fail += u32::from(!entered);
        ensure!(
            entered,
            "pointer entry did not make an owned panel HWND visible"
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
        ensure!(left, "pointer leave did not hide the owned panel HWND");
        counts.sent += 1;
        counts.behavior_pass += 1;
        println!(
            "hover attempt={} injection_enter_qpc={enter_qpc:?} injection_leave_qpc={leave_qpc:?} observed_hwnd_leave_ms={:.3}; NOT entry/present latency",
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
        || !session
            .typing_window()
            .and_then(Window::is_visible)
            .unwrap_or(false)
    {
        counts.behavior_fail += 1;
        bail!("completed short double tap did not show and foreground typing; no text injected");
    }
    session.tap(0x1e)?;
    session.tap(0x01)?;
    diagnostics.observe(session, since, |records| {
        [Mark::Dismissed, Mark::EscapeDismiss]
            .iter()
            .all(|mark| records.iter().any(|record| record.mark == *mark))
    })?;
    if !session.fixture.is_foreground() {
        counts.behavior_fail += 1;
        bail!("Escape did not return foreground to the fixture");
    }
    for switch_away in [true, false] {
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
            || !session
                .typing_window()
                .and_then(Window::is_visible)
                .unwrap_or(false)
            || !reopened.iter().any(|record| {
                record.mark
                    == (Mark::Draft {
                        nonempty: true,
                        preserved: true,
                    })
            })
        {
            counts.behavior_fail += 1;
            bail!("draft reopen was hidden, unfocused, empty or not preserved");
        }
        if switch_away {
            switch_from_typing(session, diagnostics)?;
        } else {
            session.tap(0x01)?;
            diagnostics.observe(session, reopened_since, |records| {
                records.iter().any(|record| record.mark == Mark::Dismissed)
                    && records
                        .iter()
                        .any(|record| record.mark == Mark::EscapeDismiss)
            })?;
        }
        ensure!(
            session.fixture_foreground(),
            "typing dismissal did not leave the fixture in front"
        );
    }
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

fn switch_from_typing(session: &mut Session, diagnostics: &Diagnostics) -> Result<()> {
    let typing = session.typing_window()?;
    session.wait(Duration::from_millis(325))?;
    let since = short_chord(session)?;
    session.wait(Duration::from_millis(50))?;
    short_chord(session)?;
    diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| record.mark == Mark::Opened(true))
    })?;
    ensure!(
        typing.is_visible()? && typing.is_foreground_window(),
        "repeated open hid or unfocused typing"
    );
    let alternate = session.switch_window()?;
    let checked = (|| {
        let rect = alternate.rect()?;
        let point = POINT {
            x: rect.left.midpoint(rect.right),
            y: rect.top.midpoint(rect.bottom),
        };
        session.move_pointer(point, Target::Fixture)?;
        session.click(point, Target::Fixture)?;
        let since = session
            .last_qpc
            .context("alternate fixture click was not sent")?;
        diagnostics.observe(session, since, |records| {
            records.iter().any(|record| record.mark == Mark::Dismissed)
        })?;
        session.wait(Duration::from_millis(300))?;
        ensure!(
            !typing.is_visible()? && alternate.is_foreground_window(),
            "typing did not hide or restored the previous foreground after switching away"
        );
        let point = session.edit_point()?;
        session.move_pointer(point, Target::Edit)?;
        session.click(point, Target::Edit)?;
        let until = Instant::now() + Duration::from_millis(300);
        while !session.fixture_foreground() && Instant::now() < until {
            session.wait(Duration::from_millis(5))?;
        }
        ensure!(
            session.fixture_foreground(),
            "return click did not foreground the original fixture"
        );
        Ok(())
    })();
    combine([checked, alternate.close()])
}

/// Holds left Ctrl + left Alt until the hold is recognised, then releases. Returns the make's
/// injection QPC and the hold's records.
fn positive_hold(session: &mut Session, diagnostics: &Diagnostics) -> Result<(i64, Vec<Record>)> {
    session.modifier(0x1d, InputEdge::Down)?;
    session.modifier(0x38, InputEdge::Down)?;
    let since = session.last_qpc.context("hold make QPC missing")?;
    let held = diagnostics.observe(session, since, |records| {
        records.iter().any(|record| {
            matches!(
                record.mark,
                Mark::Hold {
                    recognized: true,
                    callback_qpc,
                } if callback_qpc >= since
            )
        })
    });
    session.modifier(0x38, InputEdge::Up)?;
    session.modifier(0x1d, InputEdge::Up)?;
    Ok((since, held?))
}

/// Whether a record is the shortcut firing: a chord, a recognised hold or an opened typing box.
fn shortcut(record: &Record) -> Option<i64> {
    match record.mark {
        Mark::Hold { callback_qpc, .. } => Some(callback_qpc),
        Mark::Opened(_) => Some(record.qpc),
        _ => None,
    }
}

/// `AltGr` typing under German and Spanish in the fixture: each layout types its `@` and `€`
/// with no chord, hold or typing box between the `AltGr` press and the next left-modifier make,
/// then one left Ctrl + left Alt hold is recognised.
fn layouts(session: &mut Session, counts: &mut Counts, diagnostics: &Diagnostics) -> Result<()> {
    let layouts = Layouts::capture(session);
    let exercise = (|| {
        for selection in [Layout::German, Layout::Spanish] {
            counts.attempts += 1;
            Layouts::load(session, selection)?;
            session.clear_fixture()?;
            session.modifier(0x138, InputEdge::Down)?;
            let altgr = session.last_qpc.context("AltGr injection QPC missing")?;
            session.tap(match selection {
                Layout::German => 0x10,
                Layout::Spanish => 0x03,
            })?;
            session.tap(0x12)?;
            session.modifier(0x138, InputEdge::Up)?;
            counts.sent += 1;
            session.wait(Duration::from_millis(100))?;
            let typed = session.expected_layout_text()?;
            counts.behavior_pass += u32::from(typed);
            counts.behavior_fail += u32::from(!typed);
            counts.attempts += 1;
            let (since, _) = positive_hold(session, diagnostics)?;
            counts.altgr_windows.push((altgr, since));
            counts.sent += 1;
            counts.behavior_pass += 1;
            session.wait(Duration::from_millis(300))?;
        }
        let records = diagnostics.read()?;
        for &(start, end) in &counts.altgr_windows {
            let quiet = !records
                .iter()
                .filter_map(shortcut)
                .any(|qpc| (start..end).contains(&qpc));
            println!("altgr_only_window start_qpc={start} next_left_chord_qpc={end} quiet={quiet}");
            counts.behavior_pass += u32::from(quiet);
            counts.behavior_fail += u32::from(!quiet);
        }
        ensure!(
            counts.behavior_fail == 0,
            "AltGr typed the wrong characters or fired the shortcut"
        );
        Ok(())
    })();
    let release = session.release_injected();
    let restore = layouts.restore(session);
    combine([exercise, release, restore])
}

/// How many batches of [`BATCH_HOLDS`] the Listening row measures: each batch is its own launch,
/// so it stays inside the shell's two-minute quit bound and one main trace.
pub(crate) const BATCHES: u8 = 10;
/// How many holds each Listening batch measures.
pub(crate) const BATCH_HOLDS: u32 = 10;

/// One hold's records since its make: its chord, its recognition and the line taking the
/// Listening colour, each no earlier than `since`.
fn heard(records: &[Record], since: i64) -> Option<Heard> {
    let hold = |recognized: bool| {
        records.iter().find_map(|record| match record.mark {
            Mark::Hold {
                recognized: seen,
                callback_qpc,
            } if seen == recognized && callback_qpc >= since => Some(callback_qpc),
            _ => None,
        })
    };
    let listening = records
        .iter()
        .find(|record| record.mark == Mark::Listening(true))?
        .qpc;
    let (key_down, recognized) = (hold(false)?, hold(true)?);
    (key_down <= recognized && recognized <= listening).then_some(Heard {
        key_down,
        recognized,
        listening,
        resting: i64::MAX,
    })
}

/// How long a Listening batch waits, before its trace starts, for the brain to load speech input.
pub(crate) const SPEECH_WAIT: Duration = Duration::from_secs(20);

/// Waits until the brain reports speech input loaded and ready.
pub(crate) fn await_speech(session: &Session, diagnostics: &Diagnostics) -> Result<()> {
    diagnostics
        .observe_within(session, 0, SPEECH_WAIT, |records| {
            records
                .iter()
                .any(|record| record.mark == Mark::SpeechReady(true))
        })
        .map(drop)
}

/// Makes [`BATCH_HOLDS`] holds of left Ctrl + left Alt, each kept down until the line shows
/// Listening and a little after, then released and left to finish before the next.
fn listening(session: &mut Session, counts: &mut Counts, diagnostics: &Diagnostics) -> Result<()> {
    for index in 0..BATCH_HOLDS {
        session.wait(Duration::from_micros(1000 + u64::from(index * 997 % 6944)))?;
        counts.attempts += 1;
        session.modifier(0x1d, InputEdge::Down)?;
        session.modifier(0x38, InputEdge::Down)?;
        let since = session.last_qpc.context("hold make QPC missing")?;
        let shown = diagnostics.observe_within(session, since, Duration::from_secs(3), |records| {
            records
                .iter()
                .any(|record| record.mark == Mark::Listening(true))
        });
        let kept =
            shown.and_then(|records| session.wait(Duration::from_millis(150)).map(|()| records));
        session.modifier(0x38, InputEdge::Up)?;
        let released = session.last_qpc;
        session.modifier(0x1d, InputEdge::Up)?;
        counts.sent += 1;
        let released = released.context("hold break QPC missing")?;
        let sample = kept.map(|records| heard(&records, since));
        let Ok(Some(mut sample)) = sample else {
            counts.behavior_fail += 1;
            println!(
                "hold attempt={} reached no complete Listening",
                counts.attempts
            );
            return sample.and_then(|_| {
                bail!("a hold's chord, recognition and Listening are missing or out of order")
            });
        };
        let ended =
            diagnostics.observe_within(session, released, Duration::from_secs(3), |records| {
                records
                    .iter()
                    .any(|record| record.mark == Mark::Listening(false))
            })?;
        sample.resting = ended
            .iter()
            .find(|record| record.mark == Mark::Listening(false))
            .context("Listening off vanished")?
            .qpc;
        diagnostics.heard.borrow_mut().push(sample);
        counts.behavior_pass += 1;
        session.wait(SETTLE)?;
    }
    Ok(())
}

/// The pause after a hold leaves Listening, so its silent audio is transcribed and dropped
/// before the next hold asks to be admitted [guess: unmeasured on Rez].
const SETTLE: Duration = Duration::from_millis(1500);
