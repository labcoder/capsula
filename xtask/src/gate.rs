//! One approved manual-feel/Ready handoff followed by bounded final-main measurements.
#![expect(
    clippy::print_stdout,
    reason = "gate output contains retained evidence paths and non-content measurements"
)]

mod capture;
mod cold;
mod draw_timing;
mod observation;
mod present;
mod receipts;

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, DATA_DIR, DISPLAY_NAME, SHELL_EXE};
use windows::Win32::Foundation::{POINT, RECT};

use crate::hermetic::{self, Folders, Prebuilt, Profile, Run};
use crate::platform::{
    self, gate_capture,
    input_session::{
        self as native, FixtureEscMode, FixturePlacement, Registry, Session, Window, WindowKind,
    },
};
use crate::session::{self, Counts, Diagnostics, Mark, Phase, PhaseContext};
use capture::Capture;
use present::PresentRows;

const SESSION_LIMIT: Duration = Duration::from_secs(3600);
const FEEL_LIMIT: Duration = Duration::from_secs(90);
const PHASE_LIMIT: Duration = Duration::from_secs(90);
const INPUT_RESERVE: Duration = Duration::from_secs(13);
const FINAL_RESERVE: Duration = Duration::from_secs(30);
const CLOSE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trial {
    Idle,
    Input(Phase, FixtureEscMode),
    ReceiptRest,
    StopButton,
}

const TRIALS: [Trial; 8] = [
    Trial::Idle,
    Trial::ReceiptRest,
    Trial::Input(Phase::Hover, FixtureEscMode::Free),
    Trial::Input(Phase::Focus, FixtureEscMode::Free),
    Trial::Input(Phase::Layouts, FixtureEscMode::Free),
    Trial::Input(Phase::Esc, FixtureEscMode::Free),
    Trial::Input(Phase::Esc, FixtureEscMode::Reserved),
    Trial::StopButton,
];

/// The Listening batches' evidence folders, by batch number from 1.
const LISTENING: [&str; 11] = [
    "listening-unknown",
    "listening-01",
    "listening-02",
    "listening-03",
    "listening-04",
    "listening-05",
    "listening-06",
    "listening-07",
    "listening-08",
    "listening-09",
    "listening-10",
];

