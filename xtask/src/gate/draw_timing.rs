//! One guarded, offline draw timing trial with prebuilt Debug GATE images.

mod evidence;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{
    BRAIN_EXE, LIVE_BUDGET_ENV, LIVE_CHATGPT_TEXT_ENV, LIVE_CHATGPT_TEXT_STAMPS_FILE,
    LIVE_RUN_BIN_DIR, LIVE_RUNS_DIR, ProductIdentity, SHELL_EXE,
};

use super::{await_idle_open, await_startup, capture::Capture, observation, retained_run};
use crate::hermetic::{self, Folders};
use crate::platform::{
    self, gate_capture,
    input_session::{self as native, FixturePlacement, Registry, Session, Window, WindowKind},
};
use crate::session::{self, Diagnostics, Mark};

const LIMIT: Duration = Duration::from_secs(90);
const SHELL_LIMIT_MS: u64 = 65_000;
const CAPTURE_LIMIT: Duration = Duration::from_secs(30);

pub(super) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    hermetic::require_gate(ProductIdentity::ACTIVE)?;
    ensure!(
        cfg!(debug_assertions),
        "draw-timing requires prebuilt Debug GATE images"
    );
    let transcript = args.next().context("draw-timing needs a transcript path")?;
    let hashes = [
        args.next()
            .context("draw-timing needs the pinned shell SHA-256")?,
        args.next()
            .context("draw-timing needs the pinned brain SHA-256")?,
    ];
    ensure!(
        args.next().is_none()
            && hashes
                .iter()
                .all(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
        "draw-timing takes only a transcript path and two SHA-256 pins"
    );
    let exe = std::env::current_exe()?;
    retained_run(std::iter::once(transcript), |output, run| {
        let bins = exe.parent().context("missing prebuilt image directory")?;
        let (shell, stamps) = prepare(output, bins, &hashes)?;
        let folders = run.folders("app")?;
        folders.choose_model("ChatGpt", "draw-replay")?;
        fs::write(
            output.join("identity.txt"),
            format!(
                "driver_build_id={}\nidentity={}\nprofile=debug\ndriver_sha256={}\nshell_sha256={}\nbrain_sha256={}\n",
                kumochi_protocol::BUILD_ID,
                ProductIdentity::ACTIVE.data_dir,
                gate_capture::file_sha256(&exe)?,
                hashes[0],
                hashes[1],
            ),
        )?;
        guarded_session(&exe, output, &shell, &stamps, &folders)
    })
}

fn prepare(output: &Path, images: &Path, hashes: &[String; 2]) -> Result<(PathBuf, PathBuf)> {
    let launch = output.join("data/app").join(LIVE_RUNS_DIR).join("1");
    let bin = launch.join(LIVE_RUN_BIN_DIR);
    fs::create_dir_all(&bin)?;
    for (name, expected) in [SHELL_EXE, BRAIN_EXE].into_iter().zip(hashes) {
        let copy = bin.join(name);
        fs::copy(images.join(name), &copy).with_context(|| format!("copying pinned {name}"))?;
        ensure!(
            gate_capture::file_sha256(&copy)?.eq_ignore_ascii_case(expected),
            "prebuilt {name} SHA-256 does not match its pin; no launch"
        );
    }
    Ok((
        bin.join(SHELL_EXE),
        launch.join(LIVE_CHATGPT_TEXT_STAMPS_FILE),
    ))
}

