//! `capsula`, Capsula's shell. It has no console window.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod credential;
mod escape;
mod input;
mod link;
mod logging;
mod platform;
mod presence;
mod strings;

use std::process::ExitCode;

use capsula_protocol::constants;
use strings::Strings;

fn main() -> ExitCode {
    if logging::init(constants::SHELL_LOG).is_err() {
        return ExitCode::FAILURE;
    }
    let _instance = match platform::lock_single_instance(constants::SHELL_MUTEX) {
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
    tracing::info!(build = capsula_protocol::BUILD_ID, "shell started");
    let (states, state_changes) = async_channel::unbounded();
    let (link, start) = match link::prepare(states) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, "can't prepare the link to the brain");
            return ExitCode::FAILURE;
        }
    };
    let flags = match presence::Launch::parse(std::env::args().skip(1)) {
        Ok(flags) => flags,
        Err(error) => {
            tracing::error!(%error, "can't start with these flags");
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
            cx.set_global(Strings::english());
            // The keyboard is armed inside `open`, so the link starts only afterwards (v1 §2.2).
            let started = presence::open(
                state_changes,
                link.clone(),
                &flags,
                diagnostics,
                observer,
                cx,
            )
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
