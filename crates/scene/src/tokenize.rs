//! A bounded cursor-pull tokenizer for one JSON drawing envelope.

mod lex;

use crate::limits::{MAX_DRAWING_BYTES, MAX_JSON_DEPTH, MAX_RECORD_BYTES};
use lex::{Lex, Number, Purpose, StringState};

/// Bytes of one closed record, valid until the next tokenizer call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClosedRecord<'a> {
    /// Zero-based array position.
    pub index: u32,
    /// The complete JSON object, without surrounding array whitespace.
    pub bytes: &'a [u8],
}

impl std::fmt::Debug for ClosedRecord<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClosedRecord")
            .field("index", &self.index)
            .field("bytes_len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// A terminal tokenizer failure, containing no drawing content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Invalid JSON or an envelope other than one records array.
    #[error("malformed drawing at byte {at}")]
    Malformed {
        /// Zero-based offset in the drawing.
        at: usize,
    },
    /// A record exceeded its byte allowance.
    #[error("record exceeds byte limit")]
    RecordTooLarge,
    /// The drawing exceeded its byte allowance.
    #[error("drawing exceeds byte limit")]
    DrawingTooLarge,
    /// Containers exceeded the JSON depth allowance.
    #[error("drawing exceeds nesting limit")]
    TooDeep,
    /// The stream ended before the envelope was complete.
    #[error("truncated drawing")]
    Truncated,
    /// Something other than whitespace followed the closed records array. The records before
    /// it were whole, and the bytes after it are never read as records.
    #[error("text after the records array")]
    AfterRecords,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Root,
    Key,
    Colon,
    Array,
    RecordOrEnd,
    Record,
    AfterRecord,
    AfterArray,
    Done,
}

#[derive(Debug, Clone, Copy)]
enum Container {
    ObjectKeyOrEnd,
    ObjectKey,
    ObjectColon,
    ObjectValue,
    ObjectAfterValue,
    ArrayValueOrEnd,
    ArrayValue,
    ArrayAfterValue,
}

/// Incrementally consumes one drawing, retaining only the current record's bytes.
///
/// After any error every call returns that same error. After an emission the caller
/// may parse and await persistence before requesting the next record.
pub struct Tokenizer {
    buffer: Vec<u8>,
    total: usize,
    phase: Phase,
    stack: [Container; MAX_JSON_DEPTH - 2],
    depth: usize,
    lex: Lex,
    key_len: usize,
    emitted: bool,
    index: u32,
    failed: Option<TokenError>,
}

