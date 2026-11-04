//! One approved manual-feel/Ready handoff followed by bounded final-main measurements.
#![expect(
    clippy::print_stdout,
    reason = "gate output contains retained evidence paths and non-content measurements"
)]

mod capture;
mod cold;
mod observation;
mod present;
mod receipts;

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, DISPLAY_NAME, SHELL_EXE};
use windows::Win32::Foundation::{POINT, RECT};

use crate::platform::{
    self, gate_capture,
    input_session::{self as native, FixtureEscMode, Registry, Session, Window, WindowKind},
};
use crate::session::{self, Counts, Diagnostics, Mark, Phase, PhaseContext};
use capture::Capture;
use present::PresentRows;

const SESSION_LIMIT: Duration = Duration::from_secs(3600);
const FEEL_LIMIT: Duration = Duration::from_secs(90);
const PHASE_LIMIT: Duration = Duration::from_secs(90);
const INPUT_RESERVE: Duration = Duration::from_secs(13);
const FINAL_RESERVE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trial {
    Idle,
    Input(Phase, FixtureEscMode),
    ReceiptRest,
    StopButton,
}

// shortcut: the motion trials (three stretch CPU rows and the motion receipts) left with the stretch; C2a brings them back with the pull.
const TRIALS: [Trial; 9] = [
    Trial::Idle,
    Trial::Input(Phase::Keyboard, FixtureEscMode::Free),
    Trial::Input(Phase::Click, FixtureEscMode::Free),
    Trial::ReceiptRest,
    Trial::Input(Phase::Focus, FixtureEscMode::Free),
    Trial::Input(Phase::Layouts, FixtureEscMode::Free),
    Trial::Input(Phase::Esc, FixtureEscMode::Free),
    Trial::Input(Phase::Esc, FixtureEscMode::Reserved),
    Trial::StopButton,
];

impl Trial {
    fn trace_bound(self) -> Option<Duration> {
        let seconds = match self {
            Self::Input(Phase::Keyboard, _) => 50,
            Self::Input(Phase::Click, _) | Self::ReceiptRest => 30,
            Self::Input(Phase::Focus | Phase::Layouts, _) => 20,
            Self::Input(Phase::Esc, _) | Self::StopButton => 10,
            Self::Idle => return None,
        };
        Some(Duration::from_secs(seconds))
    }

    fn budget(self) -> Duration {
        if self == Self::Idle {
            Duration::from_secs(120)
        } else {
            PHASE_LIMIT
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Idle => "cpu-idle",
            Self::Input(Phase::Keyboard, _) => "keyboard",
            Self::Input(Phase::Click, _) => "click",
            Self::Input(Phase::Focus, _) => "focus-draft-escape",
            Self::Input(Phase::Layouts, _) => "layouts",
            Self::Input(Phase::Esc, FixtureEscMode::Free) => "escape-free",
            Self::Input(Phase::Esc, FixtureEscMode::Reserved) => "escape-contested",
            Self::ReceiptRest => "receipt-rest",
            Self::StopButton => "stop-button",
        }
    }

    fn cpu_limit(self) -> Option<f64> {
        match self {
            Self::Idle => Some(1.0),
            _ => None,
        }
    }

    fn contested(self) -> bool {
        matches!(
            self,
            Self::Input(Phase::Esc, FixtureEscMode::Reserved) | Self::StopButton
        )
    }
}

fn phase_deadline(now: Instant, session_end: Instant, budget: Duration) -> Result<Instant> {
    let end = now.checked_add(budget).context("phase deadline overflow")?;
    ensure!(
        end.checked_add(FINAL_RESERVE)
            .is_some_and(|cleanup| cleanup <= session_end),
        "insufficient session budget for a complete phase and final cleanup"
    );
    Ok(end)
}

