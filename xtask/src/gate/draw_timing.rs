//! One offline paint timing trial with prepared Gate images.

mod evidence;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{
    BRAIN_EXE, LIVE_BUDGET_ENV, LIVE_CHATGPT_TEXT_ENV, LIVE_CHATGPT_TEXT_STAMPS_FILE,
    LIVE_RUN_BIN_DIR, LIVE_RUNS_DIR, ProductIdentity, SHELL_EXE,
};

use super::{observation, retained_run};
use crate::hermetic::{self, Folders, Prebuilt, Profile, Run};
use crate::platform::{self, gate_capture, input_session::WindowKind};
use crate::session;

const LIMIT: Duration = Duration::from_secs(90);
const SHELL_LIMIT_MS: u64 = 65_000;

pub(super) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    hermetic::require_gate(ProductIdentity::ACTIVE)?;
    let transcript = args.next().context("draw-timing needs a transcript path")?;
    let mut args = args.peekable();
    let release_mode = args.next_if(|arg| arg == "--prebuilt").is_some();
    let pins = Prebuilt::parse(&mut args)?;
    ensure!(
        args.next().is_none(),
        "unexpected argument after draw-timing pins"
    );
    let exe = std::env::current_exe()?;
    let images = if release_mode {
        pins.release()?
    } else {
        ensure!(
            cfg!(debug_assertions),
            "release draw-timing requires --prebuilt"
        );
        exe.parent()
            .context("missing prebuilt image directory")?
            .to_owned()
    };
    retained_run(
        std::iter::once(transcript),
        Run::finish_discard,
        |output, run| {
            let folders = run.folders("app")?;
            let (shell, stamps) = if release_mode {
                prepare(&folders, |bin| pins.stage_into(&images, bin))?
            } else {
                prepare(&folders, |bin| {
                    stage_debug(&images, bin, [&pins.shell_sha256, &pins.brain_sha256])
                })?
            };
            fs::write(
                output.join("identity.txt"),
                format!(
                    "driver_build_id={}\nidentity={}\nprofile={}\ndriver_sha256={}\nshell_sha256={}\nbrain_sha256={}\n",
                    kumochi_protocol::BUILD_ID,
                    ProductIdentity::ACTIVE.data_dir,
                    if release_mode { "release" } else { "debug" },
                    gate_capture::file_sha256(&exe)?,
                    pins.shell_sha256,
                    pins.brain_sha256,
                ),
            )?;
            trial(output, &shell, &stamps, &folders)
        },
    )
}

/// Sets up the launch `folders` for the draw replay and stages its images; returns the shell and
/// the stamps file.
fn prepare(
    folders: &Folders,
    stage: impl FnOnce(&Path) -> Result<()>,
) -> Result<(PathBuf, PathBuf)> {
    folders.choose_model("ChatGpt", "draw-replay")?;
    let launch = folders
        .local
        .parent()
        .context("a launch folder has a parent")?
        .join(LIVE_RUNS_DIR)
        .join("1");
    let bin = launch.join(LIVE_RUN_BIN_DIR);
    fs::create_dir_all(&launch)?;
    stage(&bin)?;
    Ok((
        bin.join(SHELL_EXE),
        launch.join(LIVE_CHATGPT_TEXT_STAMPS_FILE),
    ))
}

fn stage_debug(images: &Path, bin: &Path, hashes: [&str; 2]) -> Result<()> {
    fs::create_dir(bin)?;
    for (name, expected) in [SHELL_EXE, BRAIN_EXE].into_iter().zip(hashes) {
        let copy = bin.join(name);
        fs::copy(images.join(name), &copy).with_context(|| format!("copying pinned {name}"))?;
        ensure!(
            gate_capture::file_sha256(&copy)?.eq_ignore_ascii_case(expected),
            "prebuilt {name} SHA-256 does not match its pin; no launch"
        );
    }
    hermetic::require_compatible(&bin.join(SHELL_EXE), hermetic::SHELL_ROLE, Profile::Debug)?;
    hermetic::require_compatible(&bin.join(BRAIN_EXE), hermetic::BRAIN_ROLE, Profile::Debug)
}

