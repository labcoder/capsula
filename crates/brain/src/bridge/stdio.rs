//! Stdin lines, bounded, and what each JSON-RPC message is; the reply lines written back.

use std::fmt;

use serde_json::Value;

use super::MAX_INCOMING;
use super::id::JsonRpcId;
use super::wire::Method;

/// One stdin line, without its newline or a carriage return before it.
#[derive(PartialEq, Eq)]
pub(crate) enum Line {
    Text(Vec<u8>),
    /// Over [`MAX_INCOMING`]; the rest of it up to the next newline is dropped.
    TooLong,
}

/// Splits stdin into lines, never holding more than [`MAX_INCOMING`] bytes of one.
#[derive(Default)]
pub(crate) struct LineReader {
    buf: Vec<u8>,
    discarding: bool,
}

impl LineReader {
    /// Takes the next bytes read and appends each line they complete to `out`.
    pub(crate) fn push(&mut self, mut bytes: &[u8], out: &mut Vec<Line>) {
        while !bytes.is_empty() {
            let end = bytes.iter().position(|&byte| byte == b'\n');
            let part = &bytes[..end.unwrap_or(bytes.len())];
            if !self.discarding {
                if self.buf.len() + part.len() > MAX_INCOMING {
                    self.buf.clear();
                    self.discarding = true;
                    out.push(Line::TooLong);
                } else {
                    self.buf.extend_from_slice(part);
                }
            }
            let Some(end) = end else { return };
            if !std::mem::take(&mut self.discarding) {
                out.push(Line::Text(take_line(&mut self.buf)));
            }
            bytes = &bytes[end + 1..];
        }
    }

    /// At EOF: a last line with no newline.
    pub(crate) fn finish(&mut self) -> Option<Line> {
        (!self.discarding && !self.buf.is_empty()).then(|| Line::Text(take_line(&mut self.buf)))
    }
}

fn take_line(buf: &mut Vec<u8>) -> Vec<u8> {
    let mut line = std::mem::take(buf);
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    line
}

/// What one line is.
#[derive(PartialEq)]
pub(crate) enum Incoming {
    /// Empty or whitespace only: ignored.
    Blank,
    /// A request the brain answers, or `ping`, which the bridge answers.
    Request {
        id: JsonRpcId,
        method: Method,
        params: Vec<u8>,
    },
    /// No id: nothing is answered. Only `notifications/initialized` is relayed.
    Notification { initialized: bool },
    /// Answered with this error, and the id when there's a valid one.
    Invalid {
        id: Option<JsonRpcId>,
        error: RpcError,
    },
}

// Debug prints lengths only: lines and params are the client's text and tool arguments.
impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f.debug_struct("Text").field("bytes", &text.len()).finish(),
            Self::TooLong => f.write_str("TooLong"),
        }
    }
}

impl fmt::Debug for Incoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => f.write_str("Blank"),
            Self::Request { id, method, params } => f
                .debug_struct("Request")
                .field("id", id)
                .field("method", method)
                .field("params", &params.len())
                .finish(),
            Self::Notification { initialized } => f
                .debug_struct("Notification")
                .field("initialized", initialized)
                .finish(),
            Self::Invalid { id, error } => f
                .debug_struct("Invalid")
                .field("id", id)
                .field("error", error)
                .finish(),
        }
    }
}

/// The JSON-RPC errors the bridge answers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcError {
    Parse,
    InvalidRequest,
    MethodNotFound,
    Internal,
}

impl RpcError {
    const fn json(self) -> &'static str {
        match self {
            Self::Parse => r#"{"code":-32700,"message":"Parse error"}"#,
            Self::InvalidRequest => r#"{"code":-32600,"message":"Invalid Request"}"#,
            Self::MethodNotFound => r#"{"code":-32601,"message":"Method not found"}"#,
            Self::Internal => r#"{"code":-32603,"message":"Internal error"}"#,
        }
    }
}

/// Classifies one stdin line. A batch, a non-object, a bad `jsonrpc` or a bad id is
/// `InvalidRequest`.
pub(crate) fn classify(line: &[u8]) -> Incoming {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Incoming::Blank;
    }
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return Incoming::Invalid {
            id: None,
            error: RpcError::Parse,
        };
    };
    let invalid = |id| Incoming::Invalid {
        id,
        error: RpcError::InvalidRequest,
    };
    let Value::Object(mut fields) = value else {
        return invalid(None);
    };
    let id = match fields.remove("id") {
        None => None,
        Some(value) => match JsonRpcId::from_value(&value) {
            Some(id) => Some(id),
            None => return invalid(None),
        },
    };
    let method = fields.get("method").and_then(Value::as_str);
    let (Some(method), Some("2.0")) = (method, fields.get("jsonrpc").and_then(Value::as_str))
    else {
        return invalid(id);
    };
    let Some(id) = id else {
        return Incoming::Notification {
            initialized: method == "notifications/initialized",
        };
    };
    let method = match method {
        "initialize" => Method::Initialize,
        "tools/list" => Method::ToolsList,
        "tools/call" => Method::ToolsCall,
        "ping" => Method::Ping,
        _ => {
            return Incoming::Invalid {
                id: Some(id),
                error: RpcError::MethodNotFound,
            };
        }
    };
    let params = fields.get("params").map_or_else(
        || b"null".to_vec(),
        |params| params.to_string().into_bytes(),
    );
    Incoming::Request { id, method, params }
}

/// A reply's body.
#[derive(Clone, Copy)]
pub(crate) enum Body<'a> {
    /// The brain's result JSON.
    Result(&'a [u8]),
    /// The brain's error JSON.
    Error(&'a [u8]),
    /// One of the bridge's own errors.
    Local(RpcError),
}

/// The reply line for `id` (`null` when there's none), newline included.
pub(crate) fn reply(id: Option<&JsonRpcId>, body: Body<'_>) -> Vec<u8> {
    let id = id.map_or_else(|| "null".to_owned(), JsonRpcId::json);
    let (field, json) = match body {
        Body::Result(json) => ("result", json),
        Body::Error(json) => ("error", json),
        Body::Local(error) => ("error", error.json().as_bytes()),
    };
    let mut line = format!(r#"{{"jsonrpc":"2.0","id":{id},"{field}":"#).into_bytes();
    line.extend_from_slice(json);
    line.extend_from_slice(b"}\n");
    line
}