fn evidence_directory(transcript: &Path) -> Result<PathBuf> {
    ensure!(
        transcript.is_absolute(),
        "use an absolute retained transcript path outside the repository"
    );
    let name = transcript
        .file_name()
        .and_then(|name| name.to_str())
        .context("transcript filename is not Unicode")?;
    let stem = name
        .strip_suffix("-session.txt")
        .context("transcript filename must end in -session.txt")?;
    ensure!(
        !stem.is_empty() && stem != "." && stem != "..",
        "invalid evidence directory name"
    );
    Ok(transcript.with_file_name(stem))
}

#[derive(Debug)]
struct Images {
    shell: PathBuf,
    brain: PathBuf,
    shell_sha256: String,
    brain_sha256: String,
}

impl Images {
    fn release(exe: &Path) -> Result<Self> {
        let release = exe
            .parent()
            .and_then(Path::parent)
            .context("xtask is not in a Cargo profile directory")?
            .join("release");
        let shell = release.join(SHELL_EXE);
        let brain = release.join(BRAIN_EXE);
        ensure!(
            shell.is_file() && brain.is_file(),
            "build the final-main release before the approved gate session"
        );
        let shell_sha256 = gate_capture::file_sha256(&shell)?;
        let brain_sha256 = gate_capture::file_sha256(&brain)?;
        Ok(Self {
            shell,
            brain,
            shell_sha256,
            brain_sha256,
        })
    }

    fn unchanged(&self) -> Result<()> {
        ensure!(
            gate_capture::file_sha256(&self.shell)? == self.shell_sha256
                && gate_capture::file_sha256(&self.brain)? == self.brain_sha256,
            "release binaries changed during the gate session"
        );
        Ok(())
    }
}

fn retained_output(mut args: impl Iterator<Item = String>) -> Result<PathBuf> {
    let transcript = PathBuf::from(
        args.next()
            .context("task needs an existing create-new redirected path-session.txt")?,
    );
    ensure!(
        args.next().is_none(),
        "task accepts only its transcript path"
    );
    gate_capture::require_transcript(&transcript)?;
    let output = evidence_directory(&transcript)?;
    fs::create_dir(&output)?;
    fs::write(
        output.join("transcript.txt"),
        format!(
            "path={}\nstdout_and_stderr_verified=true\ncreate_new_redirection_required=true\n",
            transcript.display()
        ),
    )?;
    Ok(output)
}

