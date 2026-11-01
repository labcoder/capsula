//! `capsulad`, Capsula's headless brain. It has no console window.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use capsula_brain::logging;
use capsula_protocol::constants;

fn main() -> ExitCode {
    if logging::init(constants::BRAIN_LOG).is_err() {
        return ExitCode::FAILURE;
    }
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "brain started");
    ExitCode::SUCCESS
}
