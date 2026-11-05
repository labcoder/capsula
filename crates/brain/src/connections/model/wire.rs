//! HTTP/1.1 on the model connection: the request's bytes, the response head and the body's
//! framing. Every length is bounded and checked, and none sizes an allocation.

#[cfg(test)]
mod tests;

use zeroize::Zeroizing;

use super::error::Failure;

/// The longest response head, its blank line included.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// The most headers one response head carries.
pub const MAX_HEADERS: usize = 32;
/// The most interim heads (100 or 103) before the final one.
pub const MAX_INTERIM_HEADS: usize = 4;
/// The longest chunk-size line, extensions included and its CRLF excluded.
pub const MAX_CHUNK_LINE_BYTES: usize = 1024;
/// The most trailer bytes after the last chunk, CRLFs included.
pub const MAX_TRAILER_BYTES: usize = 16 * 1024;

/// The request: its line, headers and body in one buffer, wiped when it drops.
///
/// # Errors
/// [`Failure::Credential`] when the bearer isn't a valid header value.
pub fn request(bearer: &str, body: &[u8]) -> Result<Zeroizing<Vec<u8>>, Failure> {
    if bearer.is_empty() || !bearer.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Failure::Credential);
    }
    let length = body.len().to_string();
    let parts: [&[u8]; 6] = [
        b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer ",
        bearer.as_bytes(),
        b"\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\n\
          Accept-Encoding: identity\r\nConnection: keep-alive\r\nContent-Length: ",
        length.as_bytes(),
        b"\r\n\r\n",
        body,
    ];
    // Sized once, so no reallocation leaves an unwiped copy behind.
    let mut bytes = Zeroizing::new(Vec::with_capacity(
        parts.iter().map(|part| part.len()).sum(),
    ));
    for part in parts {
        bytes.extend_from_slice(part);
    }
    Ok(bytes)
}

/// How a response body ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// After this many bytes.
    Length(u64),
    /// At the last chunk and its trailers.
    Chunked,
    /// When the server closes the connection.
    Close,
}

/// A response head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// The status; 100 and 103 are interim heads, which have no body.
    pub status: u16,
    /// How the body ends.
    pub framing: Framing,
    /// `Connection: close`, so the connection isn't reused.
    pub close: bool,
    /// What the Content-Type says the body is.
    pub media: Media,
}

/// What a response head's Content-Type says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    /// There's no Content-Type.
    Absent,
    /// `text/event-stream`.
    EventStream,
    /// Another type, or more than one Content-Type.
    Other,
}

/// The head at the start of `bytes` and its length, or `None` while it's incomplete.
///
/// # Errors
/// [`Failure::Protocol`] for anything but a bounded HTTP/1.1 head with one clear framing and no
/// content coding.
pub fn head(bytes: &[u8]) -> Result<Option<(Head, usize)>, Failure> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    let length = match response.parse(bytes) {
        Ok(httparse::Status::Complete(length)) if length <= MAX_HEAD_BYTES => length,
        Ok(httparse::Status::Partial) if bytes.len() < MAX_HEAD_BYTES => return Ok(None),
        _ => return Err(Failure::Protocol),
    };
    let status = response.code.ok_or(Failure::Protocol)?;
    if response.version != Some(1) || !(100..600).contains(&status) {
        return Err(Failure::Protocol);
    }
    let mut head = Head {
        status,
        framing: Framing::Length(0),
        close: false,
        media: Media::Absent,
    };
    match status {
        100 | 103 => return Ok(Some((head, length))),
        ..200 => return Err(Failure::Protocol),
        _ => {}
    }
    let (mut content_length, mut chunked) = (None, false);
    for header in response.headers.iter() {
        let value = header.value.trim_ascii();
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(Failure::Protocol);
            }
            content_length = Some(decimal(value)?);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case(b"chunked") {
                return Err(Failure::Protocol);
            }
            chunked = true;
        } else if header.name.eq_ignore_ascii_case("connection") {
            head.close |= value
                .split(|&byte| byte == b',')
                .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"close"));
        } else if header.name.eq_ignore_ascii_case("content-encoding") {
            if !value.eq_ignore_ascii_case(b"identity") {
                return Err(Failure::Protocol);
            }
        } else if header.name.eq_ignore_ascii_case("content-type") {
            let media = value.split(|&byte| byte == b';').next().unwrap_or_default();
            head.media = match head.media {
                Media::Absent
                    if media
                        .trim_ascii()
                        .eq_ignore_ascii_case(b"text/event-stream") =>
                {
                    Media::EventStream
                }
                _ => Media::Other,
            };
        }
    }
    head.framing = match (content_length, chunked) {
        (Some(length), false) => Framing::Length(length),
        (None, true) => Framing::Chunked,
        (None, false) if head.close => Framing::Close,
        _ => return Err(Failure::Protocol),
    };
    Ok(Some((head, length)))
}

fn decimal(value: &[u8]) -> Result<u64, Failure> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(Failure::Protocol);
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(Failure::Protocol)
}

/// A response body's framing: it consumes the framing bytes and hands out the data between them.
#[derive(Debug)]
pub struct Body(State);

#[derive(Debug, Clone, Copy)]
enum State {
    Length(u64),
    Close,
    Size,
    Data(u64),
    DataEnd,
    Trailers(usize),
    Done,
}

/// What [`Body::step`] found at the front of the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// This many bytes of data.
    Data(usize),
    /// This many framing bytes, to skip.
    Framing(usize),
    /// More bytes are needed.
    More,
    /// The body has ended; any bytes after it aren't this response's.
    Done,
}

impl Body {
    /// A body framed as `framing`.
    #[must_use]
    pub fn new(framing: Framing) -> Self {
        Self(match framing {
            Framing::Length(0) => State::Done,
            Framing::Length(length) => State::Length(length),
            Framing::Chunked => State::Size,
            Framing::Close => State::Close,
        })
    }

