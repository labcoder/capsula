//! One offline paint timing trial with prebuilt Debug GATE images.

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
use crate::hermetic::{self, Folders, Run};
use crate::platform::{self, gate_capture, input_session::WindowKind};
use crate::session;

const LIMIT: Duration = Duration::from_secs(90);
const SHELL_LIMIT_MS: u64 = 65_000;

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
    retained_run(
        std::iter::once(transcript),
        Run::finish_discard,
        |output, _| {
            let bins = exe.parent().context("missing prebuilt image directory")?;
            let (folders, shell, stamps) = prepare(output, bins, &hashes)?;
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
            trial(output, &shell, &stamps, &folders)
        },
    )
}

fn prepare(
    output: &Path,
    images: &Path,
    hashes: &[String; 2],
) -> Result<(Folders, PathBuf, PathBuf)> {
    let app = output.join("data/app");
    let folders = Folders::create(&app)?;
    folders.choose_model("ChatGpt", "draw-replay")?;
    let launch = app.join(LIVE_RUNS_DIR).join("1");
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
        folders,
        bin.join(SHELL_EXE),
        launch.join(LIVE_CHATGPT_TEXT_STAMPS_FILE),
    ))
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
            let error = prepare(output, &images, &[shell_pin, "0".repeat(64)]).unwrap_err();
            assert_eq!(
                error.to_string(),
                if missing {
                    format!("copying pinned {BRAIN_EXE}")
                } else {
                    format!("prebuilt {BRAIN_EXE} SHA-256 does not match its pin; no launch")
                }
            );
            let local = output.join("data/app/local");
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
