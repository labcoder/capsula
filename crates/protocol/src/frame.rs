//! Framing: `u32` little-endian length, then a `postcard` payload. Incoming length is checked
//! against the pipe's cap before anything is allocated.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::constants::OBJECT_PREFIX;

/// Bytes in a frame header.
pub const HEADER_LEN: usize = 4;

/// The two pipes between a brain and a shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    /// Small, latency-sensitive messages: handshake, Esc, run state.
    Control,
    /// Everything that can grow: steps, audio, text, images.
    Data,
}

impl Pipe {
    /// The largest payload this pipe accepts. Control messages have fixed maximum sizes well
    /// under its cap; anything that can grow is chunked on the data pipe.
    pub const fn max_payload(self) -> usize {
        match self {
            Self::Control => 64 * 1024,
            Self::Data => 256 * 1024,
        }
    }

    /// The pipe's name for one Windows session, so two signed-in users never share a name.
    pub fn name(self, session: u32) -> String {
        let suffix = match self {
            Self::Control => "control",
            Self::Data => "data",
        };
        format!(r"\\.\pipe\{OBJECT_PREFIX}-{session}-{suffix}")
    }
}

/// Why a frame couldn't be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The length is over the pipe's cap; the payload was never read.
    #[error("frame of {len} bytes is over the {max}-byte cap")]
    TooLarge {
        /// The declared or encoded length.
        len: usize,
        /// The pipe's cap.
        max: usize,
    },
    /// The stream ended inside a frame.
    #[error("the stream ended inside a frame")]
    Truncated,
    /// The payload isn't a valid message.
    #[error("malformed message")]
    Malformed(#[from] postcard::Error),
    /// The transport failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A chunked message's chunks broke their order, id or cap.
    #[error("chunks out of order")]
    Chunks,
}

/// Serializes `message` with no frame header and no cap, for a caller that chunks it under its
/// own cap.
///
/// # Errors
/// Fails when the message can't be encoded.
pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    Ok(postcard::to_allocvec(message)?)
}

/// Encodes `message` as one frame for `pipe`.
///
/// # Errors
/// Fails when the message is over the pipe's cap or can't be encoded.
pub fn encode<T: Serialize>(message: &T, pipe: Pipe) -> Result<Vec<u8>, FrameError> {
    let mut frame = Vec::new();
    encode_into(message, pipe, &mut frame)?;
    Ok(frame)
}

/// Encodes one frame into caller-owned storage, replacing its previous contents.
/// Partial or over-cap bytes remain on error; the caller owns any private-buffer cleanup.
///
/// # Errors
/// Fails when the message is over the pipe's cap or can't be encoded.
pub fn encode_into<T: Serialize>(
    message: &T,
    pipe: Pipe,
    frame: &mut Vec<u8>,
) -> Result<(), FrameError> {
    frame.clear();
    frame.resize(HEADER_LEN, 0);
    postcard::to_io(message, &mut *frame)?;
    let len = frame.len() - HEADER_LEN;
    let max = pipe.max_payload();
    let declared = u32::try_from(len)
        .ok()
        .filter(|_| len <= max)
        .ok_or(FrameError::TooLarge { len, max })?;
    frame[..HEADER_LEN].copy_from_slice(&declared.to_le_bytes());
    Ok(())
}

/// The payload length a header declares, checked against `pipe`'s cap.
///
/// # Errors
/// Fails when the length is over the cap.
pub fn payload_len(header: [u8; HEADER_LEN], pipe: Pipe) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    let max = pipe.max_payload();
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    Ok(len)
}

/// Decodes one payload. Trailing bytes are an error, so a frame means exactly one message.
///
/// # Errors
/// Fails when the payload isn't exactly one valid message.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    let (message, rest) = postcard::take_from_bytes(payload)?;
    if rest.is_empty() {
        Ok(message)
    } else {
        Err(FrameError::Malformed(
            postcard::Error::DeserializeBadEncoding,
        ))
    }
}

/// Reads one frame from a blocking stream. `None` means the peer closed cleanly between frames.
///
/// # Errors
/// Fails on a transport error, an over-cap length, a stream that ends mid-frame, or a payload
/// that isn't exactly one message.
pub fn read<T: DeserializeOwned>(
    reader: &mut impl Read,
    pipe: Pipe,
) -> Result<Option<T>, FrameError> {
    let mut header = [0; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut payload = vec![0; payload_len(header, pipe)?];
    reader
        .read_exact(&mut payload)
        .map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => FrameError::Truncated,
            _ => FrameError::Io(e),
        })?;
    decode(&payload).map(Some)
}