pub(crate) fn cold(args: impl Iterator<Item = String>) -> Result<()> {
    cold::run(args)
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let output = retained_output(args)?;
    ensure!(
        platform::processes()?.is_empty(),
        "product processes already exist; gate session refused"
    );
    let started = Instant::now();
    let end = started
        .checked_add(SESSION_LIMIT)
        .context("session deadline overflow")?;
    let exe = std::env::current_exe()?;
    let images = Images::release(&exe)?;
    println!("retained gate evidence: {}", output.display());
    fs::write(
        output.join("identity.txt"),
        format!(
            "driver_build_id={}\ndriver_sha256={}\nshell_sha256={}\nbrain_sha256={}\n",
            kumochi_protocol::BUILD_ID,
            gate_capture::file_sha256(&exe)?,
            images.shell_sha256,
            images.brain_sha256
        ),
    )?;
    fs::write(
        output.join("environment-before.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    fs::write(
        output.join("phases.tsv"),
        "phase\tstatus\twall_s\tattempts\taccepted_sequences\tbehavior_pass\tbehavior_fail\n",
    )?;
    let mut fixture = native::spawn_fixture(&exe, SESSION_LIMIT)?;
    let mut fixture_window = None;
    let mut session = None;
    let result = (|| {
        let window = session::discover(&mut fixture, WindowKind::Fixture)?;
        fixture_window = Some(window);
        session = Some(Session::new(window, end)?);
        let session = session.as_mut().context("persistent session missing")?;
        feel(session, &images, &output, end)?;
        let registry = Registry::snapshot();
        fs::write(output.join("registry-before.txt"), format!("{registry:#?}"))?;
        let measured = (|| {
            for (index, trial) in TRIALS.into_iter().enumerate() {
                session.guard()?;
                session.set_phase(u8::try_from(index + 1)?)?;
                run_trial(trial, session, &images, &output, end)?;
            }
            Ok(())
        })();
        let after = Registry::snapshot();
        let retained = fs::write(output.join("registry-after.txt"), format!("{after:#?}"))
            .map_err(anyhow::Error::from);
        let compared = registry.ensure_unchanged(&after);
        session::combine([measured, retained, compared])
    })();
    let released = if let Some(session) = session.as_mut() {
        let release = session.release_injected();
        let uncovered = session.uncover();
        let cursor = if result.is_ok() && release.is_ok() && uncovered.is_ok() {
            session
                .restore_cursor()
                .map(|status| println!("final_cursor_restore={status}"))
        } else {
            Ok(())
        };
        session::combine([release, uncovered, cursor])
    } else {
        Ok(())
    };
    let stop = session.as_ref().and_then(Session::stop_metadata);
    let guard_report = stop.map_or_else(String::new, |metadata| {
        let phase = metadata
            .phase
            .checked_sub(1)
            .and_then(|index| TRIALS.get(usize::from(index)))
            .map_or("feel-ready", |trial| trial.name());
        format!("guard_stop phase={phase} metadata={metadata:?}\n")
    });
    print!("{guard_report}");
    let closed = fixture_window.map_or(Ok(()), Window::close);
    let exited = session::wait_child(&mut fixture, end);
    let environment = gate_capture::desktop_metadata().and_then(|metadata| {
        fs::write(output.join("environment-after.txt"), metadata).map_err(anyhow::Error::from)
    });
    let final_result = session::combine([result, released, closed, exited, environment]);
    fs::write(
        output.join("session-result.txt"),
        format!(
            "runtime_s={:.3}\nautomation_result={final_result:?}\nhuman_feel_is_separate=true\nalt_tab=owner_manual_separate_verdict\nplacements=runtime_unmeasured\n{guard_report}",
            started.elapsed().as_secs_f64()
        ),
    )?;
    final_result
}

fn feel(session: &mut Session, images: &Images, output: &Path, end: Instant) -> Result<()> {
    println!(
        "Injection is disabled. The dock is a short line at the top center by default; Listening turns it the accent colour. Try hold and double tap. This test window is only for the two verdicts and Ready; explicitly choose each verdict (Unrated is valid). Press Alt-Tab once and separately report whether any {DISPLAY_NAME} window appears. Then physically click Ready and do not touch the mouse or keyboard until this window closes (about 10 minutes, at most one hour). The first quiet second only opens injection. Input before injection opens returns to Ready without extending the feel deadline. Every feel verdict, including Off, is independent of owner approval."
    );
    let feel_directory = output.join("feel");
    fs::create_dir(&feel_directory)?;
    let diagnostics = Diagnostics::in_directory(&feel_directory);
    let mut child = platform::launch(&images.shell, 120_000, [diagnostics.argument()])?;
    let mut window = None;
    let mut helpers = Vec::new();
    let body = (|| {
        let product = session::discover(&mut child, WindowKind::ProductProxy)?;
        window = Some(product);
        helpers = session::owned_helpers(child.id())?;
        session.bind_product(product, end)?;
        let ready_end = Instant::now()
            .checked_add(FEEL_LIMIT)
            .context("feel deadline overflow")?;
        let mut attempts = 0;
        let verdicts = loop {
            let verdicts = session.await_ready(ready_end)?;
            attempts += 1;
            println!("ready_attempt={attempts} feel={verdicts:?}");
            if session.await_quiet(ready_end)? {
                break verdicts;
            }
            println!(
                "ready_attempt={attempts} rearmed=true injection_opened=false; original deadline unchanged"
            );
        };
        fs::write(
            output.join("owner-feel.txt"),
            format!("{verdicts:?}\nindependent_human_gate=true\nnot_owner_approval=true\n"),
        )?;
        Ok(())
    })();
    session.unbind_product();
    let cleanup_end = Instant::now()
        .checked_add(Duration::from_secs(15))
        .context("feel cleanup deadline overflow")?
        .min(end);
    let stopped = session::stop_product(Some(&mut child), window, helpers, cleanup_end);
    let summary = diagnostics.feel_summary().unwrap_or_else(|error| {
        format!("summary=unmeasured\nerror={error:#}\nnot_an_automated_gate=true\n")
    });
    let summarized =
        fs::write(feel_directory.join("summary.txt"), summary).map_err(anyhow::Error::from);
    session::combine([body, stopped, summarized])
}

fn physical_rect([left, top, width, height]: [i32; 4]) -> Result<RECT> {
    ensure!(width > 0 && height > 0, "empty observed rectangle");
    Ok(RECT {
        left,
        top,
        right: left
            .checked_add(width)
            .context("observed rectangle overflow")?,
        bottom: top
            .checked_add(height)
            .context("observed rectangle overflow")?,
    })
}

fn await_startup(
    reader: &mut observation::Reader,
    session: &Session,
) -> Result<observation::Startup> {
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        session.guard()?;
        if let Some(startup) = reader.update()?.startup {
            return Ok(startup);
        }
        ensure!(
            Instant::now() < end,
            "observer startup acknowledgement missing"
        );
        session.wait(Duration::from_millis(1))?;
    }
}

