//! A text-only Chat Completions request: what it may hold, and its bytes on the wire.

use std::fmt;

use kumochi_protocol::Secret;
use kumochi_protocol::message::ModelSlug;
use serde::Serialize;
use zeroize::Zeroizing;

use super::Failure;
use super::endpoint::LoopbackEndpoint;
use crate::connections::model::{MAX_REQUEST_BYTES, serialized_len};

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The app's instructions.
    System,
    /// The user.
    User,
    /// The model.
    Assistant,
}

/// One plain text message; nothing else can be expressed.
#[derive(Clone, Copy, Serialize)]
pub struct ChatMessage<'a> {
    /// Who wrote it.
    pub role: Role,
    /// Its text.
    #[serde(rename = "content")]
    pub text: &'a str,
}

impl fmt::Debug for ChatMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatMessage")
            .field("role", &self.role)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

#[derive(Serialize)]
struct Wire<'a> {
    model: &'a str,
    messages: &'a [ChatMessage<'a>],
    stream: bool,
}

/// A request's JSON, checked before any connection is made.
pub struct ChatBody(Secret<Zeroizing<Vec<u8>>>);

impl ChatBody {
    /// The streamed request for `model` with `messages`.
    ///
    /// # Errors
    /// [`Failure::Request`] when there are no messages or the JSON passes [`MAX_REQUEST_BYTES`].
    pub fn new(model: &ModelSlug, messages: &[ChatMessage<'_>]) -> Result<Self, Failure> {
        let wire = Wire {
            model: model.as_str(),
            messages,
            stream: true,
        };
        let len = serialized_len(&wire);
        if messages.is_empty() || len > MAX_REQUEST_BYTES {
            return Err(Failure::Request);
        }
        // Sized first, so the buffer never grows and leaves a copy behind.
        let mut bytes = Zeroizing::new(Vec::with_capacity(len));
        serde_json::to_writer(&mut *bytes, &wire).map_err(|_| Failure::Request)?;
        Ok(Self(Secret::new(bytes)))
    }

    /// The whole request to `endpoint`: fixed text, the endpoint's own authority and the body.
    /// It has no credential of any kind.
    pub(super) fn request(&self, endpoint: LoopbackEndpoint) -> Zeroizing<Vec<u8>> {
        let body = self.0.expose();
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Accept: text/event-stream\r\nAccept-Encoding: identity\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n",
            endpoint.authority(),
            body.len()
        );
        let mut bytes = Zeroizing::new(Vec::with_capacity(head.len() + body.len()));
        bytes.extend_from_slice(head.as_bytes());
        bytes.extend_from_slice(body);
        bytes
    }
}

impl fmt::Debug for ChatBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatBody")
            .field("bytes", &self.0.expose().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelSlug {
        ModelSlug::new("qwen3-4b").expect("slug")
    }

    #[test]
    fn the_request_is_fixed_text_the_endpoint_authority_and_the_json() {
        let body = ChatBody::new(
            &model(),
            &[
                ChatMessage {
                    role: Role::System,
                    text: "Be brief.",
                },
                ChatMessage {
                    role: Role::User,
                    text: "Hi \"there\"\r\nHost: evil",
                },
            ],
        )
        .expect("body");
        let json = br#"{"model":"qwen3-4b","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi \"there\"\r\nHost: evil"}],"stream":true}"#;
        for (endpoint, host) in [
            ("127.0.0.1:8080", "127.0.0.1:8080"),
            ("[::1]:1234", "[::1]:1234"),
        ] {
            let mut expected = format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\n\
                 Content-Type: application/json\r\nAccept: text/event-stream\r\n\
                 Accept-Encoding: identity\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                json.len()
            )
            .into_bytes();
            expected.extend_from_slice(json);
            assert_eq!(
                *body.request(LoopbackEndpoint::for_tests(endpoint)),
                expected
            );
        }
    }

    #[test]
    fn an_empty_or_oversized_request_is_refused_before_any_connection() {
        assert!(matches!(
            ChatBody::new(&model(), &[]),
            Err(Failure::Request)
        ));
        let model = model();
        let empty = Wire {
            model: model.as_str(),
            messages: &[ChatMessage {
                role: Role::User,
                text: "",
            }],
            stream: true,
        };
        let room = MAX_REQUEST_BYTES - serialized_len(&empty);
        for (len, fits) in [(room, true), (room + 1, false)] {
            let text = "a".repeat(len);
            let body = ChatBody::new(
                &model,
                &[ChatMessage {
                    role: Role::User,
                    text: &text,
                }],
            );
            assert_eq!(
                body.map(|body| body.0.expose().len()).ok(),
                fits.then_some(MAX_REQUEST_BYTES)
            );
        }
    }

    #[test]
    fn debug_never_prints_message_text() {
        const MARK: &str = "sentinel-7f3a";
        let message = ChatMessage {
            role: Role::User,
            text: MARK,
        };
        let body = ChatBody::new(&model(), &[message]).expect("body");
        let bytes = format!("{:?}", MARK.as_bytes());
        let list = bytes.trim_matches(['[', ']']);
        let request = body.request(LoopbackEndpoint::for_tests("127.0.0.1:8080"));
        for printed in [
            format!("{message:?}"),
            format!("{body:?}"),
            format!("{:?}", [message]),
            format!("{request:?}"),
        ] {
            assert!(
                !printed.contains(MARK) && !printed.contains(list),
                "{printed}"
            );
        }
    }
}
