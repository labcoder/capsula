use std::io;
use std::time::Instant;

use kumochi_protocol::frame;

use super::capture::{Capturer, Failure, Open, Screen};
use super::elements::OpenElements;
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
pub fn serve() -> Result<(), frame::FrameError> {
    serve_on(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        open,
        open_elements,
        &Instant::now,
    )
}

/// Answers requests until `input` ends, a capture must retire the worker, or framing breaks.
#[cfg_attr(
    debug_assertions,
    expect(
        clippy::panic_in_result_fn,
        reason = "explicit dev-only panic-hook fixture"
    )
)]
fn serve_on(
    input: &mut impl io::Read,
    output: &mut impl io::Write,
    open: Open,
    open_elements: OpenElements,
    now: &dyn Fn() -> Instant,
) -> Result<(), frame::FrameError> {
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
    while let Some((id, body)) = read_request::<RequestBody>(input, ScreenKind::MAX_REQUEST)? {
        let received = now();
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
        let result = match body {
            RequestBody::Ping => Ok((ReplyBody::Pong, false)),
            RequestBody::Capture {
                ticket,
                display,
                exclude,
            } => screen
                .capture(
                    &ticket,
                    &display,
                    exclude,
                    received,
                    now,
                    (open, open_elements),
                )
                .map(|meta| (ReplyBody::Captured(meta), false)),
            RequestBody::Elements { capture, target } => screen
                .elements(capture, target, now)
                .map(|walked| (ReplyBody::Elements(walked.tree), walked.retire)),
            #[cfg(debug_assertions)]
            RequestBody::Fault(_) => continue,
        };
        let (reply, retire) = match result {
            Ok(reply) => reply,
            Err(Failure::Refused(refusal)) => (ReplyBody::Refused(refusal), false),
            Err(Failure::Fault(code)) => (ReplyBody::Fault(code), false),
            Err(Failure::Retire(code)) => {
                tracing::error!(code, "screen operation requires worker retirement");
                (ReplyBody::Fault(code), true)
            }
        };
        write_message(output, id, &reply, ScreenKind::MAX_REPLY)?;
        if retire {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn open() -> Result<Box<dyn Capturer>, i32> {
    crate::platform::screen_capturer()
}

/// No platform but Windows captures yet, so every capture is a numeric fault.
// shortcut: macOS will open ScreenCaptureKit here.
#[cfg(not(windows))]
fn open() -> Result<Box<dyn Capturer>, i32> {
    Err(NO_CAPTURER)
}

/// `E_NOTIMPL`, as the Windows adapter reports an unsupported capture.
#[cfg(not(windows))]
const NO_CAPTURER: i32 = 0x8000_4001_u32.cast_signed();

// shortcut: A3b opens UI Automation on Windows; other platforms follow later.
fn open_elements() -> Result<Box<dyn super::elements::ElementReader>, i32> {
    Err(0x8000_4001_u32.cast_signed())
}

#[cfg(test)]
mod tests;