fn completed_stop_target(observed: &observation::Observation) -> Option<observation::StopTarget> {
    let target = observed.stop_target?;
    let frame = observed
        .frames
        .iter()
        .rev()
        .find(|frame| frame.kind == observation::Kind::Panel)?;
    (frame.id == target.frame
        && frame.hover == target.hover
        && frame.completion_qpc.is_some_and(|end| end >= target.qpc))
    .then_some(target)
}

fn await_stop_target(
    reader: &mut observation::Reader,
    session: &Session,
) -> Result<observation::StopTarget> {
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        session.guard()?;
        let observed = reader.update()?;
        if let Some(target) = completed_stop_target(observed) {
            ensure!(
                session.panel_visible()?,
                "observed Stop panel is not visible"
            );
            return Ok(target);
        }
        ensure!(
            Instant::now() < end,
            "current completed Stop target missing or stale"
        );
        session.wait(Duration::from_millis(1))?;
    }
}

fn stop_button(
    session: &mut Session,
    reader: &mut observation::Reader,
    output: &Path,
    counts: &mut Counts,
    diagnostics: &Diagnostics,
) -> Result<()> {
    counts.attempts += 1;
    diagnostics.observe(session, 0, |records| {
        records.iter().any(|record| {
            record.mark
                == (Mark::EscapeState {
                    wanted: true,
                    stop: true,
                })
        })
    })?;
    let target = await_stop_target(reader, session)?;
    let bounds = physical_rect(target.rect)?;
    let point = POINT {
        x: bounds.left + (bounds.right - bounds.left) / 2,
        y: bounds.top + (bounds.bottom - bounds.top) / 2,
    };
    session.move_pointer(point, native::Target::Panel)?;
    session.wait(Duration::from_millis(2))?;
    let current = await_stop_target(reader, session)?;
    ensure!(
        current.rect == target.rect && current.hover == target.hover,
        "Stop rectangle or hover scope changed before click; no retry"
    );
    let clicked = session.click(point, native::Target::Panel);
    let recorded = fs::write(output.join("stop-click.txt"), format!("target_frame={}\ntarget_qpc={}\nhover={}\nrect={:?}\npoint={},{}\ninjection_begin_qpc={:?}\ninjection_end_qpc={:?}\naccepted={}\nresult={clicked:?}\nbackend_stop=unmeasured_idle_fixture\n", current.frame, current.qpc, current.hover, current.rect, point.x, point.y, session.last_qpc, session.last_end_qpc, session.last_accepted)).map_err(anyhow::Error::from);
    session::combine([clicked, recorded])?;
    counts.sent += 1;
    let since = session.last_qpc.context("Stop click QPC missing")?;
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
    ensure!(
        session.fixture.is_foreground(),
        "Stop click did not return foreground to the fixture"
    );
    session.set_escape(FixtureEscMode::Free)?;
    let (_, before) = session.fixture.counts()?;
    session.tap(0x01)?;
    counts.sent += 1;
    session.wait(Duration::from_millis(100))?;
    let (_, after) = session.fixture.counts()?;
    let passed = after == before + 1;
    fs::write(
        output.join("stop-escape.txt"),
        format!(
            "reservation_released_after_clear=true\nfixture_escapes_before={before}\nfixture_escapes_after={after}\nordinary_escape_pass={passed}\n"
        ),
    )?;
    counts.behavior_pass += u32::from(passed);
    counts.behavior_fail += u32::from(!passed);
    ensure!(
        passed,
        "ordinary Escape after Stop did not reach the fixture"
    );
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one owned product/trace lifecycle keeps input release and all cleanup/error aggregation together on every workload outcome"
)]
fn run_trial(
    trial: Trial,
    session: &mut Session,
    images: &Images,
    output: &Path,
    whole_end: Instant,
) -> Result<()> {
    images.unchanged()?;
    ensure!(
        platform::processes()?.is_empty(),
        "unexpected product process between phases"
    );
    let directory = output.join(trial.name());
    fs::create_dir(&directory)?;
    let calibration = if trial.cpu_limit().is_some() {
        Some(crate::bench::gate_calibrate(|| session.guard())?)
    } else {
        None
    };
    let started = Instant::now();
    let end = phase_deadline(started, whole_end, trial.budget())?;
    let input_end = end
        .checked_sub(INPUT_RESERVE)
        .context("phase has no input interval")?;
    let diagnostics = Diagnostics::in_directory(&directory);
    let observer = directory.join("observer.log");
    let mut options = Vec::new();
    if trial.cpu_limit().is_none() {
        options.push(diagnostics.argument());
        let mut argument = OsString::from("--gate-observer=");
        argument.push(&observer);
        options.push(argument);
    }
    if trial == Trial::ReceiptRest {
        options.push(OsString::from("--receipt-fixture"));
    }
    if matches!(trial, Trial::Input(Phase::Esc, _) | Trial::StopButton) {
        options.push(OsString::from("--esc-fixture"));
    }
    if trial.contested() {
        session.set_escape(FixtureEscMode::Reserved)?;
    }
    session.guard()?;
    let launched = platform::launch(
        &images.shell,
        u64::try_from(trial.budget().as_millis())?,
        options,
    );
    let mut child = match launched {
        Ok(child) => child,
        Err(error) => return Err(error.context("launching bounded gate product")),
    };
    let pid = child.id();
    let mut window = None;
    let mut helpers = Vec::new();
    let mut counts = Counts::default();
    let mut capture = None;
    let mut dock = 0;
    let body = (|| {
        let product = session::discover(&mut child, WindowKind::ProductProxy)?;
        window = Some(product);
        helpers = session::owned_helpers(pid)?;
        session.bind_product(product, input_end)?;
        fs::write(
            directory.join("product.txt"),
            format!(
                "pid={pid}\nshell_sha256={}\nbrain_sha256={}\n",
                images.shell_sha256, images.brain_sha256
            ),
        )?;
        if let Some(limit) = trial.cpu_limit() {
            let rate = calibration.context("CPU calibration missing")?;
            let sample = if trial == Trial::Idle {
                Duration::from_secs(60)
            } else {
                Duration::from_secs(30)
            };
            let measured =
                crate::bench::gate_cpu(pid, &directory, rate, sample, || session.guard())?;
            fs::write(
                directory.join("cpu.txt"),
                format!(
                    "rate={rate}\ncycles_percent={}\ntime_percent={}\nprivate_bytes={}\nlimit_percent={limit}\n",
                    measured.cycles_percent, measured.time_percent, measured.private_bytes
                ),
            )?;
            let passed = measured.cycles_percent <= limit;
            counts.behavior_pass += u32::from(passed);
            counts.behavior_fail += u32::from(!passed);
            ensure!(passed, "calibrated CPU exceeds the phase limit");
            return Ok(());
        }
        diagnostics.await_started(session)?;
        let mut reader = observation::Reader::open(&observer, pid)?;
        let startup = await_startup(&mut reader, session)?;
        let trace = Capture::warm_and_start(
            pid,
            &directory.join("presents.csv"),
            trial
                .trace_bound()
                .context("CPU phase cannot start an input trace")?,
            || product.request_gate_repaint(),
            || session.guard(),
        )?;
        dock = trace.dock_swapchain;
        let trace_input_end = input_end.min(trace.input_deadline());
        capture = Some(trace);
        session.bind_product(product, trace_input_end)?;
        session.bind_panel(startup.panel_hwnd)?;
        let context = PhaseContext {
            dock: physical_rect(startup.dock)?,
            overlay: physical_rect(startup.overlay)?,
            observer: &observer,
        };
        match trial {
            Trial::Input(phase, mode) => {
                session::exercise(phase, mode, session, &mut counts, &diagnostics, &context)
            }
            Trial::StopButton => {
                stop_button(session, &mut reader, &directory, &mut counts, &diagnostics)
            }
            Trial::ReceiptRest => receipts::exercise(
                session,
                &observer,
                &directory,
                &mut counts,
                receipts::Mode::Rest,
            ),
            Trial::Idle => bail!("CPU trial reached input dispatch"),
        }
    })();
    let release = session.release_injected();
    let uncovered = session.uncover();
    let rows = capture
        .as_mut()
        .map(|capture| capture.finish(|| session.guard()));
    session.unbind_product();
    let stopped = session::stop_product(Some(&mut child), window, helpers, end);
    let escape = if trial.contested() {
        session.set_escape(FixtureEscMode::Free)
    } else {
        Ok(())
    };
    let flushed = if let Trial::Input(phase, mode) = trial {
        session::check_flushed(phase, mode, &diagnostics, &mut counts)
    } else {
        Ok(())
    };
    let summarized = summarize(trial, &observer, pid, rows, dock, &directory, &diagnostics);
    let result = session::combine([
        body, release, uncovered, stopped, escape, flushed, summarized,
    ]);
    let status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let recorded = (|| {
        let mut report = OpenOptions::new()
            .append(true)
            .open(output.join("phases.tsv"))?;
        writeln!(
            report,
            "{}\t{status}\t{:.3}\t{}\t{}\t{}\t{}",
            trial.name(),
            started.elapsed().as_secs_f64(),
            counts.attempts,
            counts.sent,
            counts.behavior_pass,
            counts.behavior_fail
        )?;
        fs::write(directory.join("result.txt"), format!("{result:?}\n"))?;
        Ok(())
    })();
    session::combine([result, recorded])
}

