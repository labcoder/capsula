use std::io::{self, Write as _};
use std::time::Instant;

use kumochi_protocol::frame;

use super::capture::{Capturer, Failure, Screen};
use super::supervisor::Screen as ScreenKind;
use super::wire::{ReplyBody, RequestBody};
use crate::supervise::{Kind as _, read_request, write_message};

/// Serves stdin/stdout after the caller initializes content-free logging.
///
/// # Errors
/// Returns a framing or transport failure, never request contents.
///
/// # Panics
/// Debug builds panic when the explicit panic fixture is selected.
#[cfg_attr(
    debug_assertions,
    expect(
        clippy::panic_in_result_fn,
        reason = "explicit dev-only panic-hook fixture"
    )
)]
pub fn serve() -> Result<(), frame::FrameError> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut screen = Screen::default();
    #[cfg(debug_assertions)]
    let mut fault = std::env::var(kumochi_protocol::constants::SCREEN_FAULT_ENV)
        .ok()
        .and_then(|value| match value.as_str() {
            "hang" => Some(super::wire::WorkerFault::Hang),
            "crash" => Some(super::wire::WorkerFault::Crash),
            "garbage" => Some(super::wire::WorkerFault::Garbage),
            "panic" => Some(super::wire::WorkerFault::Panic),
            "exit" => Some(super::wire::WorkerFault::Exit),
            _ => None,
        });
    #[cfg(debug_assertions)]
    if fault == Some(super::wire::WorkerFault::Exit) {
        return Ok(());
    }
    while let Some((id, body)) = read_request::<RequestBody>(&mut input, ScreenKind::MAX_REQUEST)? {
        let received = Instant::now();
        #[cfg(debug_assertions)]
        {
            if let RequestBody::Fault(requested) = body {
                fault = Some(requested);
            }
            if let Some(fault) = fault.take() {
                use super::wire::WorkerFault;
                match fault {
                    WorkerFault::Hang => loop {
                        std::thread::park();
                    },
                    WorkerFault::Crash => std::process::exit(1),
                    WorkerFault::Garbage => {
                        output.write_all(&[0xff; 4])?;
                        output.flush()?;
                        continue;
                    }
                    WorkerFault::Panic => panic!("private screen fault payload"),
                    WorkerFault::Exit => return Ok(()),
                }
            }
        }
        let (reply, retire) = match body {
            RequestBody::Ping => (ReplyBody::Pong, false),
            RequestBody::Capture {
                ticket,
                display,
                exclude,
            } => match screen.capture(&ticket, &display, exclude, received, &Instant::now, open) {
                Ok(meta) => (ReplyBody::Captured(meta), false),
                Err(Failure::Refused(refusal)) => (ReplyBody::Refused(refusal), false),
                Err(Failure::Fault(code)) => (ReplyBody::Fault(code), false),
                Err(Failure::Retire(code)) => {
                    tracing::error!(code, "screen capture didn't stop; retiring the worker");
                    (ReplyBody::Fault(code), true)
                }
            },
            #[cfg(debug_assertions)]
            RequestBody::Fault(_) => continue,
        };
        write_message(&mut output, id, &reply, ScreenKind::MAX_REPLY)?;
        if retire {
            return Ok(());
        }
    }
    Ok(())
}

/// No platform captures yet, so every capture is a numeric fault.
// shortcut: A2b opens Windows Graphics Capture here; macOS will open ScreenCaptureKit.
fn open() -> Result<Box<dyn Capturer>, i32> {
    Err(NO_CAPTURER)
}

/// `E_NOTIMPL`, as the Windows adapter reports an unsupported capture.
const NO_CAPTURER: i32 = 0x8000_4001_u32.cast_signed();