impl std::fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokenizer")
            .field("buffer_len", &self.buffer.len())
            .field("total", &self.total)
            .field("index", &self.index)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer {
    /// Allocates the one bounded record buffer.
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(MAX_RECORD_BYTES),
            total: 0,
            phase: Phase::Root,
            stack: [Container::ObjectKeyOrEnd; MAX_JSON_DEPTH - 2],
            depth: 0,
            lex: Lex::None,
            key_len: 0,
            emitted: false,
            index: 0,
            failed: None,
        }
    }

    /// Consumes through at most one record, leaving the unconsumed delta in `input`.
    ///
    /// `None` means all supplied bytes were consumed; supply the next delta or call
    /// `finish` at the end of the stream. A record's borrowed bytes need not be copied.
    /// # Errors
    /// Returns a terminal syntax or resource-limit failure.
    pub fn next(&mut self, input: &mut &[u8]) -> Result<Option<ClosedRecord<'_>>, TokenError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.emitted {
            self.buffer.clear();
            self.emitted = false;
        }
        while let Some((&byte, rest)) = input.split_first() {
            if self.total == MAX_DRAWING_BYTES {
                return self.fail(TokenError::DrawingTooLarge);
            }
            self.total += 1;
            *input = rest;
            if self.depth > 0 {
                if self.buffer.len() == MAX_RECORD_BYTES {
                    return self.fail(TokenError::RecordTooLarge);
                }
                self.buffer.push(byte);
            }
            if let Err(error) = self.byte(byte) {
                return self.fail(error);
            }
            if self.emitted {
                let index = self.index;
                self.index += 1;
                return Ok(Some(ClosedRecord {
                    index,
                    bytes: &self.buffer,
                }));
            }
        }
        Ok(None)
    }

    /// Checks that the caller drained a complete envelope.
    /// # Errors
    /// Returns the prior terminal failure, or `Truncated` for incomplete input.
    pub fn finish(&mut self) -> Result<(), TokenError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase == Phase::Done {
            Ok(())
        } else {
            self.fail(TokenError::Truncated)
        }
    }

    fn fail<T>(&mut self, error: TokenError) -> Result<T, TokenError> {
        self.failed = Some(error);
        Err(error)
    }

    /// Only the envelope's closing brace may follow the records array, so any other byte there
    /// is text after the records rather than a malformed drawing.
    fn malformed(&self) -> TokenError {
        if matches!(self.phase, Phase::AfterArray | Phase::Done) {
            TokenError::AfterRecords
        } else {
            TokenError::Malformed { at: self.total - 1 }
        }
    }

    fn byte(&mut self, byte: u8) -> Result<(), TokenError> {
        loop {
            match self.lex {
                Lex::String(mut string) => {
                    let done = string
                        .byte(byte, &mut self.key_len)
                        .map_err(|()| self.malformed())?;
                    self.lex = if done { Lex::None } else { Lex::String(string) };
                    if done {
                        match string.purpose {
                            Purpose::RootKey => self.phase = Phase::Colon,
                            Purpose::Key => self.stack[self.depth - 1] = Container::ObjectColon,
                            Purpose::Value => self.value_finished(),
                        }
                    }
                    return Ok(());
                }
                Lex::Number(number) => {
                    if let Some(next) = number.byte(byte).map_err(|()| self.malformed())? {
                        self.lex = Lex::Number(next);
                        return Ok(());
                    }
                    self.lex = Lex::None;
                    self.value_finished();
                }
                Lex::Literal { word, offset } => {
                    if byte != word[usize::from(offset)] {
                        return Err(self.malformed());
                    }
                    if usize::from(offset) + 1 == word.len() {
                        self.lex = Lex::None;
                        self.value_finished();
                    } else {
                        self.lex = Lex::Literal {
                            word,
                            offset: offset + 1,
                        };
                    }
                    return Ok(());
                }
                Lex::None => break,
            }
        }
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            return Ok(());
        }
        if self.depth == 0 {
            return self.envelope(byte);
        }
        match (self.stack[self.depth - 1], byte) {
            (Container::ObjectKeyOrEnd | Container::ObjectAfterValue, b'}')
            | (Container::ArrayValueOrEnd | Container::ArrayAfterValue, b']') => self.close(),
            (Container::ObjectKeyOrEnd | Container::ObjectKey, b'"') => {
                self.lex = Lex::String(StringState::new(Purpose::Key));
            }
            (Container::ObjectColon, b':') => self.stack[self.depth - 1] = Container::ObjectValue,
            (Container::ObjectAfterValue, b',') => {
                self.stack[self.depth - 1] = Container::ObjectKey;
            }
            (Container::ArrayAfterValue, b',') => {
                self.stack[self.depth - 1] = Container::ArrayValue;
            }
            (Container::ObjectValue | Container::ArrayValueOrEnd | Container::ArrayValue, _) => {
                self.value(byte)?;
            }
            _ => return Err(self.malformed()),
        }
        Ok(())
    }

    fn envelope(&mut self, byte: u8) -> Result<(), TokenError> {
        self.phase = match (self.phase, byte) {
            (Phase::Root, b'{') => Phase::Key,
            (Phase::Key, b'"') => {
                self.key_len = 0;
                self.lex = Lex::String(StringState::new(Purpose::RootKey));
                Phase::Key
            }
            (Phase::Colon, b':') => Phase::Array,
            (Phase::Array, b'[') => Phase::RecordOrEnd,
            (Phase::RecordOrEnd | Phase::AfterRecord, b']') => Phase::AfterArray,
            (Phase::RecordOrEnd | Phase::Record, b'{') => {
                self.buffer.push(byte);
                self.open(Container::ObjectKeyOrEnd)?;
                Phase::Record
            }
            (Phase::AfterRecord, b',') => Phase::Record,
            (Phase::AfterArray, b'}') => Phase::Done,
            _ => return Err(self.malformed()),
        };
        Ok(())
    }

    fn open(&mut self, container: Container) -> Result<(), TokenError> {
        if self.depth == self.stack.len() {
            return Err(TokenError::TooDeep);
        }
        self.stack[self.depth] = container;
        self.depth += 1;
        Ok(())
    }

    fn close(&mut self) {
        self.depth -= 1;
        self.value_finished();
    }

    fn value_finished(&mut self) {
        if self.depth == 0 {
            self.phase = Phase::AfterRecord;
            self.emitted = true;
            return;
        }
        self.stack[self.depth - 1] = match self.stack[self.depth - 1] {
            Container::ObjectValue => Container::ObjectAfterValue,
            _ => Container::ArrayAfterValue,
        };
    }

    fn value(&mut self, byte: u8) -> Result<(), TokenError> {
        self.lex = match byte {
            b'{' => {
                self.open(Container::ObjectKeyOrEnd)?;
                Lex::None
            }
            b'[' => {
                self.open(Container::ArrayValueOrEnd)?;
                Lex::None
            }
            b'"' => Lex::String(StringState::new(Purpose::Value)),
            b'-' => Lex::Number(Number::Minus),
            b'0' => Lex::Number(Number::Zero),
            b'1'..=b'9' => Lex::Number(Number::Integer),
            b't' => Lex::Literal {
                word: b"true",
                offset: 1,
            },
            b'f' => Lex::Literal {
                word: b"false",
                offset: 1,
            },
            b'n' => Lex::Literal {
                word: b"null",
                offset: 1,
            },
            _ => return Err(self.malformed()),
        };
        Ok(())
    }
}