fn summarize(
    trial: Trial,
    observer: &Path,
    pid: u32,
    rows: Option<Result<PresentRows>>,
    dock: u64,
    output: &Path,
    diagnostics: &Diagnostics,
) -> Result<()> {
    if trial.cpu_limit().is_some() {
        return Ok(());
    }
    let read = observation::read(observer, pid).map(|_| ());
    let joined = (|| {
        let mut rows = rows.context("no main trace completed")??;
        match trial {
            Trial::Input(Phase::Keyboard, _) => observation::summarize_keyboard(
                &diagnostics.canvas_pairs()?,
                observer,
                pid,
                &mut rows,
                dock,
                output,
            ),
            Trial::Input(Phase::Click, _) => observation::summarize_click(
                observer,
                pid,
                &mut rows,
                dock,
                output,
                &diagnostics.click_injections.borrow(),
            ),
            Trial::ReceiptRest => {
                receipts::summarize(observer, pid, &mut rows, dock, output, receipts::Mode::Rest)
            }
            _ => Ok(()),
        }
    })();
    session::combine([read, joined])
}

#[cfg(test)]
mod tests {
    use super::{TRIALS, Trial, evidence_directory, phase_deadline};
    use crate::platform::input_session::FixtureEscMode;
    use crate::session::Phase;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[test]
    fn transcript_and_evidence_are_adjacent_without_overwriting_the_transcript() {
        assert_eq!(
            evidence_directory(Path::new("C:/gate/run-session.txt")).unwrap(),
            PathBuf::from("C:/gate/run")
        );
        for path in ["run-session.txt", "C:/gate/run.txt", "C:/gate/-session.txt"] {
            assert!(
                evidence_directory(Path::new(path)).is_err(),
                "invalid retained transcript location"
            );
        }
    }

