//! The dev-only pipe tap. Debug builds record control frames when [`PIPE_TAP_ENV`] names a
//! file that doesn't exist yet; release builds never record. What a record keeps is decided in
//! `kumochi_protocol::tap` from the message itself.

use std::fs::File;
use std::io::Write as _;

use kumochi_protocol::constants::PIPE_TAP_ENV;
use kumochi_protocol::frame::Pipe;
use kumochi_protocol::tap::{self as record, Direction, Record, Tapped};

/// An open recording, or nothing.
#[derive(Debug)]
pub(crate) struct Tap(Option<File>);

impl Tap {
    /// Starts a new recording at the file named by [`PIPE_TAP_ENV`], in debug builds only. An
    /// existing file is left untouched and nothing is recorded, so a recording never mixes
    /// sessions or formats.
    pub(crate) fn from_env() -> Self {
        if !cfg!(debug_assertions) {
            return Self(None);
        }
        let Some(path) = std::env::var_os(PIPE_TAP_ENV) else {
            return Self(None);
        };
        let opened = File::create_new(&path).and_then(|mut file| {
            record::write_header(&mut file)?;
            Ok(file)
        });
        match opened {
            Ok(file) => {
                tracing::info!(path = %path.to_string_lossy(), "recording pipe frames");
                Self(Some(file))
            }
            Err(error) => {
                tracing::warn!(%error, "can't start the pipe tap; it needs a new file");
                Self(None)
            }
        }
    }

    /// Records into a new `file`, whatever the build.
    #[cfg(test)]
    pub(crate) fn to_file(mut file: File) -> Self {
        record::write_header(&mut file).expect("the test file is writable");
        Self(Some(file))
    }

    /// Records one message that travelled as a `wire_len`-byte payload.
    pub(crate) fn record(
        &mut self,
        pipe: Pipe,
        direction: Direction,
        message: &impl Tapped,
        wire_len: usize,
    ) {
        let Some(file) = &mut self.0 else { return };
        let written = Record::of(pipe, direction, message, wire_len)
            .and_then(|record| record::write(file, &record))
            .and_then(|()| file.flush().map_err(Into::into));
        if let Err(error) = written {
            tracing::warn!(%error, "pipe tap write failed; recording stopped");
            self.0 = None;
        }
    }
}
