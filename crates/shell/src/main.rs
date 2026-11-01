//! `capsula`, Capsula's shell. It has no console window.
#![windows_subsystem = "windows"]

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
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "shell started");
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            if let Err(error) = status::open(cx) {
                tracing::error!(%error, "can't open the status window");
                cx.quit();
            }
        });
    ExitCode::SUCCESS
}
