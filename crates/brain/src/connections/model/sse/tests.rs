use kumochi_protocol::message::{Action, ErrorKey, UserError};
use serde_json::{Value, json};

use super::{MAX_TEXT_BYTES, Progress, Reader, SseEvent};
use crate::connections::model::error::{Code, Failure, ProviderError};

const TEXT: &[u8] = include_bytes!("../../../../fixtures/responses/text.sse");
const CRLF: &[u8] = include_bytes!("../../../../fixtures/responses/crlf.sse");
const FAILED: &[u8] = include_bytes!("../../../../fixtures/responses/failed_usage_limit.sse");
const ERROR: &[u8] = include_bytes!("../../../../fixtures/responses/error_event.sse");
const INCOMPLETE: &[u8] = include_bytes!("../../../../fixtures/responses/incomplete.sse");
const COMPLETED: &[u8] = b"data: {\"type\":\"response.completed\"}\n\n";

#[test]
fn anthropic_stop_does_not_complete_a_responses_stream() {
    let mut reader = Reader::default();
    let mut seen = Vec::new();
    assert_eq!(
        reader.feed(b"data: {\"type\":\"message_stop\"}\n\n", &mut |event| seen
            .push(event.kind().to_owned())),
        Ok(Progress::More)
    );
    assert_eq!(seen, ["message_stop"]);
    assert_eq!(
        reader.feed(COMPLETED, &mut |event| seen.push(event.kind().to_owned())),
        Ok(Progress::Completed)
    );
    assert_eq!(seen, ["message_stop", "response.completed"]);
}

/// The event kinds handed out, the streamed text, and how the stream ended.
type Read = (Vec<String>, String, Result<(), Failure>);

/// Feeds `bytes` in chunks of `size` until the stream ends.
fn read(bytes: &[u8], size: usize) -> Read {
    let mut reader = Reader::default();
    let mut kinds = Vec::new();
    let mut text = String::new();
    let end = {
        let mut on_event = |event: &SseEvent| {
            kinds.push(event.kind().to_owned());
            if event.kind() == "response.output_text.delta" {
                text.push_str(event.value()["delta"].as_str().expect("a delta"));
            }
        };
        let mut end = None;
        for chunk in bytes.chunks(size) {
            match reader.feed(chunk, &mut on_event) {
                Ok(Progress::More) => {}
                Ok(Progress::Completed) => {
                    end = Some(Ok(()));
                    break;
                }
                Err(failure) => {
                    end = Some(Err(failure));
                    break;
                }
            }
        }
        end.unwrap_or(Err(Failure::Truncated))
    };
    (kinds, text, end)
}

fn kinds(kinds: &[&str]) -> Vec<String> {
    kinds.iter().map(|kind| (*kind).to_owned()).collect()
}

