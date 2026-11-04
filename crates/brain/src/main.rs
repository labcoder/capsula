//! `kumochid`, Kumochi's headless brain. It has no console window.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use kumochi_brain::logging;
use kumochi_protocol::constants;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some(kumochi_brain::SCREEN_WORKER) {
        // shortcut: the fake screen worker only waits for its job to end it; slice 6 builds it.
        loop {
            std::thread::park();
        }
    }
    if logging::init(constants::BRAIN_LOG).is_err() {
        return ExitCode::FAILURE;
    }
    if let Err(error) = kumochi_scene::deferred::start() {
        tracing::error!(%error, "can't start scene destruction");
        return ExitCode::FAILURE;
    }
    tracing::info!(build = kumochi_protocol::BUILD_ID, "brain started");
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
    let ran = runtime.block_on(kumochi_brain::run());
    kumochi_brain::shut_down(runtime);
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, source = ?std::error::Error::source(&error), "brain stopped");
            ExitCode::FAILURE
        }
    }
}