    #[test]
    fn physical_rectangles_preserve_signed_origins_and_reject_overflow() {
        let rect = super::physical_rect([-20, -10, 30, 40]).unwrap();
        assert_eq!(
            [rect.left, rect.top, rect.right, rect.bottom],
            [-20, -10, 10, 30]
        );
        assert!(super::physical_rect([i32::MAX, 0, 1, 1]).is_err());
        assert!(super::physical_rect([0, 0, 0, 1]).is_err());
    }

    #[test]
    fn stop_target_requires_the_current_completed_panel_and_hover() {
        use super::observation::{Frame, Kind, Observation, StopTarget};
        let mut observed = Observation::default();
        observed.stop_target = Some(StopTarget {
            frame: 1,
            qpc: 12,
            hover: 0,
            rect: [0, 0, 10, 10],
        });
        observed.frames.push(Frame {
            id: 1,
            paint_qpc: 10,
            completion_qpc: None,
            kind: Kind::Panel,
            hover: 0,
            geometry: 0,
        });
        assert!(super::completed_stop_target(&observed).is_none());
        observed.frames[0].completion_qpc = Some(13);
        assert_eq!(
            super::completed_stop_target(&observed).map(|target| target.frame),
            Some(1)
        );
        observed.frames[0].hover = 1;
        assert!(super::completed_stop_target(&observed).is_none());
        observed.frames[0].hover = 0;
        observed.frames.push(Frame {
            id: 2,
            paint_qpc: 14,
            completion_qpc: Some(15),
            kind: Kind::Panel,
            hover: 0,
            geometry: 0,
        });
        assert!(super::completed_stop_target(&observed).is_none());
    }

