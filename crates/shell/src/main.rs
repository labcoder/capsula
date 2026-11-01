//! `capsula`, Capsula's shell. It has no console window.
#![windows_subsystem = "windows"]

mod credential;
mod link;
mod logging;
mod platform;
mod status;
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
    if let Err(error) = link::spawn(states) {
        tracing::error!(%error, "can't start the link to the brain");
        return ExitCode::FAILURE;
    }
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            if let Err(error) = status::open(state_changes, cx) {
                tracing::error!(%error, "can't open the status window");
                cx.quit();
            }
        });
    ExitCode::SUCCESS
}
