//! Cuts a run's streamed answer into sentences to speak. No I/O: the run's `Reply` feeds it each
//! text delta and takes the finished cuts.
//!
//! A segment ends at a sentence end (`.`, `!` or `?` followed by whitespace), at a clause end
//! (`,`, `;` or `:` followed by whitespace) once it's past [`CLAUSE_CHARS`], or by a hard cut
//! past [`MAX_CHARS`]. A punctuation mark at the end of a delta waits for the next character, so
//! `"3."` then `"5"` stays `3.5`.

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;

/// Past this many characters a clause end also ends a segment [S5].
pub(crate) const CLAUSE_CHARS: usize = 120;
/// The most characters in one segment [S5].
pub(crate) const MAX_CHARS: usize = 240;

/// One run's segmenter.
#[derive(Default)]
pub(crate) struct Segmenter {
    /// Text since the last cut.
    pending: String,
    /// Where `pending` starts in the run's answer, in bytes.
    start: usize,
    /// The producer finished; later text is ignored.
    finished: bool,
}

/// A segment of the answer: its byte range in the answer and its text.
pub(crate) struct Cut {
    pub range: Range<usize>,
    pub text: String,
}

impl Segmenter {
    /// Adds a delta and appends every segment it completes to `out`.
    pub(crate) fn push(&mut self, delta: &str, out: &mut Vec<Cut>) {
        if self.finished {
            return;
        }
        self.pending.push_str(delta);
        let mut from = 0;
        while let Some(len) = cut(&self.pending[from..]) {
            self.emit(from, from + len, out);
            from += len;
        }
        self.pending.drain(..from);
        self.start += from;
    }

    /// Ends a segment at the current end of the text, as before a step's `say`.
    pub(crate) fn flush(&mut self, out: &mut Vec<Cut>) {
        let len = self.pending.len();
        self.emit(0, len, out);
        self.pending.clear();
        self.start += len;
    }

    /// The producer finished: flushes the tail once. A second call does nothing.
    pub(crate) fn finish(&mut self, out: &mut Vec<Cut>) {
        if !self.finished {
            self.flush(out);
            self.finished = true;
        }
    }

    /// Text that's only whitespace is consumed but never spoken.
    fn emit(&self, from: usize, to: usize, out: &mut Vec<Cut>) {
        let text = &self.pending[from..to];
        if !text.trim().is_empty() {
            out.push(Cut {
                range: self.start + from..self.start + to,
                text: text.to_owned(),
            });
        }
    }
}

/// The byte length of the first complete segment of `text`, if it has one yet.
fn cut(text: &str) -> Option<usize> {
    let mut last_space = None;
    let mut chars = text.char_indices().enumerate().peekable();
    while let Some((n, (at, ch))) = chars.next() {
        if n == MAX_CHARS {
            return Some(last_space.unwrap_or(at));
        }
        let end = at + ch.len_utf8();
        if ch.is_whitespace() {
            last_space = Some(end);
            continue;
        }
        let ends =
            matches!(ch, '.' | '!' | '?') || (n >= CLAUSE_CHARS && matches!(ch, ',' | ';' | ':'));
        if ends
            && chars
                .peek()
                .is_some_and(|(_, (_, next))| next.is_whitespace())
        {
            return Some(end);
        }
    }
    None
}

impl fmt::Debug for Segmenter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Segmenter")
            .field("pending", &self.pending.len())
            .field("start", &self.start)
            .field("finished", &self.finished)
            .finish()
    }
}

impl fmt::Debug for Cut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cut")
            .field("range", &self.range)
            .field("text", &self.text.len())
            .finish()
    }
}