/// Writes one frame to a blocking stream.
///
/// # Errors
/// Fails when the message is over the cap or the transport fails.
pub fn write<T: Serialize>(
    writer: &mut impl Write,
    message: &T,
    pipe: Pipe,
) -> Result<(), FrameError> {
    writer.write_all(&encode(message, pipe)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FrameError, HEADER_LEN, Pipe, decode, encode, encode_into, payload_len, read};
    use crate::Secret;
    use crate::message::{ControlToBrain, Hello};

    fn hello() -> ControlToBrain {
        ControlToBrain::Hello(Hello {
            build_id: "0.1.0+abc".to_owned(),
            capability: Secret::new([7; 32]),
        })
    }

    #[test]
    fn caller_owned_frames_replace_previous_bytes_without_changing_the_wire() {
        let mut frame = vec![88; 32];
        encode_into(&513_u16, Pipe::Control, &mut frame).expect("first frame");
        assert_eq!(frame, [2, 0, 0, 0, 129, 4]);
        encode_into(&0_u8, Pipe::Data, &mut frame).expect("replacement frame");
        assert_eq!(frame, [1, 0, 0, 0, 0]);
    }

    #[test]
    fn frames_round_trip_back_to_back() {
        let mut stream = encode(&hello(), Pipe::Control).expect("hello encodes");
        stream.extend(encode(&ControlToBrain::Quitting, Pipe::Control).expect("quitting encodes"));
        let mut reader = stream.as_slice();
        let first: Option<ControlToBrain> = read(&mut reader, Pipe::Control).expect("first frame");
        let second: Option<ControlToBrain> =
            read(&mut reader, Pipe::Control).expect("second frame");
        let end: Option<ControlToBrain> = read(&mut reader, Pipe::Control).expect("clean end");
        assert_eq!(first, Some(hello()));
        assert_eq!(second, Some(ControlToBrain::Quitting));
        assert_eq!(end, None);
    }

    #[test]
    fn empty_stream_is_a_clean_end() {
        let got: Option<ControlToBrain> =
            read(&mut [].as_slice(), Pipe::Control).expect("no error");
        assert_eq!(got, None);
    }

    #[test]
    fn truncated_header_and_payload_are_errors() {
        let frame = encode(&hello(), Pipe::Control).expect("hello encodes");
        for cut in [1, HEADER_LEN, frame.len() - 1] {
            let result = read::<ControlToBrain>(&mut &frame[..cut], Pipe::Control);
            assert!(
                matches!(result, Err(FrameError::Truncated)),
                "cut at {cut}: {result:?}"
            );
        }
    }

    #[test]
    fn length_is_checked_against_each_pipe_cap() {
        let at_control_cap = u32::try_from(Pipe::Control.max_payload())
            .expect("fits")
            .to_le_bytes();
        let over_control_cap = (u32::from_le_bytes(at_control_cap) + 1).to_le_bytes();
        assert_eq!(
            payload_len(at_control_cap, Pipe::Control).expect("at cap"),
            64 * 1024
        );
        assert!(matches!(
            payload_len(over_control_cap, Pipe::Control),
            Err(FrameError::TooLarge {
                len: 65_537,
                max: 65_536
            })
        ));
        assert_eq!(
            payload_len(over_control_cap, Pipe::Data).expect("under data cap"),
            65_537
        );
        assert!(matches!(
            payload_len([0xff; HEADER_LEN], Pipe::Data),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn oversized_messages_are_refused_on_encode() {
        let big = ControlToBrain::Hello(Hello {
            build_id: "x".repeat(Pipe::Control.max_payload()),
            capability: Secret::new([0; 32]),
        });
        assert!(matches!(
            encode(&big, Pipe::Control),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(
            encode(&big, Pipe::Data).is_ok(),
            "the same message fits the data pipe"
        );
    }

    #[test]
    fn garbage_and_trailing_bytes_are_malformed() {
        assert!(matches!(
            decode::<ControlToBrain>(&[9]),
            Err(FrameError::Malformed(_))
        ));
        assert!(matches!(
            decode::<ControlToBrain>(&[]),
            Err(FrameError::Malformed(_))
        ));
        let quitting = encode(&ControlToBrain::Quitting, Pipe::Control).expect("encodes");
        let mut payload = quitting[HEADER_LEN..].to_vec();
        payload.push(0);
        assert!(matches!(
            decode::<ControlToBrain>(&payload),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn pipe_names_differ_by_session_and_pipe() {
        assert_ne!(Pipe::Control.name(1), Pipe::Control.name(2));
        assert_ne!(Pipe::Control.name(1), Pipe::Data.name(1));
        assert!(
            Pipe::Data.name(1).starts_with(r"\\.\pipe\"),
            "{}",
            Pipe::Data.name(1)
        );
    }
}