/// A `data` event for `value`.
fn event(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

#[test]
fn a_stream_reads_the_same_whole_byte_by_byte_and_in_odd_chunks() {
    let expected = (
        kinds(&[
            "response.created",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.completed",
        ]),
        "Hello, w\u{f6}rld \u{1f642}".to_owned(),
        Ok(()),
    );
    // One byte at a time splits every event and the multi-byte characters.
    for size in [TEXT.len(), 1, 7] {
        assert_eq!(read(TEXT, size), expected, "chunks of {size}");
    }
}

#[test]
fn crlf_lines_comments_split_data_and_done_are_read() {
    assert_eq!(
        read(CRLF, 5),
        (
            kinds(&[
                "response.created",
                "response.output_text.delta",
                "response.completed"
            ]),
            "two lines".to_owned(),
            Ok(()),
        )
    );
}

#[test]
fn a_failed_response_after_text_is_its_provider_error() {
    let error = ProviderError {
        status: 200,
        code: Some(Code::UsageLimit),
        param: None,
    };
    assert_eq!(
        read(FAILED, 3),
        (
            kinds(&["response.created", "response.output_text.delta"]),
            "Partial".to_owned(),
            Err(Failure::Provider(error)),
        )
    );
    assert_eq!(
        Failure::Provider(error).user_error(),
        UserError::new(ErrorKey::UsageLimit, Vec::new(), vec![Action::ManageUsage]).expect("valid")
    );
}

#[test]
fn an_error_event_is_its_provider_error() {
    assert_eq!(
        read(ERROR, 64).2,
        Err(Failure::Provider(ProviderError {
            status: 200,
            code: Some(Code::Unavailable),
            param: None,
        }))
    );
}

#[test]
fn an_incomplete_response_and_an_early_end_fail() {
    assert_eq!(
        read(INCOMPLETE, 64),
        (
            kinds(&["response.created", "response.output_text.delta"]),
            "Hello, ".to_owned(),
            Err(Failure::Incomplete),
        )
    );
    // The completed event's blank line never arrives.
    let cut = &TEXT[..TEXT.len() - 1];
    assert_eq!(
        read(cut, 64),
        (
            kinds(&[
                "response.created",
                "response.output_text.delta",
                "response.output_text.delta"
            ]),
            "Hello, w\u{f6}rld \u{1f642}".to_owned(),
            Err(Failure::Truncated),
        )
    );
    assert_eq!(
        read(b"", 1),
        (Vec::new(), String::new(), Err(Failure::Truncated))
    );
}

#[test]
fn data_that_isnt_a_typed_event_is_unreadable() {
    for data in [
        &b"data: not json\n\n"[..],
        b"data: {\"delta\":\"no type\"}\n\n",
        b"data: {\"type\":7}\n\n",
        b"data: [\"type\"]\n\n",
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":7}\n\n",
    ] {
        assert_eq!(
            read(data, 64).2,
            Err(Failure::Protocol),
            "{}",
            String::from_utf8_lossy(data)
        );
    }
}

#[test]
fn text_may_be_exactly_the_cap_and_no_longer() {
    for (last, end) in [
        (MAX_TEXT_BYTES / 2, Ok(())),
        (MAX_TEXT_BYTES / 2 + 1, Err(Failure::Protocol)),
    ] {
        let delta = |bytes: usize| {
            event(&json!({"type": "response.output_text.delta", "delta": "a".repeat(bytes)}))
        };
        let mut stream = delta(MAX_TEXT_BYTES / 2);
        stream.extend(delta(last));
        stream.extend_from_slice(COMPLETED);
        for size in [stream.len(), 64 * 1024] {
            assert_eq!(read(&stream, size).2, end, "{last} in chunks of {size}");
        }
    }
}

#[test]
fn a_readers_debug_shows_counts_never_bytes() {
    let mut reader = Reader::default();
    let partial = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"PLANTED";
    assert_eq!(reader.feed(partial, &mut |_| {}), Ok(Progress::More));
    let printed = format!("{reader:?}");
    assert!(!printed.contains("PLANTED"));
    let bytes = format!("{:?}", b"PLANTED");
    assert!(!printed.contains(bytes.trim_matches(['[', ']'])));
}

#[test]
fn debug_shows_only_a_known_event_kind() {
    let known = SseEvent(
        json!({"type": "response.output_text.delta", "delta": "PLANTED-TEXT"}),
        None,
    );
    let unknown = SseEvent(
        json!({"type": "PLANTED-KIND", "delta": "PLANTED-TEXT"}),
        None,
    );
    assert_eq!(known.kind(), "response.output_text.delta");
    assert_eq!(unknown.kind(), "PLANTED-KIND");
    for printed in [format!("{known:?}"), format!("{unknown:?}")] {
        for marker in ["PLANTED-TEXT", "PLANTED-KIND"] {
            assert!(!printed.contains(marker));
            let bytes = format!("{:?}", marker.as_bytes());
            assert!(!printed.contains(bytes.trim_matches(['[', ']'])));
        }
    }
}