fn guarded_session(
    exe: &Path,
    output: &Path,
    shell: &Path,
    stamps: &Path,
    folders: &Folders,
) -> Result<()> {
    fs::write(
        output.join("environment-before.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    let end = Instant::now() + LIMIT;
    let mut fixture = native::spawn_fixture(exe, LIMIT, FixturePlacement::AwayFromCursor)?;
    let mut window = None;
    let measured = (|| {
        let owned = session::discover(&mut fixture, WindowKind::Fixture)?;
        window = Some(owned);
        let mut session = Session::new(owned, end)?;
        await_idle_open(
            &mut session,
            output,
            Instant::now() + Duration::from_secs(10),
        )?;
        session.set_phase(1)?;
        let before = Registry::snapshot();
        let measured = trial(&mut session, output, shell, stamps, folders, end);
        let released = session.release_injected();
        let registry = before.ensure_unchanged(&Registry::snapshot());
        if let Some(stop) = session.stop_metadata() {
            println!("guard_stop={stop:?}");
        }
        session::combine([measured, released, registry])
    })();
    let closed = window.map_or(Ok(()), Window::close);
    let exited = session::wait_child(&mut fixture, end);
    let environment = gate_capture::desktop_metadata().and_then(|metadata| {
        fs::write(output.join("environment-after.txt"), metadata).map_err(anyhow::Error::from)
    });
    session::combine([measured, closed, exited, environment])
}

fn trial(
    session: &mut Session,
    output: &Path,
    shell: &Path,
    stamps: &Path,
    folders: &Folders,
    end: Instant,
) -> Result<()> {
    let diagnostics = Diagnostics::in_directory(output);
    let trace_path = output.join("observer.log");
    let mut observer_arg = OsString::from("--gate-observer=");
    observer_arg.push(&trace_path);
    let input_end = end
        .checked_sub(Duration::from_secs(12))
        .context("no draw cleanup reserve")?;
    session.guard()?;
    ensure!(
        platform::processes()?.is_empty(),
        "unexpected product before draw launch"
    );
    let mut child = platform::launch_with(
        shell,
        folders,
        SHELL_LIMIT_MS,
        [diagnostics.argument(), observer_arg, "--draw-timing".into()],
        |command| {
            command.env_clear().envs(
                ["SystemRoot", "WINDIR"]
                    .into_iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
            );
            command
                .env(LIVE_CHATGPT_TEXT_ENV, "draw")
                .env(LIVE_BUDGET_ENV, "1");
        },
    )?;
    let pid = child.id();
    let mut window = None;
    let mut helpers = Vec::new();
    let mut capture = None;
    let mut dismissed_qpc = None;
    let body = (|| {
        let product = session::discover(&mut child, WindowKind::ProductProxy)?;
        window = Some(product);
        helpers = session::owned_helpers(pid)?;
        let brain = brain_pid(&helpers)?;
        session.bind_product(product, input_end)?;
        diagnostics.await_started(session)?;
        let mut reader = observation::Reader::open(&trace_path, pid)?;
        let startup = await_startup(&mut reader, session)?;
        ensure!(
            startup.frequency == session.qpc_frequency,
            "shell and gate QPC rates differ"
        );
        session.bind_panel(startup.panel_hwnd)?;
        evidence::ready(stamps, brain, session)?;
        let trace = Capture::warm_and_start(
            pid,
            &output.join("presents.csv"),
            CAPTURE_LIMIT,
            || product.request_gate_repaint(),
            || session.guard(),
        )?;
        let trace_end = trace.input_deadline();
        capture = Some(trace);
        let work_end = input_end
            .min(trace_end)
            .checked_sub(Duration::from_secs(1))
            .context("no draw capture cleanup reserve")?;
        dismissed_qpc = Some(submit(session, &diagnostics)?);
        while !reader.update()?.all_drawn() {
            ensure!(
                Instant::now() < work_end,
                "drawing observer did not record four completed paints"
            );
            session.wait(Duration::from_millis(10))?;
        }
        session.wait(Duration::from_millis(250))
    })();
    let released = session.release_injected();
    let captured = capture
        .as_mut()
        .map(|trace| (trace.dock_swapchain, trace.finish(|| session.guard())));
    session.unbind_product();
    let brain = brain_pid(&helpers);
    let stopped = session::stop_product(Some(&mut child), window, helpers, end);
    let report = (|| {
        let (dock, rows) = captured.context("no main draw capture")?;
        let observed = observation::read(&trace_path, pid)?;
        evidence::report(
            output,
            stamps,
            brain?,
            dismissed_qpc.context("no guarded typing dismissal")?,
            &observed,
            &mut rows?,
            dock,
        )
    })();
    session::combine([body, released, stopped, report])
}

fn brain_pid(helpers: &[(&str, platform::Process, u32)]) -> Result<u32> {
    helpers
        .iter()
        .find_map(|(role, process, _)| (*role == "brain").then_some(process.pid))
        .context("missing owned brain")
}

fn submit(session: &mut Session, diagnostics: &Diagnostics) -> Result<i64> {
    ensure!(
        session.fixture_foreground(),
        "the owned fixture is not foreground before the typing chord"
    );
    let since = session::short_chord(session)?;
    session.wait(Duration::from_millis(50))?;
    session::short_chord(session)?;
    let opened = diagnostics.observe(session, since, |records| {
        records
            .iter()
            .any(|record| matches!(record.mark, Mark::Opened(_)))
    })?;
    ensure!(
        opened
            .iter()
            .any(|record| record.mark == Mark::Opened(true)),
        "typing did not open with focus; no text or Enter sent"
    );
    let typing = session.typing_window()?;
    session.tap_typing(0x1e, typing)?;
    session.tap_typing(0x1c, typing)?;
    let enter = session.last_qpc.context("missing Enter QPC")?;
    let dismissed = diagnostics.observe(session, enter, |records| {
        records.iter().any(|record| record.mark == Mark::Dismissed)
    })?;
    ensure!(
        !typing.is_visible()? && session.fixture_foreground(),
        "typing dismissal did not hide the verified window and return fixture foreground"
    );
    dismissed
        .iter()
        .find_map(|record| (record.mark == Mark::Dismissed).then_some(record.qpc))
        .context("missing typing dismissal QPC")
}