fn trial(output: &Path, shell: &Path, stamps: &Path, folders: &Folders) -> Result<()> {
    fs::write(
        output.join("environment-before.txt"),
        gate_capture::desktop_metadata()?,
    )?;
    let trace_path = output.join("observer.log");
    let mut observer_arg = OsString::from("--gate-observer=");
    observer_arg.push(&trace_path);
    let end = Instant::now() + LIMIT;
    ensure!(
        platform::processes()?.is_empty(),
        "unexpected product before draw launch"
    );
    let mut child = platform::launch_with(
        shell,
        folders,
        SHELL_LIMIT_MS,
        [observer_arg, "--draw-timing".into()],
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
    let painted = (|| {
        window = Some(session::discover(&mut child, WindowKind::ProductProxy)?);
        helpers = session::owned_helpers(pid)?;
        let brain = helpers
            .iter()
            .find_map(|(role, process, _)| (*role == "brain").then_some(process.pid))
            .context("missing owned brain")?;
        let mut reader = observation::Reader::open(&trace_path, pid)?;
        while !reader.update()?.all_painted() {
            ensure!(Instant::now() < end, "draw paint trial deadline exceeded");
            ensure!(
                child.try_wait()?.is_none(),
                "owned shell exited before four draw paints"
            );
            sleep(Duration::from_millis(10));
        }
        Ok(brain)
    })();
    let stopped = session::stop_product(Some(&mut child), window, helpers, end);
    let result = match painted {
        Ok(brain) => stopped.and_then(|()| {
            let observed = observation::read(&trace_path, pid)?;
            evidence::report(output, stamps, brain, &observed)
        }),
        Err(error) => session::combine([Err(error), stopped]),
    };
    let environment = gate_capture::desktop_metadata().and_then(|metadata| {
        fs::write(output.join("environment-after.txt"), metadata).map_err(anyhow::Error::from)
    });
    session::combine([result, environment])
}

#[cfg(test)]
#[test]
fn release_draw_stages_the_pinned_pair_in_the_owned_live_layout() {
    let dir = crate::scratch::dir("draw-prebuilt-layout");
    let images = dir.join("images");
    fs::create_dir(&images).unwrap();
    let shell = hermetic::image("shell started", ProductIdentity::GATE);
    let brain = hermetic::image("brain started", ProductIdentity::GATE);
    fs::write(images.join(SHELL_EXE), shell.as_bytes()).unwrap();
    fs::write(images.join(BRAIN_EXE), brain.as_bytes()).unwrap();
    let mut hashes = [SHELL_EXE, BRAIN_EXE]
        .into_iter()
        .map(|name| gate_capture::file_sha256(&images.join(name)).unwrap());
    let pins = Prebuilt::parse(&mut hashes).unwrap();
    let app = dir.join("app");
    let folders = Folders::create(&app).unwrap();
    let (staged, stamps) = prepare(&folders, |bin| pins.stage_into(&images, bin)).unwrap();
    let launch = app.join(LIVE_RUNS_DIR).join("1");
    assert_eq!(staged, launch.join(LIVE_RUN_BIN_DIR).join(SHELL_EXE));
    assert_eq!(stamps, launch.join(LIVE_CHATGPT_TEXT_STAMPS_FILE));
    assert!(!stamps.exists());
    assert_eq!(fs::read(staged).unwrap(), shell.as_bytes());
    assert_eq!(
        fs::read(launch.join(LIVE_RUN_BIN_DIR).join(BRAIN_EXE)).unwrap(),
        brain.as_bytes()
    );
}

#[cfg(test)]
#[test]
fn draw_trial_a_broken_or_missing_pin_does_not_poison_the_next_claim() {
    for missing in [false, true] {
        let dir = crate::scratch::dir("draw-bad-pin");
        let images = dir.join("images");
        fs::create_dir(&images).unwrap();
        let shell = images.join(SHELL_EXE);
        fs::write(&shell, b"fake shell").unwrap();
        let shell_pin = gate_capture::file_sha256(&shell).unwrap();
        if !missing {
            fs::write(images.join(BRAIN_EXE), b"fake brain").unwrap();
        }
        hermetic::assert_claim_after_refusal(&dir, |output| {
            let folders = Folders::create(&output.join("app")).unwrap();
            let error = prepare(&folders, |bin| {
                stage_debug(&images, bin, [&shell_pin, &"0".repeat(64)])
            })
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                if missing {
                    format!("copying pinned {BRAIN_EXE}")
                } else {
                    format!("prebuilt {BRAIN_EXE} SHA-256 does not match its pin; no launch")
                }
            );
            let local = output.join("app/local");
            assert!(
                local.is_dir(),
                "pin failure leaves a cleanable launch folder"
            );
            let app = local.join(ProductIdentity::GATE.data_dir);
            fs::create_dir(&app).unwrap();
            let db = rusqlite::Connection::open(app.join("fixture.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE draw_fixture (id INTEGER);")
                .unwrap();
            Err(error)
        });
    }
}