    /// The connection ended while [`Body::step`] wanted more.
    ///
    /// # Errors
    /// [`Failure::Truncated`], unless the body ends when the connection does.
    pub fn end(&mut self) -> Result<(), Failure> {
        match self.0 {
            State::Close | State::Done => {
                self.0 = State::Done;
                Ok(())
            }
            _ => Err(Failure::Truncated),
        }
    }

    /// Reads the front of `bytes`.
    ///
    /// # Errors
    /// [`Failure::Protocol`] for framing that's malformed or past a bound.
    pub fn step(&mut self, bytes: &[u8]) -> Result<Step, Failure> {
        let more = Ok(Step::More);
        match self.0 {
            State::Done => Ok(Step::Done),
            State::Length(_) | State::Data(_) | State::Close if bytes.is_empty() => more,
            State::Close => Ok(Step::Data(bytes.len())),
            State::Length(left) | State::Data(left) => {
                let available = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let taken = left.min(available);
                let left = left - taken;
                self.0 = match (self.0, left) {
                    (State::Length(_), 0) => State::Done,
                    (State::Length(_), left) => State::Length(left),
                    (_, 0) => State::DataEnd,
                    (_, left) => State::Data(left),
                };
                Ok(Step::Data(usize::try_from(taken).unwrap_or(bytes.len())))
            }
            State::DataEnd => match bytes {
                [b'\r', b'\n', ..] => {
                    self.0 = State::Size;
                    Ok(Step::Framing(2))
                }
                [] | [b'\r'] => more,
                _ => Err(Failure::Protocol),
            },
            State::Size => {
                let Some(line) = line(bytes, MAX_CHUNK_LINE_BYTES)? else {
                    return more;
                };
                let size = chunk_size(&bytes[..line])?;
                self.0 = if size == 0 {
                    State::Trailers(0)
                } else {
                    State::Data(size)
                };
                Ok(Step::Framing(line + 2))
            }
            State::Trailers(seen) => {
                let cap = MAX_TRAILER_BYTES
                    .checked_sub(seen + 2)
                    .ok_or(Failure::Protocol)?;
                let Some(line) = line(bytes, cap)? else {
                    return more;
                };
                if line > 0 {
                    field_line(&bytes[..line])?;
                }
                self.0 = if line == 0 {
                    State::Done
                } else {
                    State::Trailers(seen + line + 2)
                };
                Ok(Step::Framing(line + 2))
            }
        }
    }
}

/// The length of the CRLF-ended line at the front of `bytes`, which may be at most `cap` long.
fn line(bytes: &[u8], cap: usize) -> Result<Option<usize>, Failure> {
    let window = &bytes[..bytes.len().min(cap + 2)];
    match window.iter().position(|&byte| byte == b'\n') {
        Some(end) if end > 0 && window[end - 1] == b'\r' => Ok(Some(end - 1)),
        Some(_) => Err(Failure::Protocol),
        None if window.len() == cap + 2 => Err(Failure::Protocol),
        None => Ok(None),
    }
}

/// A chunk-size line's size. Its extensions are checked against RFC 9112's grammar, then ignored.
fn chunk_size(line: &[u8]) -> Result<u64, Failure> {
    let end = line
        .iter()
        .position(|byte| !byte.is_ascii_hexdigit())
        .unwrap_or(line.len());
    let (digits, mut extensions) = line.split_at(end);
    if digits.is_empty() || digits.len() > 16 {
        return Err(Failure::Protocol);
    }
    while !extensions.is_empty() {
        let rest = blank(extensions)
            .strip_prefix(b";")
            .ok_or(Failure::Protocol)?;
        let (_, rest) = token(blank(rest))?;
        extensions = match blank(rest).strip_prefix(b"=") {
            Some(value) => extension_value(blank(value))?,
            None => rest,
        };
    }
    std::str::from_utf8(digits)
        .ok()
        .and_then(|digits| u64::from_str_radix(digits, 16).ok())
        .ok_or(Failure::Protocol)
}

/// What's left after a chunk extension's value: a token or a quoted string.
fn extension_value(bytes: &[u8]) -> Result<&[u8], Failure> {
    let Some(mut rest) = bytes.strip_prefix(b"\"") else {
        return token(bytes).map(|(_, rest)| rest);
    };
    loop {
        match rest {
            [b'"', rest @ ..] => return Ok(rest),
            [b'\\', escaped, rest_after @ ..] if text(*escaped) => rest = rest_after,
            [byte, rest_after @ ..] if text(*byte) && *byte != b'\\' => rest = rest_after,
            _ => return Err(Failure::Protocol),
        }
    }
}

/// A trailer line must be a field line: a token name, a colon and a visible or blank value.
fn field_line(line: &[u8]) -> Result<(), Failure> {
    let (_, rest) = token(line)?;
    let value = rest.strip_prefix(b":").ok_or(Failure::Protocol)?;
    if !value.iter().all(|&byte| text(byte)) {
        return Err(Failure::Protocol);
    }
    Ok(())
}

/// The non-empty token at the front of `bytes`, and what follows it.
fn token(bytes: &[u8]) -> Result<(&[u8], &[u8]), Failure> {
    let end = bytes
        .iter()
        .position(|&byte| !(byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)))
        .unwrap_or(bytes.len());
    if end == 0 {
        return Err(Failure::Protocol);
    }
    Ok(bytes.split_at(end))
}

/// A space, tab, visible ASCII or obs-text byte: what a field value or quoted text may hold.
fn text(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x21..=0x7e | 0x80..=0xff)
}

fn blank(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(bytes.len());
    &bytes[end..]
}
