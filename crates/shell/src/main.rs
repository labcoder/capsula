//! `kumochi`, Kumochi's shell. It has no console window.
#![cfg_attr(not(test), windows_subsystem = "windows")]

#[cfg(test)]
#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "shared allocator helpers serve several test binaries"
)]
mod allocation;
#[cfg(test)]
#[path = "../../../tests/support/scratch.rs"]
mod scratch;

mod confirmation;
#[cfg(test)]
mod github_link_tests;
mod platform;
mod presence;
mod presenter;

use std::process::ExitCode;

use kumochi_protocol::constants;
use kumochi_protocol::message::AudioSession;
use kumochi_shell::{assets, strings, theme};
use kumochi_shell_core::{
    accounts, chat_notice, claude_accounts, drawing, escape, input, instance, link, logging,
    onboarding, runs, settings, sign_in_mark, threads,
};
use strings::Strings;

fn main() -> ExitCode {
    let launch = presence::Launch::parse(std::env::args().skip(1));
    #[cfg(debug_assertions)]
    if let Ok(flags) = &launch
        && let Some(state) = flags.accounts_fixture
    {
        let delay = flags
            .quit_delay
            .expect("fixture parsing requires a deadline");
        presence::run_accounts_fixture(state, flags.accounts_appearance, delay);
        return ExitCode::SUCCESS;
    }
    let Some(flags) = launch_with_logging(launch, || logging::init(constants::SHELL_LOG)) else {
        return ExitCode::FAILURE;
    };
    let Some(data_dir) = platform::data_dir() else {
        tracing::error!("can't find the data folder for the single-instance lock");
        return ExitCode::FAILURE;
    };
    let session = match platform::session_id() {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "can't read the session for the single-instance lock");
            return ExitCode::FAILURE;
        }
    };
    let lock = data_dir.join(format!("{}-{session}.lock", constants::SHELL_LOCK));
    let _instance = match instance::InstanceLock::try_acquire(&lock) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            tracing::info!("another shell is running in this session");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            tracing::error!(%error, "can't take the single-instance lock");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        "shell started"
    );
    let (events, link_events) = async_channel::unbounded();
    let (link, start) = match link::prepare(events, AudioSession::Legacy) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, "can't prepare the link to the brain");
            return ExitCode::FAILURE;
        }
    };
    let observer = match presence::Observer::prepare(&flags) {
        Ok(observer) => observer,
        Err(error) => {
            tracing::error!(%error, "can't create the --gate-observer file");
            return ExitCode::FAILURE;
        }
    };
    let diagnostics = match presence::Diagnostics::prepare() {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            tracing::error!(%error, "can't create the --input-diagnostics file");
            return ExitCode::FAILURE;
        }
    };
    let platform = gpui_kit::platform::current_platform(false);
    let drawing_text = platform.text_system();
    gpui_kit::Application::with_platform(platform)
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_global(Strings::english());
            // The keyboard is armed inside `open`, so the link starts only afterwards (v1 §2.2).
            let started =
                presence::open(link_events, link.clone(), &flags, diagnostics, observer, cx)
                    .map_err(|error| tracing::error!(%error, "can't open the dock"))
                    .and_then(|()| {
                        start
                            .spawn(crate::presenter::text_profiles(cx, drawing_text))
                            .map_err(|error| tracing::error!(%error, "can't start the link"))
                    });
            if started.is_err() {
                link.begin_quit();
                let diagnostics = presence::diagnostics::release(cx);
                let observer = presence::observer::release(cx);
                let told = cx.background_executor().spawn(async move {
                    link.finish_quit();
                    drop(diagnostics);
                    if let Some(observer) = observer {
                        observer.finish();
                    }
                });
                cx.spawn(async move |cx| {
                    told.await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
    ExitCode::SUCCESS
}

fn launch_with_logging(
    launch: Result<presence::Launch, presence::LaunchError>,
    init_logging: impl FnOnce() -> Result<(), logging::LoggingError>,
) -> Option<presence::Launch> {
    init_logging().ok()?;
    launch
        .inspect_err(|error| tracing::error!(%error, "invalid launch flags"))
        .ok()
}

#[cfg(test)]
mod launch_tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn invalid_launch_flags_get_one_actionable_redacted_diagnostic() {
        for (args, reason) in [
            (
                &["--accounts-fixture=private-fixture-value", "--smoke"][..],
                Some(
                    "--accounts-fixture requires a debug build, one fixed state and a quit deadline",
                ),
            ),
            (
                &[
                    "--accounts-fixture=enabled",
                    "--accounts-appearance=private-fixture-value",
                    "--smoke",
                ][..],
                Some(
                    "--accounts-appearance is optional and accepts light, dark, large or constrained once",
                ),
            ),
            (
                &[
                    "--gate-observer=C:/private-fixture-value/observer.log",
                    "--quit-after-ms=private-fixture-value",
                ][..],
                Some("--quit-after-ms must be between 1 and 120000"),
            ),
            (&[][..], None),
        ] {
            let out = Arc::new(Mutex::new(Vec::new()));
            let mut logger = None;
            let parsed = presence::Launch::parse(args.iter().map(|arg| (*arg).to_owned()));
            let flags = launch_with_logging(parsed, || {
                logger = Some(tracing::subscriber::set_default(
                    tracing_subscriber::fmt()
                        .without_time()
                        .with_ansi(false)
                        .with_target(false)
                        .with_writer({
                            let out = Arc::clone(&out);
                            move || Captured(Arc::clone(&out))
                        })
                        .finish(),
                ));
                Ok(())
            });
            drop(logger);
            let bytes = out.lock().unwrap();
            let log = std::str::from_utf8(&bytes).unwrap();
            if let Some(reason) = reason {
                assert_eq!(flags, None);
                assert_eq!(log.lines().count(), 1, "expected one startup diagnostic");
                assert!(log.contains("invalid launch flags"));
                assert!(log.contains(reason));
                assert!(!log.contains("private-fixture-value"));
                assert!(!log.contains("observer.log"));
            } else {
                assert_eq!(flags, Some(presence::Launch::default()));
                assert_eq!(log, "");
            }
        }
    }
}
