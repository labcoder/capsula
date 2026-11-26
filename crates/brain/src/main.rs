//! `kumochid`, Kumochi's headless brain. It has no console window.
#![windows_subsystem = "windows"]

use std::{ffi::OsString, process::ExitCode};

use kumochi_brain::platform::{Mode, supports};
use kumochi_brain::{
    BrainError, MCP_BRIDGE, PARENT_BOUND, ParentBoundError, SCREEN_WORKER, SPEECH_WORKER, logging,
};
use kumochi_protocol::constants::{self, BRAIN_EXIT_BUSY, ProductIdentity};

#[path = "main_platform.rs"]
mod platform;

const UNAVAILABLE: u8 = 69;
const BAD_ARGUMENTS: u8 = 64;
/// A parent-bound brain whose settlement didn't confirm every stage.
const INCOMPLETE: u8 = 70;
/// A parent-bound start that was refused: the parent, the inherited pair, the greeting or the
/// commit claim.
const REFUSED: u8 = 76;

#[derive(PartialEq, Eq)]
enum Route {
    Detached,
    ParentBound,
    BadParentBound,
    Bridge(String),
    Screen,
    Speech,
    Diagnostic(DevCheck),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DevCheck {
    Image,
    ImageDiagnostic,
    Marks,
    Draw,
    CatalogProvenance,
}

fn route(mut args: impl Iterator<Item = OsString>) -> Route {
    match args.next().as_deref() {
        Some(arg) if arg == PARENT_BOUND => {
            if args.next().is_none() {
                Route::ParentBound
            } else {
                Route::BadParentBound
            }
        }
        Some(arg) if arg == MCP_BRIDGE => Route::Bridge(
            args.next()
                .and_then(|arg| arg.into_string().ok())
                .unwrap_or_default(),
        ),
        Some(arg) if arg == SCREEN_WORKER => Route::Screen,
        Some(arg) if arg == SPEECH_WORKER => Route::Speech,
        Some(arg) if arg == constants::IMAGE_CHECK_ARG => Route::Diagnostic(DevCheck::Image),
        Some(arg) if arg == constants::IMAGE_DIAGNOSTIC_ARG => {
            Route::Diagnostic(DevCheck::ImageDiagnostic)
        }
        Some(arg) if arg == constants::MARKS_CHECK_ARG => Route::Diagnostic(DevCheck::Marks),
        Some(arg) if arg == constants::DRAW_CHECK_ARG => Route::Diagnostic(DevCheck::Draw),
        Some(arg) if arg == constants::CATALOG_PROVENANCE_ARG => {
            Route::Diagnostic(DevCheck::CatalogProvenance)
        }
        _ => Route::Detached,
    }
}

fn main() -> ExitCode {
    let route = route(std::env::args_os().skip(1));
    let mode = if route == Route::ParentBound {
        Mode::ParentBound
    } else {
        Mode::Detached
    };
    match route {
        Route::BadParentBound => ExitCode::from(BAD_ARGUMENTS),
        _ if !supports(mode) => ExitCode::from(UNAVAILABLE),
        Route::ParentBound => ExitCode::from(parent_bound()),
        Route::Detached => run_detached(),
        Route::Bridge(launch) => match kumochi_brain::bridge::serve(&launch) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "mcp bridge stopped");
                ExitCode::FAILURE
            }
        },
        Route::Screen => {
            if logging::init(constants::SCREEN_LOG).is_err() {
                return ExitCode::FAILURE;
            }
            if kumochi_brain::screen::serve().is_err() {
                tracing::error!(code = 1, "screen transport failed");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Route::Speech => {
            if logging::init(constants::SPEECH_LOG).is_err() {
                return ExitCode::FAILURE;
            }
            if kumochi_brain::speech::serve().is_err() {
                tracing::error!(code = 1, "speech transport failed");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Route::Diagnostic(diagnostic) => run_diagnostic(diagnostic),
    }
}

/// Takes descriptors 3 and 4 only after the mode and identity checks, and before anything else in
/// the process can open a descriptor.
fn parent_bound() -> u8 {
    if ProductIdentity::ACTIVE == ProductIdentity::GATE {
        return UNAVAILABLE;
    }
    let Ok(fds) = platform::claim_inherited() else {
        return REFUSED;
    };
    match kumochi_brain::run_parent_bound(fds) {
        Ok(()) => 0,
        Err(ParentBoundError::Unsupported) => UNAVAILABLE,
        Err(ParentBoundError::Busy) => BRAIN_EXIT_BUSY,
        Err(ParentBoundError::Refused) => REFUSED,
        Err(ParentBoundError::Incomplete) => INCOMPLETE,
    }
}

#[cfg(debug_assertions)]
fn run_diagnostic(diagnostic: DevCheck) -> ExitCode {
    use kumochi_brain::connections::{draw_check, image_check, marks_check, provenance};

    ExitCode::from(match diagnostic {
        DevCheck::Image => image_check::run(image_check::Set::Check),
        DevCheck::ImageDiagnostic => image_check::run(image_check::Set::Diagnostic),
        DevCheck::Marks => marks_check::run(),
        DevCheck::Draw => draw_check::run(),
        DevCheck::CatalogProvenance => provenance::run().code(),
    })
}

#[cfg(not(debug_assertions))]
fn run_diagnostic(_: DevCheck) -> ExitCode {
    ExitCode::from(10)
}

fn run_detached() -> ExitCode {
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
        edition = constants::EDITION,
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
        Err(BrainError::ProfileBusy) => {
            tracing::warn!("another session is using this profile");
            ExitCode::from(BRAIN_EXIT_BUSY)
        }
        Err(error) => {
            log_brain_stopped(&error);
            ExitCode::FAILURE
        }
    }
}

fn log_brain_stopped(error: &BrainError) {
    let (kind, os_code) = error.diagnostic();
    tracing::error!(kind, os_code, "brain stopped");
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
