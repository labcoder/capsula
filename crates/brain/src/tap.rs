//! The dev-only pipe tap. Debug builds record frames when [`PIPE_TAP_ENV`] names a file; release
//! builds never record.

use std::fs::File;

use capsula_protocol::constants::PIPE_TAP_ENV;
use capsula_protocol::frame::{self, HEADER_LEN, Pipe};
use capsula_protocol::tap::{self as record, Direction, Record};
use serde::Serialize;

/// An open recording, or nothing.
#[derive(Debug)]
pub(crate) struct Tap(Option<File>);

impl Tap {
    /// Opens the file named by [`PIPE_TAP_ENV`], in debug builds only.
    pub(crate) fn from_env() -> Self {
        if !cfg!(debug_assertions) {
            return Self(None);
        }
        let Some(path) = std::env::var_os(PIPE_TAP_ENV) else {
            return Self(None);
        };
        match File::options().create(true).append(true).open(&path) {
            Ok(file) => {
                tracing::info!(path = %path.to_string_lossy(), "recording pipe frames");
                Self(Some(file))
            }
            Err(error) => {
                tracing::warn!(%error, "can't open the pipe tap");
                Self(None)
            }
        }
    }

    /// Records one message. Callers pass a `Hello` already redacted.
    pub(crate) fn record(&mut self, pipe: Pipe, direction: Direction, message: &impl Serialize) {
        let Some(file) = &mut self.0 else { return };
        let written = frame::encode(message, pipe).and_then(|frame| {
            let payload = frame[HEADER_LEN..].to_vec();
            record::write(
                file,
                &Record {
                    pipe,
                    direction,
                    payload,
                },
            )
        });
        if let Err(error) = written {
            tracing::warn!(%error, "pipe tap write failed; recording stopped");
            self.0 = None;
        }
    }
}
