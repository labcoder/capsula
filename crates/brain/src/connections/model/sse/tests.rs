use std::time::Instant;

use kumochi_protocol::message::{Action, ErrorKey, UserError};
use serde_json::{Value, json};

use super::{
    MAX_EVENT_BYTES, MAX_LINE_BYTES, MAX_STREAM_BYTES, MAX_TEXT_BYTES, Progress, Reader, SseEvent,
};
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

/// A comment line of exactly `bytes` bytes, its line feed excluded.
fn comment(bytes: usize) -> Vec<u8> {
    let mut line = vec![b'a'; bytes];
    line[0] = b':';
    line.push(b'\n');
    line
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
fn a_line_may_be_exactly_the_cap_and_no_longer() {
    for (bytes, end) in [
        (MAX_LINE_BYTES, Ok(())),
        (MAX_LINE_BYTES + 1, Err(Failure::Protocol)),
    ] {
        let mut stream = comment(bytes);
        stream.extend_from_slice(COMPLETED);
        // Whole, the line ends in the chunk; in 64 KiB chunks it's buffered first.
        for size in [stream.len(), 64 * 1024] {
            assert_eq!(
                read(&stream, size).2,
                end,
                "{bytes} bytes in chunks of {size}"
            );
        }
    }
    // A line that never ends is refused once it's past the cap.
    let endless = vec![b'a'; MAX_LINE_BYTES + 1];
    assert_eq!(read(&endless, 4096).2, Err(Failure::Protocol));
}

#[test]
fn an_events_data_may_be_exactly_the_cap_and_no_longer() {
    // Whitespace lines between JSON tokens, joined by line feeds: 19 + 1 + first + 1 + rest + 1 + 1.
    let first = 600_000;
    for (rest, end) in [
        (MAX_EVENT_BYTES - 23 - first, Ok(())),
        (MAX_EVENT_BYTES - 22 - first, Err(Failure::Protocol)),
    ] {
        let mut stream = b"data: {\"type\":\"keepalive\"\ndata: ".to_vec();
        stream.extend(vec![b' '; first]);
        stream.extend_from_slice(b"\ndata: ");
        stream.extend(vec![b' '; rest]);
        stream.extend_from_slice(b"\ndata: }\n\n");
        stream.extend_from_slice(COMPLETED);
        for size in [stream.len(), 64 * 1024] {
            let (seen, _, got) = read(&stream, size);
            assert_eq!(got, end, "{rest} in chunks of {size}");
            if end.is_ok() {
                assert_eq!(seen, kinds(&["keepalive", "response.completed"]));
            }
        }
    }
}

#[test]
fn a_stream_may_be_exactly_the_cap_and_no_longer() {
    let mut stream = Vec::with_capacity(MAX_STREAM_BYTES + 1);
    let mut padding = MAX_STREAM_BYTES - COMPLETED.len();
    while padding > 0 {
        let line = padding.min(MAX_LINE_BYTES + 1);
        stream.extend(comment(line - 1));
        padding -= line;
    }
    stream.extend_from_slice(COMPLETED);
    assert_eq!(stream.len(), MAX_STREAM_BYTES);
    let at_cap = stream.clone();
    // A trailing [DONE] after the terminal event isn't read, whether or not it shares a chunk.
    stream.extend_from_slice(b"data: [DONE]\n\n");
    for size in [stream.len(), MAX_STREAM_BYTES, 64 * 1024] {
        assert_eq!(read(&stream, size).2, Ok(()), "chunks of {size}");
    }
    let mut over = at_cap;
    over.insert(0, b'\n');
    for size in [over.len(), 64 * 1024] {
        assert_eq!(
            read(&over, size).2,
            Err(Failure::Protocol),
            "chunks of {size}"
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
    assert_eq!(
        format!("{reader:?}"),
        "Reader { received: 59, text: 0, .. }"
    );
}

#[test]
fn debug_shows_only_a_known_event_kind() {
    let known = SseEvent(json!({"type": "response.output_text.delta", "delta": "PLANTED-TEXT"}));
    assert_eq!(
        format!("{known:?}"),
        "SseEvent { kind: \"response.output_text.delta\" }"
    );
    let unknown = SseEvent(json!({"type": "PLANTED-KIND", "delta": "PLANTED-TEXT"}));
    assert_eq!(format!("{unknown:?}"), "SseEvent { kind: \"other\" }");
}

/// The spike's framer (spikes/connections-spike/src/inference.rs), the "before" for PR 11's
/// measurement: it joins data lines and parses each event the same way.
fn spike_feed(line: &mut Vec<u8>, data: &mut Vec<u8>, bytes: &[u8], events: &mut usize) {
    for &b in bytes {
        if b != b'\n' {
            line.push(b);
            continue;
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            if !data.is_empty() {
                data.pop();
                if data.as_slice() != b"[DONE]" {
                    let value: Value = serde_json::from_slice(data).expect("spike event");
                    *events += usize::from(value["type"].is_string());
                }
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            data.extend_from_slice(value);
            data.push(b'\n');
        }
        line.clear();
    }
}

/// About 1 MiB of text deltas, as the provider sends them, then the completed event.
fn megabyte_of_deltas() -> Vec<u8> {
    let mut stream = Vec::new();
    let mut sequence = 0;
    while stream.len() < 1024 * 1024 - 512 {
        sequence += 1;
        let event = json!({
            "type": "response.output_text.delta",
            "sequence_number": sequence,
            "item_id": "msg_measure",
            "output_index": 0,
            "content_index": 0,
            "delta": " a word or two",
        });
        stream.extend(format!("event: response.output_text.delta\ndata: {event}\n\n").into_bytes());
    }
    stream.extend_from_slice(COMPLETED);
    stream
}

#[test]
#[ignore = "a measurement for a quiet Rez: cargo test -p kumochi-brain --release measure_ -- --ignored --nocapture"]
fn measure_feed_against_the_spike() {
    let stream = megabyte_of_deltas();
    let events = stream.windows(2).filter(|pair| pair == b"\n\n").count();
    let rounds = 50;
    let started = Instant::now();
    for _ in 0..rounds {
        let mut reader = Reader::default();
        let mut seen = 0;
        let mut count = |_: &SseEvent| seen += 1;
        for chunk in stream.chunks(16 * 1024) {
            if reader.feed(chunk, &mut count) == Ok(Progress::Completed) {
                break;
            }
        }
        assert_eq!(seen, events, "every event read");
    }
    let ours = started.elapsed();
    let started = Instant::now();
    for _ in 0..rounds {
        let (mut line, mut data, mut seen) = (Vec::new(), Vec::new(), 0);
        for chunk in stream.chunks(16 * 1024) {
            spike_feed(&mut line, &mut data, chunk, &mut seen);
        }
        assert_eq!(seen, events, "every event read");
    }
    let spike = started.elapsed();
    let bytes = f64::from(u32::try_from(stream.len()).expect("about 1 MiB"));
    let count = f64::from(u32::try_from(events).expect("tens of thousands"));
    let mib = f64::from(rounds) * bytes / (1024.0 * 1024.0);
    for (name, took) in [("reader", ours), ("spike", spike)] {
        let per_event = took.as_secs_f64() * 1e9 / (f64::from(rounds) * count);
        eprintln!(
            "{name}: {:.0} MiB/s, {per_event:.0} ns per event ({events} events, {} bytes)",
            mib / took.as_secs_f64(),
            stream.len()
        );
    }
}
