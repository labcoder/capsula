use std::io::{self, Write as _};

use kumochi_protocol::frame::{self, Pipe};

use super::wire::{Request, RequestBody, pong};

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
    #[cfg(debug_assertions)]
    let mut fault = std::env::var(kumochi_protocol::constants::SCREEN_FAULT_ENV)
        .ok()
        .and_then(|value| match value.as_str() {
            "hang" => Some(super::wire::WorkerFault::Hang),
            "crash" => Some(super::wire::WorkerFault::Crash),
            "garbage" => Some(super::wire::WorkerFault::Garbage),
            "panic" => Some(super::wire::WorkerFault::Panic),
            _ => None,
        });
    while let Some(request) = frame::read::<Request>(&mut input, Pipe::Data)? {
        #[cfg(debug_assertions)]
        {
            if let RequestBody::Fault(requested) = request.body {
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
                }
            }
        }
        #[cfg(not(debug_assertions))]
        let RequestBody::Ping = request.body;
        frame::write(&mut output, &pong(request.id)?, Pipe::Data)?;
        output.flush()?;
    }
    Ok(())
}
