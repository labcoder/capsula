//! The pipe tap: a dev-only recording of every frame the brain sends and receives, which
//! `cargo xtask pipe-dump` decodes with the same message types. Each record is the pipe, the
//! direction, a `u32` little-endian length and the payload.
//!
//! A recording holds the user's turns and the assistant's text, so it's a transcript: keep it
//! local and delete it like one.

use std::io::{self, Read, Write};

use crate::frame::{FrameError, HEADER_LEN, Pipe, decode, payload_len};
use crate::message::{ControlToBrain, ControlToShell, DataToBrain, DataToShell};

/// Which way a frame travelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Shell to brain.
    ToBrain,
    /// Brain to shell.
    ToShell,
}

/// One recorded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The pipe it travelled on.
    pub pipe: Pipe,
    /// Which way.
    pub direction: Direction,
    /// The frame's payload, without its header. A recorded `Hello` has a zeroed capability.
    pub payload: Vec<u8>,
}

impl Record {
    /// The payload decoded with the message type for its pipe and direction.
    pub fn describe(&self) -> String {
        let decoded = match (self.pipe, self.direction) {
            (Pipe::Control, Direction::ToBrain) => {
                decode::<ControlToBrain>(&self.payload).map(|m| format!("{m:?}"))
            }
            (Pipe::Control, Direction::ToShell) => {
                decode::<ControlToShell>(&self.payload).map(|m| format!("{m:?}"))
            }
            (Pipe::Data, Direction::ToBrain) => {
                decode::<DataToBrain>(&self.payload).map(|m| format!("{m:?}"))
            }
            (Pipe::Data, Direction::ToShell) => {
                decode::<DataToShell>(&self.payload).map(|m| format!("{m:?}"))
            }
        };
        decoded.unwrap_or_else(|error| format!("undecodable {} bytes: {error}", self.payload.len()))
    }
}

/// Appends one record.
///
/// # Errors
/// Fails when the payload is over its pipe's cap or the write fails.
pub fn write(writer: &mut impl Write, record: &Record) -> Result<(), FrameError> {
    let len = payload_len_checked(record)?;
    let pipe = match record.pipe {
        Pipe::Control => 0,
        Pipe::Data => 1,
    };
    let direction = match record.direction {
        Direction::ToBrain => 0,
        Direction::ToShell => 1,
    };
    writer.write_all(&[pipe, direction])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&record.payload)?;
    Ok(())
}

fn payload_len_checked(record: &Record) -> Result<u32, FrameError> {
    let len = record.payload.len();
    let max = record.pipe.max_payload();
    u32::try_from(len)
        .ok()
        .filter(|_| len <= max)
        .ok_or(FrameError::TooLarge { len, max })
}

/// Reads the next record; `None` at a clean end of the recording.
///
/// # Errors
/// Fails on a read error, an unknown pipe or direction byte, an over-cap length, or a record cut
/// short.
pub fn read(reader: &mut impl Read) -> Result<Option<Record>, FrameError> {
    let mut kind = [0; 2];
    // Only an end before the first byte is clean; anything later is a cut record.
    match reader.read(&mut kind[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut kind[1..]).map_err(truncated)?;
    let pipe = match kind[0] {
        0 => Pipe::Control,
        1 => Pipe::Data,
        _ => {
            return Err(FrameError::Malformed(
                postcard::Error::DeserializeBadEncoding,
            ));
        }
    };
    let direction = match kind[1] {
        0 => Direction::ToBrain,
        1 => Direction::ToShell,
        _ => {
            return Err(FrameError::Malformed(
                postcard::Error::DeserializeBadEncoding,
            ));
        }
    };
    let mut header = [0; HEADER_LEN];
    reader.read_exact(&mut header).map_err(truncated)?;
    let mut payload = vec![0; payload_len(header, pipe)?];
    reader.read_exact(&mut payload).map_err(truncated)?;
    Ok(Some(Record {
        pipe,
        direction,
        payload,
    }))
}

fn truncated(error: io::Error) -> FrameError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Record, read, write};
    use crate::Secret;
    use crate::frame::{FrameError, HEADER_LEN, Pipe, encode};
    use crate::message::{ControlToBrain, ControlToShell, Hello, Incarnation};

    fn payload(message: &impl serde::Serialize, pipe: Pipe) -> Vec<u8> {
        encode(message, pipe).expect("encodes")[HEADER_LEN..].to_vec()
    }

    #[test]
    fn records_round_trip_and_describe_with_redaction() {
        let hello = Hello {
            build_id: "0.1.0+abc".to_owned(),
            capability: Secret::new([9; 32]),
        };
        let records = [
            Record {
                pipe: Pipe::Control,
                direction: Direction::ToBrain,
                payload: payload(&ControlToBrain::Hello(hello.redacted()), Pipe::Control),
            },
            Record {
                pipe: Pipe::Control,
                direction: Direction::ToShell,
                payload: payload(
                    &ControlToShell::Welcome {
                        incarnation: Incarnation(7),
                    },
                    Pipe::Control,
                ),
            },
        ];
        let mut file = Vec::new();
        for record in &records {
            write(&mut file, record).expect("writes");
        }
        let mut reader = file.as_slice();
        let first = read(&mut reader).expect("first").expect("present");
        let second = read(&mut reader).expect("second").expect("present");
        assert_eq!([first.clone(), second.clone()], records);
        assert!(
            read(&mut reader).expect("clean end").is_none(),
            "nothing after two records"
        );
        assert_eq!(
            first.describe(),
            r#"Hello(Hello { build_id: "0.1.0+abc", capability: *** })"#
        );
        assert_eq!(second.describe(), "Welcome { incarnation: Incarnation(7) }");
        assert!(
            !first.payload.windows(32).any(|w| w == [9; 32]),
            "the capability was recorded"
        );
    }

    #[test]
    fn cut_and_corrupt_recordings_are_errors() {
        let record = Record {
            pipe: Pipe::Control,
            direction: Direction::ToBrain,
            payload: vec![1],
        };
        let mut file = Vec::new();
        write(&mut file, &record).expect("writes");
        assert!(
            matches!(read(&mut &file[..1]), Err(FrameError::Truncated)),
            "one byte"
        );
        assert!(matches!(read(&mut &file[..3]), Err(FrameError::Truncated)));
        assert!(matches!(
            read(&mut &file[..file.len() - 1]),
            Err(FrameError::Truncated)
        ));
        file[0] = 7;
        assert!(matches!(
            read(&mut file.as_slice()),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn undecodable_payloads_are_described_not_dropped() {
        let record = Record {
            pipe: Pipe::Control,
            direction: Direction::ToShell,
            payload: vec![42],
        };
        assert!(
            record.describe().starts_with("undecodable 1 bytes"),
            "{}",
            record.describe()
        );
    }
}