impl Trial {
    fn trace_bound(self) -> Option<Duration> {
        let seconds = match self {
            Self::Input(Phase::Hover, _) | Self::ReceiptRest => 30,
            Self::Input(Phase::Focus | Phase::Layouts, _) => 20,
            Self::Input(Phase::Listening(_), _) => 40,
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
            Self::Input(Phase::Hover, _) => "hover",
            Self::Input(Phase::Focus, _) => "focus-draft-escape",
            Self::Input(Phase::Layouts, _) => "layouts",
            Self::Input(Phase::Listening(batch), _) => LISTENING
                .get(usize::from(batch))
                .copied()
                .unwrap_or("listening-unknown"),
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

/// The gate release images and the folders every launch of them uses.
#[derive(Debug)]
struct Images {
    shell: PathBuf,
    brain: PathBuf,
    shell_sha256: String,
    brain_sha256: String,
    folders: Folders,
    /// How long building them took inside the run.
    build: Duration,
    /// Where the Listening phase copies speech input from, when the run measures it.
    speech: Option<Speech>,
}

/// An installed speech-input model folder and the native runtime, both already on this machine.
#[derive(Debug, PartialEq, Eq)]
struct Speech {
    models: PathBuf,
    natives: PathBuf,
}

impl Images {
    fn build(run: &Run) -> Result<Self> {
        let (release, build) =
            hermetic::build(&["kumochi-shell", "kumochi-brain"], Profile::Release, None)?;
        let shell = release.join(SHELL_EXE);
        let brain = release.join(BRAIN_EXE);
        let shell_sha256 = gate_capture::file_sha256(&shell)?;
        let brain_sha256 = gate_capture::file_sha256(&brain)?;
        Ok(Self {
            shell,
            brain,
            shell_sha256,
            brain_sha256,
            folders: run.folders("app")?,
            build,
            speech: None,
        })
    }

    fn prepared(run: &Run, release: &Path, pins: Prebuilt) -> Result<Self> {
        let folders = run.folders("app")?;
        let bin = pins.stage(release, &folders)?;
        Ok(Self {
            shell: bin.join(SHELL_EXE),
            brain: bin.join(BRAIN_EXE),
            shell_sha256: pins.shell_sha256,
            brain_sha256: pins.brain_sha256,
            folders,
            build: Duration::ZERO,
            speech: None,
        })
    }

    /// The run's identity record: the driver, the images and the identity they were built with.
    fn identity(&self, exe: &Path) -> Result<String> {
        Ok(format!(
            "driver_build_id={}\nidentity={DATA_DIR}\ndriver_sha256={}\nshell_sha256={}\nbrain_sha256={}\nbuild_seconds={:.1}\n",
            kumochi_protocol::BUILD_ID,
            gate_capture::file_sha256(exe)?,
            self.shell_sha256,
            self.brain_sha256,
            self.build.as_secs_f64()
        ))
    }

    /// Moves the launches onto verified copies of the images in the run's own `bin`, so the
    /// native runtime copied beside the brain stays inside the run.
    fn stage(&mut self) -> Result<()> {
        let release = self
            .brain
            .parent()
            .context("the brain image has a folder")?
            .to_path_buf();
        let pins = Prebuilt {
            shell_sha256: self.shell_sha256.clone(),
            brain_sha256: self.brain_sha256.clone(),
        };
        let bin = pins.stage(&release, &self.folders)?;
        self.shell = bin.join(SHELL_EXE);
        self.brain = bin.join(BRAIN_EXE);
        Ok(())
    }

    /// Copies speech input into the launch's own models folder and the native runtime beside the
    /// brain, verified against the manifest, so the next launch can hear. Never downloads.
    fn install_speech(&self) -> Result<()> {
        let speech = self
            .speech
            .as_ref()
            .context("the Listening phase needs hold-gate --speech")?;
        let bin = self
            .brain
            .parent()
            .context("the brain image has a folder")?;
        crate::voice_assets::place_speech_input(
            &speech.models,
            &speech.natives,
            &self.folders.models(),
            bin,
        )?;
        self.folders.install_speech_input()
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

/// The transcript and its evidence folder, which must not exist yet; the gate run creates it.
fn retained_output(mut args: impl Iterator<Item = String>) -> Result<(PathBuf, PathBuf)> {
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
    ensure!(
        !output.try_exists()?,
        "{} already exists; use a new transcript name",
        output.display()
    );
    Ok((transcript, output))
}

/// Begins the gate run in a new evidence folder for the transcript, records the transcript there,
/// runs `body` with the folder, and ends the run.
fn retained_run(
    args: impl Iterator<Item = String>,
    finish: fn(Run, Result<()>) -> Result<()>,
    body: impl FnOnce(&Path, &Run) -> Result<()>,
) -> Result<()> {
    let (transcript, output) = retained_output(args)?;
    let run = Run::begin("gate")?;
    let result = fs::create_dir(&output)
        .and_then(|()| {
            fs::write(
                output.join("transcript.txt"),
                format!(
                    "path={}\nstdout_and_stderr_verified=true\ncreate_new_redirection_required=true\n",
                    transcript.display()
                ),
            )
        })
        .map_err(anyhow::Error::from)
        .and_then(|()| body(&output, &run));
    finish(run, result)
}

pub(crate) fn cold(args: impl Iterator<Item = String>) -> Result<()> {
    cold::run(args)
}

pub(crate) fn draw_timing(args: impl Iterator<Item = String>) -> Result<()> {
    draw_timing::run(args)
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    session_run(
        args,
        ("feel-ready", feel, FixturePlacement::Default),
        &TRIALS,
        (None, None),
    )
}

/// The hover phase alone, opened by the owner's away rule instead of a Ready press.
pub(crate) fn hover(args: impl Iterator<Item = String>) -> Result<()> {
    session_run(
        args,
        ("idle-open", idle_open, FixturePlacement::AwayFromCursor),
        &[Trial::Input(Phase::Hover, FixtureEscMode::Free)],
        (None, None),
    )
}

const HOLD_USAGE: &str = "hold-gate needs <path-session.txt> [--prebuilt <shell-sha256> <brain-sha256>] [--speech <models-dir> <natives-dir>]";

/// The hold to talk rows alone, opened by the owner's away rule: `AltGr` typing under two layouts,
/// then, with `--speech`, hold recognised to Listening on screen with the microphone open.
pub(crate) fn hold(mut args: impl Iterator<Item = String>) -> Result<()> {
    let transcript = args.next().context(HOLD_USAGE)?;
    let (pins, speech) = hold_options(args)?;
    let prepared = pins
        .map(|pins| pins.release().map(|release| (pins, release)))
        .transpose()?;
    let mut trials = vec![Trial::Input(Phase::Layouts, FixtureEscMode::Free)];
    if speech.is_some() {
        trials.extend(
            (1..=session::BATCHES)
                .map(|batch| Trial::Input(Phase::Listening(batch), FixtureEscMode::Free)),
        );
    }
    session_run(
        std::iter::once(transcript),
        ("idle-open", idle_open, FixturePlacement::AwayFromCursor),
        &trials,
        (prepared, speech),
    )
}

/// `hold-gate`'s options after its transcript: optional prebuilt pins, then an optional speech
/// source, in that order.
fn hold_options(
    mut args: impl Iterator<Item = String>,
) -> Result<(Option<Prebuilt>, Option<Speech>)> {
    let mut next = args.next();
    let pins = if next.as_deref() == Some("--prebuilt") {
        let pins = Prebuilt::parse(&mut args)?;
        next = args.next();
        Some(pins)
    } else {
        None
    };
    let speech = match (next.as_deref(), args.next(), args.next(), args.next()) {
        (None, None, ..) => None,
        (Some("--speech"), Some(models), Some(natives), None) => Some(Speech {
            models: PathBuf::from(models),
            natives: PathBuf::from(natives),
        }),
        _ => bail!(HOLD_USAGE),
    };
    Ok((pins, speech))
}

type Opener = fn(&mut Session, &Images, &Path, Instant) -> Result<()>;

fn session_run(
    args: impl Iterator<Item = String>,
    opener: (&str, Opener, FixturePlacement),
    trials: &[Trial],
    (prepared, speech): (Option<(Prebuilt, PathBuf)>, Option<Speech>),
) -> Result<()> {
    retained_run(args, Run::finish, |output, run| {
        let images = match prepared {
            Some((pins, release)) => Images::prepared(run, &release, pins),
            None => Images::build(run).and_then(|mut images| {
                if speech.is_some() {
                    images.stage()?;
                }
                Ok(images)
            }),
        };
        let result = images.and_then(|mut images| {
            images.speech = speech;
            run_session(output, &images, opener, trials)
        });
        let cursor = native::cursor_position();
        println!(
            "final_cursor={}",
            cursor.map_or_else(|_| "unread".to_owned(), |at| format!("{},{}", at.x, at.y))
        );
        result
    })
}

fn run_session(
    output: &Path,
    images: &Images,
    (opener, open, placement): (&str, Opener, FixturePlacement),
    trials: &[Trial],
) -> Result<()> {
    let started = Instant::now();
    let end = started
        .checked_add(SESSION_LIMIT)
        .context("session deadline overflow")?;
    let exe = std::env::current_exe()?;
    println!("retained gate evidence: {}", output.display());
    fs::write(output.join("identity.txt"), images.identity(&exe)?)?;
    fs::write(
        output.join("environment-before.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    fs::write(
        output.join("phases.tsv"),
        "phase\tstatus\twall_s\tattempts\taccepted_sequences\tbehavior_pass\tbehavior_fail\n",
    )?;
    let mut fixture = native::spawn_fixture(&exe, SESSION_LIMIT, placement)?;
    let mut fixture_window = None;
    let mut session = None;
    let result = (|| {
        let window = session::discover(&mut fixture, WindowKind::Fixture)?;
        fixture_window = Some(window);
        session = Some(Session::new(window, end)?);
        let session = session.as_mut().context("persistent session missing")?;
        open(session, images, output, end)?;
        let registry = Registry::snapshot();
        fs::write(output.join("registry-before.txt"), format!("{registry:#?}"))?;
        let mut presented = Vec::new();
        let measured = (|| {
            for (index, &trial) in trials.iter().enumerate() {
                session.guard()?;
                session.set_phase(u8::try_from(index + 1)?)?;
                run_trial(trial, session, images, output, end, &mut presented)?;
            }
            if trials
                .iter()
                .any(|trial| matches!(trial, Trial::Input(Phase::Listening(_), _)))
            {
                observation::summarize_listening(
                    &presented,
                    usize::from(crate::session::BATCHES)
                        * usize::try_from(crate::session::BATCH_HOLDS)?,
                    output,
                )?;
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
            session.restore_cursor()
        } else {
            Ok("skipped_session_or_cleanup_failed")
        };
        println!(
            "final_cursor_restore={} pointer_left={}",
            cursor.as_ref().copied().unwrap_or("failed"),
            session.pointer_left()
        );
        let cursor = cursor.map(|_| ());
        session::combine([release, uncovered, cursor])
    } else {
        Ok(())
    };
    let stop = session.as_ref().and_then(Session::stop_metadata);
    let guard_report = stop.map_or_else(String::new, |metadata| {
        let phase = metadata
            .phase
            .checked_sub(1)
            .and_then(|index| trials.get(usize::from(index)))
            .map_or(opener, |trial| trial.name());
        format!("guard_stop phase={phase} metadata={metadata:?}\n")
    });
    print!("{guard_report}");
    let clean = (result.is_ok() && released.is_ok()).then_some(end);
    let exited = end_fixture(&mut fixture, fixture_window, clean);
    let environment = gate_capture::desktop_metadata().and_then(|metadata| {
        fs::write(output.join("environment-after.txt"), metadata).map_err(anyhow::Error::from)
    });
    let final_result = session::combine([result, released, exited, environment]);
    fs::write(
        output.join("session-result.txt"),
        format!(
            "opener={opener}\nruntime_s={:.3}\nautomation_result={final_result:?}\nhuman_feel_is_separate=true\nalt_tab=owner_manual_separate_verdict\nplacements=runtime_unmeasured\n{guard_report}",
            started.elapsed().as_secs_f64()
        ),
    )?;
    final_result
}

/// Ends the fixture. After a clean session, given its deadline, it closes the window and waits for
/// the fixture until that deadline. After any failure a known window gets [`CLOSE_GRACE`] to close
/// and the fixture is then terminated, so a fixture born topmost never outlives a failed session
/// for long.
fn end_fixture(fixture: &mut Child, window: Option<Window>, clean: Option<Instant>) -> Result<()> {
    let closed = window.map_or(Ok(()), Window::close);
    let exited = if let Some(end) = clean.filter(|_| closed.is_ok()) {
        session::wait_child(fixture, end)
    } else {
        let grace = if window.is_some() {
            CLOSE_GRACE
        } else {
            Duration::ZERO
        };
        session::end_child(fixture, grace)
    };
    session::combine([closed, exited])
}

fn feel(session: &mut Session, images: &Images, output: &Path, end: Instant) -> Result<()> {
    println!(
        "Injection is disabled. The dock is a short line at the top center by default. Try the shortcut, left Ctrl and left Alt. Press and hold both keys: after about 300 ms the hold is recognised, but this test copy has no speech model, so the line stays plain and nothing is recorded or sent; hovering the line may show Not installed, or Speech input status unavailable, in the status panel. The hold can't be judged in this copy, so choose Unrated for the Hold verdict. Double tap both keys, two short presses released less than 300 ms apart, to open the typing box under the dock; Esc hides it. AltGr and the right Ctrl and Alt keys never trigger the shortcut. This test window is only for the two verdicts and Ready; explicitly choose each verdict (Unrated is valid). Press Alt-Tab once and separately report whether any {DISPLAY_NAME} window appears. Then physically click Ready and do not touch the mouse or keyboard until this window closes (about 10 minutes, at most one hour). The first quiet second only opens injection. Input before injection opens returns to Ready without extending the feel deadline. Every feel verdict, including Off, is independent of owner approval."
    );
    let feel_directory = output.join("feel");
    fs::create_dir(&feel_directory)?;
    let diagnostics = Diagnostics::in_directory(&feel_directory);
    let mut child = platform::launch(
        &images.shell,
        &images.folders,
        120_000,
        [diagnostics.argument()],
    )?;
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

/// Every measured phase without the feel opener, opened by the owner's away rule.
pub(crate) fn measure(args: impl Iterator<Item = String>) -> Result<()> {
    session_run(
        args,
        (
            "idle-open-measure",
            measure_open,
            FixturePlacement::AwayFromCursor,
        ),
        &TRIALS,
        (None, None),
    )
}

// shortcut: the idle opener reserves time for one phase, so an open late in the hour fails later phases on their deadlines; reserve every phase's budget if runs start before the owner is away.
fn measure_open(session: &mut Session, images: &Images, output: &Path, end: Instant) -> Result<()> {
    println!(
        "This run has no Ready and records no feel verdicts: those come only from a gate-session with the owner."
    );
    fs::write(output.join("owner-feel.txt"), "not_part_of_this_run\n")?;
    idle_open(session, images, output, end)
}

/// Rez must have had no input for this long before the idle opener arms.
const AWAY: Duration = Duration::from_secs(600);

/// Opens injection only after `AWAY` with no input, then the unchanged guarded quiet second.
fn idle_open(session: &mut Session, _: &Images, output: &Path, end: Instant) -> Result<()> {
    println!(
        "Injection is disabled until Rez has had no input for {} minutes; any input after that stops the run.",
        AWAY.as_secs() / 60
    );
    let open_end = end
        .checked_sub(PHASE_LIMIT + FINAL_RESERVE)
        .context("session too short for the idle opener")?;
    let mut attempts = 0;
    loop {
        session.await_idle(open_end, AWAY)?;
        attempts += 1;
        println!("idle_attempt={attempts}");
        if session.await_quiet(open_end)? {
            break;
        }
        println!("idle_attempt={attempts} rearmed=true injection_opened=false");
    }
    fs::write(
        output.join("opener.txt"),
        format!(
            "opener=idle\naway_s={}\nattempts={attempts}\nroot_wait_ms={}\n",
            AWAY.as_secs(),
            session.root_wait.as_millis()
        ),
    )?;
    Ok(())
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
    presented: &mut Vec<observation::Presented>,
) -> Result<()> {
    images.unchanged()?;
    ensure!(
        platform::processes()?.is_empty(),
        "unexpected product process between phases"
    );
    if trial == Trial::Input(Phase::Listening(1), FixtureEscMode::Free) {
        images.install_speech()?;
    }
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
        &images.folders,
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
        if let Trial::Input(Phase::Listening(_), _) = trial {
            session::await_speech(session, &diagnostics)?;
        }
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
        session::check_flushed(phase, mode, &diagnostics)
    } else {
        Ok(())
    };
    let summarized = summarize(
        trial,
        &observer,
        (pid, dock),
        rows,
        &directory,
        &diagnostics,
        presented,
    );
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
        let (graces, longest) = session.take_null_graces();
        fs::write(
            directory.join("null-foreground.txt"),
            format!("graces={graces}\nlongest_ms={}\n", longest.as_millis()),
        )?;
        Ok(())
    })();
    session::combine([result, recorded])
}

fn summarize(
    trial: Trial,
    observer: &Path,
    (pid, dock): (u32, u64),
    rows: Option<Result<PresentRows>>,
    output: &Path,
    diagnostics: &Diagnostics,
    presented: &mut Vec<observation::Presented>,
) -> Result<()> {
    if trial.cpu_limit().is_some() {
        return Ok(());
    }
    let (seen, loaded) = observation::load(observer, pid);
    let counted = fs::write(
        output.join("pulls.txt"),
        format!("pulls={}\ncomplete={}\n", seen.pulls, loaded.is_ok()),
    )
    .map_err(anyhow::Error::from);
    let read = loaded.and_then(|()| {
        ensure!(seen.pulls == 0, "Kumo pull during {}", trial.name());
        Ok(())
    });
    let joined = (|| {
        let mut rows = rows.context("no main trace completed")??;
        match trial {
            Trial::Input(Phase::Hover, _) => observation::summarize_hover(
                observer,
                pid,
                &mut rows,
                dock,
                output,
                &diagnostics.hover_injections.borrow(),
            ),
            Trial::ReceiptRest => {
                receipts::summarize(observer, pid, &mut rows, dock, output, receipts::Mode::Rest)
            }
            Trial::Input(Phase::Listening(_), _) => observation::join_listening(
                &diagnostics.heard.borrow(),
                usize::try_from(session::BATCH_HOLDS)?,
                observer,
                (pid, dock),
                &mut rows,
                output,
                presented,
            ),
            _ => Ok(()),
        }
    })();
    session::combine([read, counted, joined])
}

#[cfg(test)]
mod tests {
    use super::{
        INPUT_RESERVE, Speech, TRIALS, Trial, end_fixture, evidence_directory, hold_options,
        phase_deadline,
    };
    use crate::hermetic::Prebuilt;
    use crate::platform::input_session::FixtureEscMode;
    use crate::session::Phase;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Stands in for a fixture whose window shows only after discovery gave up: it lives, with no
    /// window the driver knows, until it's ended.
    fn late_fixture() -> std::process::Child {
        Command::new("cmd")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd starts")
    }

    #[test]
    fn a_failed_session_ends_a_fixture_it_never_found_within_a_second() {
        let mut fixture = late_fixture();
        let started = Instant::now();
        let ended = end_fixture(&mut fixture, None, None);
        let took = started.elapsed();
        let gone = fixture.try_wait().expect("the child can be read").is_some();
        if !gone {
            fixture.kill().expect("the test child can be killed");
            fixture.wait().expect("the test child is reaped");
        }
        assert!(gone, "the fixture outlived the failed session's cleanup");
        assert!(took < Duration::from_secs(1), "{took:?}");
        ended.expect("terminating the owned fixture is a completed cleanup");
    }

    #[test]
    fn a_fixture_that_ignores_its_close_is_ended_after_the_grace() {
        let mut fixture = late_fixture();
        let grace = Duration::from_millis(200);
        let started = Instant::now();
        let ended = crate::session::end_child(&mut fixture, grace);
        let took = started.elapsed();
        let gone = fixture.try_wait().expect("the child can be read").is_some();
        if !gone {
            fixture.kill().expect("the test child can be killed");
            fixture.wait().expect("the test child is reaped");
        }
        assert!(gone, "the fixture outlived its close grace");
        assert!(
            took >= grace && took < grace + Duration::from_secs(1),
            "{took:?}"
        );
        ended.expect("terminating the owned fixture is a completed cleanup");
    }

    #[test]
    fn a_fixture_that_closes_within_the_grace_is_not_terminated() {
        let mut fixture = Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("cmd starts");
        crate::session::end_child(&mut fixture, Duration::from_secs(5)).expect("it exits");
        assert_eq!(
            fixture
                .try_wait()
                .expect("the child can be read")
                .map(|status| status.code()),
            Some(Some(0))
        );
    }

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

    fn options(args: &[&str]) -> anyhow::Result<(Option<Prebuilt>, Option<Speech>)> {
        hold_options(args.iter().map(|&arg| arg.to_owned()))
    }

    #[test]
    fn hold_gate_takes_prebuilt_pins_then_a_speech_source() {
        let (shell, brain) = ("a".repeat(64), "B".repeat(64));
        let pins = Prebuilt {
            shell_sha256: shell.clone(),
            brain_sha256: "b".repeat(64),
        };
        let speech = Speech {
            models: "m".into(),
            natives: "n".into(),
        };
        assert_eq!(options(&[]).unwrap(), (None, None));
        assert_eq!(
            options(&["--speech", "m", "n"]).unwrap(),
            (
                None,
                Some(Speech {
                    models: "m".into(),
                    natives: "n".into(),
                })
            )
        );
        assert_eq!(
            options(&["--prebuilt", &shell, &brain]).unwrap(),
            (
                Some(Prebuilt {
                    shell_sha256: shell.clone(),
                    brain_sha256: "b".repeat(64),
                }),
                None
            )
        );
        assert_eq!(
            options(&["--prebuilt", &shell, &brain, "--speech", "m", "n"]).unwrap(),
            (Some(pins), Some(speech))
        );
        for refused in [
            &["--speech", "m", "n", "--prebuilt", &shell, &brain][..],
            &["--prebuilt", &shell][..],
            &["--prebuilt", "abc", &brain][..],
            &["--speech", "m"][..],
            &["--speech", "m", "n", "extra"][..],
            &["--prebuilt", &shell, &brain, "extra"][..],
        ] {
            assert!(options(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn every_listening_batch_fits_the_shell_quit_bound_and_one_main_trace() {
        for batch in 1..=crate::session::BATCHES {
            let trial = Trial::Input(Phase::Listening(batch), FixtureEscMode::Free);
            let quit_ms = u64::try_from(trial.budget().as_millis()).unwrap();
            assert_eq!(quit_ms, 90_000);
            assert!((1..=120_000).contains(&quit_ms));
            let trace = trial.trace_bound().unwrap();
            assert_eq!(trace, Duration::from_secs(40));
            assert!((10..=70).contains(&trace.as_secs()));
            assert!(
                crate::session::SPEECH_WAIT + Duration::from_secs(1) + trace
                    <= trial.budget().checked_sub(INPUT_RESERVE).unwrap()
            );
            assert_eq!(trial.name(), format!("listening-{batch:02}"));
        }
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
    fn receipts_follow_idle_and_esc_conflict_is_last() {
        assert!(TRIALS.contains(&Trial::Input(Phase::Hover, FixtureEscMode::Free)));
        assert_eq!(&TRIALS[..2], &[Trial::Idle, Trial::ReceiptRest]);
        assert!(
            !TRIALS
                .iter()
                .any(|trial| matches!(trial, Trial::Input(Phase::Listening(_), _))),
            "only hold-gate with a speech source opens the microphone"
        );
        assert_eq!(
            &TRIALS[6..],
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
