//! What every native input adapter does once it has tried to open its stream: collect, stop,
//! confirm the device, then either finish the packets for release or discard them.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::MicPcm;

use super::pcm::{Format, Packetizer};
use super::{Finalization, Generation, MicError, Mode, PacketBoundary};

#[cfg(test)]
mod tests;

/// An open native input stream, as the shared capture sequence drives it.
pub(in crate::platform) trait Stream {
    fn format(&self) -> Format;
    /// Stops the device; `Unconfirmed` when its state afterwards is unknown.
    fn stop(&mut self) -> Result<(), MicError>;
    /// Fails unless the device is still present with the format it opened with.
    fn check(&self) -> Result<(), MicError>;
}

/// Drives one opened (or failed) stream to its finalization. `collect` runs the adapter's
/// pump until release or failure; the stream drops before the last packet is sent.
pub(in crate::platform) fn run<S: Stream>(
    token: &Generation,
    session: &AtomicBool,
    output: &Sender<MicPcm>,
    boundary: &mut PacketBoundary,
    opened: Result<S, MicError>,
    collect: impl FnOnce(&mut S, &mut Packetizer, &mut PacketBoundary) -> Result<(), MicError>,
) -> Result<Finalization, MicError> {
    let mut source = match opened {
        Ok(source) => source,
        Err(error) => {
            return Ok(match (error, token.mode(session, Instant::now())) {
                (MicError::Cancelled | MicError::Stale, Ok(Mode::Release)) => {
                    Finalization::Released { packets: 0 }
                }
                _ => Finalization::Discarded(error),
            });
        }
    };
    let mut packets = Packetizer::new(token.gesture(), source.format())?;
    let mut result = collect(&mut source, &mut packets, boundary);
    boundary.queued(packets.high_water());
    if matches!(result, Err(MicError::Stale | MicError::Cancelled))
        && token.mode(session, Instant::now()) == Ok(Mode::Release)
    {
        result = Ok(());
    }
    if result.is_err() {
        token.cancel();
        packets.clear();
    }
    let stopped = source.stop();
    if stopped == Err(MicError::Unconfirmed) {
        return Err(MicError::Unconfirmed);
    }
    result = boundary.finish(result.and(stopped).and_then(|()| source.check()));
    if result.is_err() {
        token.cancel();
        packets.clear();
    }
    drop(source);
    if result == Err(MicError::Unconfirmed) {
        return Err(MicError::Unconfirmed);
    }
    match result {
        Ok(()) => match token.mode(session, Instant::now()) {
            Ok(Mode::Release) => {
                let finished = packets.finish(output);
                boundary.queued(packets.high_water());
                Ok(match finished {
                    Ok(packets) => Finalization::Released { packets },
                    Err(error) => {
                        token.cancel();
                        Finalization::Discarded(error)
                    }
                })
            }
            Err(error) => Ok(Finalization::Discarded(error)),
            _ => Ok(Finalization::Discarded(MicError::Cancelled)),
        },
        Err(error) => Ok(Finalization::Discarded(error)),
    }
}
