//! `kumochid`, Kumochi's headless brain. It has no console window.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use kumochi_brain::logging;
use kumochi_brain::platform::require_native_runtime;
use kumochi_protocol::constants;

const UNAVAILABLE: u8 = 69;

#[expect(
    clippy::too_many_lines,
    reason = "all executable modes must return before falling through to normal runtime startup"
)]
fn main() -> ExitCode {
    if require_native_runtime().is_err() {
        return ExitCode::from(UNAVAILABLE);
    }

    let mut args = std::env::args();
    let worker = args.nth(1);
    if worker.as_deref() == Some(kumochi_brain::MCP_BRIDGE) {
        let launch = args.next().unwrap_or_default();
        return match kumochi_brain::bridge::serve(&launch) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "mcp bridge stopped");
                ExitCode::FAILURE
            }
        };
    }
    if worker.as_deref() == Some(kumochi_brain::SCREEN_WORKER) {
        if logging::init(constants::SCREEN_LOG).is_err() {
            return ExitCode::FAILURE;
        }
        if kumochi_brain::screen::serve().is_err() {
            tracing::error!(code = 1, "screen transport failed");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    if worker.as_deref() == Some(kumochi_brain::SPEECH_WORKER) {
        #[cfg(windows)]
        {
            if logging::init(constants::SPEECH_LOG).is_err() {
                return ExitCode::FAILURE;
            }
            if kumochi_brain::speech::serve().is_err() {
                tracing::error!(code = 1, "speech transport failed");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        #[cfg(not(windows))]
        return ExitCode::from(UNAVAILABLE);
    }
    let diagnostic = worker.as_deref() == Some(constants::IMAGE_DIAGNOSTIC_ARG);
    if diagnostic || worker.as_deref() == Some(constants::IMAGE_CHECK_ARG) {
        #[cfg(debug_assertions)]
        return ExitCode::from(kumochi_brain::connections::image_check::run(
            if diagnostic {
                kumochi_brain::connections::image_check::Set::Diagnostic
            } else {
                kumochi_brain::connections::image_check::Set::Check
            },
        ));
        // Without the mode the flag still never reaches a normal start, which could refresh.
        #[cfg(not(debug_assertions))]
        return ExitCode::from(10);
    }
    if worker.as_deref() == Some(constants::MARKS_CHECK_ARG) {
        #[cfg(debug_assertions)]
        return ExitCode::from(kumochi_brain::connections::marks_check::run());
        // Without the mode the flag still never reaches a normal start, which could refresh.
        #[cfg(not(debug_assertions))]
        return ExitCode::from(10);
    }
    if worker.as_deref() == Some(constants::DRAW_CHECK_ARG) {
        #[cfg(debug_assertions)]
        return ExitCode::from(kumochi_brain::connections::draw_check::run());
        // Without the mode the flag still never reaches a normal start, which could refresh.
        #[cfg(not(debug_assertions))]
        return ExitCode::from(10);
    }
    if worker.as_deref() == Some(constants::CATALOG_PROVENANCE_ARG) {
        #[cfg(debug_assertions)]
        return ExitCode::from(kumochi_brain::connections::provenance::run().code());
        // Without the mode the flag still never reaches a normal start, which could refresh.
        #[cfg(not(debug_assertions))]
        return ExitCode::from(10);
    }
    if logging::init(constants::BRAIN_LOG).is_err() {
        return ExitCode::FAILURE;
    }
    if let Err(error) = kumochi_scene::deferred::start() {
        tracing::error!(%error, "can't start scene destruction");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        "brain started"
    );
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