    #[test]
    fn phase_never_spends_final_cleanup_reserve() {
        let now = Instant::now();
        assert_eq!(
            phase_deadline(
                now,
                now + Duration::from_secs(120),
                Trial::ReceiptRest.budget()
            )
            .unwrap(),
            now + Duration::from_secs(90)
        );
        assert!(
            phase_deadline(
                now,
                now + Duration::from_millis(119_999),
                Trial::ReceiptRest.budget()
            )
            .is_err()
        );
        assert_eq!(
            phase_deadline(now, now + Duration::from_secs(150), Trial::Idle.budget()).unwrap(),
            now + Duration::from_secs(120)
        );
        assert!(
            phase_deadline(
                now,
                now + Duration::from_millis(149_999),
                Trial::Idle.budget()
            )
            .is_err()
        );
    }

    #[test]
    fn receipts_follow_click_and_esc_conflict_is_last() {
        let hover = TRIALS
            .iter()
            .position(|trial| *trial == Trial::Input(Phase::Click, FixtureEscMode::Free))
            .unwrap();
        assert_eq!(TRIALS[hover + 1], Trial::ReceiptRest);
        assert_eq!(
            &TRIALS[7..],
            &[
                Trial::Input(Phase::Esc, FixtureEscMode::Reserved),
                Trial::StopButton
            ]
        );
        assert_eq!(
            TRIALS
                .iter()
                .take_while(|trial| trial.cpu_limit().is_some())
                .count(),
            1
        );
    }
}
