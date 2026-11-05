//! A bounded server-sent-events framer: bytes in, each event's joined `data` out. It knows no
//! JSON, no `[DONE]` and no terminal event; the provider's reader decides what each event means
//! and when the stream ends.

#[cfg(test)]
mod tests;

use std::fmt;
use std::mem;
use std::ops::ControlFlow;

/// The longest line, its line feed excluded.
pub(in crate::connections) const MAX_LINE_BYTES: usize = 1024 * 1024;
/// The most data one event carries, its `data` lines joined by line feeds.
pub(in crate::connections) const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// The most bytes one stream carries, up to the event that stops it.
pub(in crate::connections) const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// A stream passed one of the framer's caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connections) struct Limit;

/// Splits one stream into events. Lines end in LF or CRLF; comments, `event`, `id` and `retry`
/// are ignored, and each blank line hands out the event's `data` lines, joined by line feeds.
#[derive(Default)]
pub(in crate::connections) struct Framer {
    /// The line so far, when a chunk ended inside it.
    line: Vec<u8>,
    /// The event's data so far.
    data: Vec<u8>,
    /// The event has a `data` line, possibly empty.
    has_data: bool,
    received: usize,
}

impl fmt::Debug for Framer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Framer")
            .field("received", &self.received)
            .finish_non_exhaustive()
    }
}

impl Framer {
    /// The bytes counted so far.
    pub(in crate::connections) fn received(&self) -> usize {
        self.received
    }

    /// Reads the stream's next bytes, handing each event's data to `on_data` until it breaks.
    /// The bytes after a break are neither counted nor buffered.
    ///
    /// # Errors
    /// [`Limit`] once a cap is passed, or the first error `on_data` returns.
    pub(in crate::connections) fn feed<E>(
        &mut self,
        mut bytes: &[u8],
        on_data: &mut impl FnMut(&[u8]) -> Result<ControlFlow<()>, E>,
    ) -> Result<ControlFlow<()>, E>
    where
        E: From<Limit>,
    {
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            let (head, rest) = bytes.split_at(end);
            bytes = &rest[1..];
            self.consume(end + 1)?;
            let flow = if self.line.is_empty() {
                self.line_ended(head, on_data)?
            } else {
                let mut line = mem::take(&mut self.line);
                if line.len() + head.len() > MAX_LINE_BYTES {
                    return Err(Limit.into());
                }
                line.extend_from_slice(head);
                let flow = self.line_ended(&line, on_data);
                line.clear();
                self.line = line;
                flow?
            };
            if flow.is_break() {
                return Ok(flow);
            }
        }
        self.consume(bytes.len())?;
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            return Err(Limit.into());
        }
        self.line.extend_from_slice(bytes);
        Ok(ControlFlow::Continue(()))
    }

    /// Counts bytes read, so the stream cap doesn't depend on what follows the event that stops it.
    fn consume(&mut self, bytes: usize) -> Result<(), Limit> {
        self.received = self
            .received
            .checked_add(bytes)
            .filter(|received| *received <= MAX_STREAM_BYTES)
            .ok_or(Limit)?;
        Ok(())
    }

    fn line_ended<E>(
        &mut self,
        line: &[u8],
        on_data: &mut impl FnMut(&[u8]) -> Result<ControlFlow<()>, E>,
    ) -> Result<ControlFlow<()>, E>
    where
        E: From<Limit>,
    {
        if line.len() > MAX_LINE_BYTES {
            return Err(Limit.into());
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !mem::take(&mut self.has_data) {
                return Ok(ControlFlow::Continue(()));
            }
            let flow = on_data(&self.data);
            self.data.clear();
            return flow;
        }
        let (field, value) = match line.iter().position(|&byte| byte == b':') {
            Some(0) => return Ok(ControlFlow::Continue(())),
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        // The event's data names it, so `event`, `id` and `retry` are ignored.
        if field == b"data" {
            let joined = usize::from(self.has_data) + value.len();
            if self.data.len() + joined > MAX_EVENT_BYTES {
                return Err(Limit.into());
            }
            if self.has_data {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            self.has_data = true;
        }
        Ok(ControlFlow::Continue(()))
    }
}
