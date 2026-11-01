//! `capsulad`, Capsula's headless brain. It has no console window.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use capsula_brain::logging;
use capsula_protocol::constants;

fn main() -> ExitCode {
    if logging::init(constants::BRAIN_LOG).is_err() {
        return ExitCode::FAILURE;
    }
    tracing::info!(build = capsula_protocol::BUILD_ID, "brain started");
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "can't start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(capsula_brain::run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, source = ?std::error::Error::source(&error), "brain stopped");
            ExitCode::FAILURE
        }
    }
}
