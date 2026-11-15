//! `kumochi`, Kumochi's shell. It has no console window.
#![cfg_attr(not(test), windows_subsystem = "windows")]

#[cfg(test)]
#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "shared allocator helpers serve several test binaries"
)]
mod allocation;

mod confirmation;
#[cfg(test)]
mod github_link_tests;
mod platform;
mod presence;
mod presenter;
mod strings;
mod theme;

use std::process::ExitCode;

use kumochi_protocol::constants;
use kumochi_shell_core::{
    accounts, chat_notice, claude_accounts, drawing, escape, input, instance, link, logging,
    onboarding, runs, settings, sign_in_mark, threads,
};
use strings::Strings;

fn main() -> ExitCode {
    let Ok(flags) = presence::Launch::parse(std::env::args().skip(1)) else {
        return ExitCode::FAILURE;
    };
    #[cfg(debug_assertions)]
    if let Some(state) = flags.accounts_fixture {
        let delay = flags
            .quit_delay
            .expect("fixture parsing requires a deadline");
        presence::run_accounts_fixture(state, flags.accounts_appearance, delay);
        return ExitCode::SUCCESS;
    }
    if logging::init(constants::SHELL_LOG).is_err() {
        return ExitCode::FAILURE;
    }
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
    let (link, start) = match link::prepare(events) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, "can't prepare the link to the brain");
            return ExitCode::FAILURE;
        }
    };
    let observer = match presence::Observer::prepare(flags.observer()) {
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
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
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
                            .spawn()
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
