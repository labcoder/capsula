//! The reader over synthetic streams shaped like llama-server's; no socket.

use super::*;

const TEXT: &[u8] = include_bytes!("../../../../fixtures/local/synthetic-llama-server-text.sse");

fn chunk(choice: &str) -> String {
    format!("data: {{\"object\":\"chat.completion.chunk\",\"choices\":[{choice}]}}\n\n")
}

fn delta(content: &str) -> String {
    chunk(&format!("{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":null}}"))
}

const STOP: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
const DONE: &str = "data: [DONE]\n\n";

/// Each piece fed in order, then the body's end: the text handed out and how it ended.
fn read(pieces: &[&[u8]]) -> (String, Result<(), Failure>) {
    let mut reader = Reader::default();
    let mut text = String::new();
    for piece in pieces {
        if let Err(failure) = reader.feed(piece, &mut |delta| text.push_str(delta)) {
            return (text, Err(failure));
        }
    }
    let end = reader.finish();
    (text, end)
}

#[test]
fn a_text_stream_completes_once_after_stop_and_done_however_it_is_split() {
    assert_eq!(read(&[TEXT]), ("Hello from loopback.".to_owned(), Ok(())));
    let bytes: Vec<&[u8]> = TEXT.chunks(1).collect();
    assert_eq!(read(&bytes), ("Hello from loopback.".to_owned(), Ok(())));
    let crlf = String::from_utf8_lossy(TEXT).replace('\n', "\r\n");
    assert_eq!(read(&[crlf.as_bytes()]), ("Hello from loopback.".to_owned(), Ok(())));
}

#[test]
fn anything_but_text_then_stop_then_done_fails_and_keeps_earlier_text() {
    let hi = delta("Hi");
    let tool = chunk(r#"{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"x","arguments":"{}"}}]},"finish_reason":null}"#);
    let function = chunk(r#"{"index":0,"delta":{"function_call":{"name":"x","arguments":"{}"}},"finish_reason":null}"#);
    let length = chunk(r#"{"index":0,"delta":{},"finish_reason":"length"}"#);
    let tool_stop = chunk(r#"{"index":0,"delta":{},"finish_reason":"tool_calls"}"#);
    let second = chunk(r#"{"index":1,"delta":{"content":"x"},"finish_reason":null}"#);
    let two = format!(
        "data: {{\"choices\":[{0},{0}]}}\n\n",
        r#"{"index":0,"delta":{"content":"x"},"finish_reason":null}"#
    );
    let empty = "data: {\"choices\":[]}\n\n".to_owned();
    let no_delta = chunk(r#"{"index":0,"finish_reason":null}"#);
    let number = chunk(r#"{"index":0,"delta":{"content":7},"finish_reason":null}"#);
    let repeated = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\",\"finish_reason\":null}]}\n\n".to_owned();
    let error = "data: {\"error\":{\"code\":500,\"message\":\"secret-detail\",\"type\":\"server_error\"}}\n\n".to_owned();
    let cases: [(&[&str], Failure); 15] = [
        (&[&hi, &tool], Failure::Protocol),
        (&[&hi, &function], Failure::Protocol),
        (&[&hi, &length, DONE], Failure::Incomplete),
        (&[&hi, &tool_stop], Failure::Protocol),
        (&[&hi, &second], Failure::Protocol),
        (&[&hi, &two], Failure::Protocol),
        (&[&hi, &empty], Failure::Protocol),
        (&[&hi, &no_delta], Failure::Protocol),
        (&[&hi, &number], Failure::Protocol),
        (&[&hi, &repeated], Failure::Protocol),
        (&[&hi, &error], Failure::Provider),
        (&[&hi, DONE], Failure::Protocol),
        (&[&hi, STOP, STOP], Failure::Protocol),
        (&[&hi, STOP, &delta("more")], Failure::Protocol),
        (&[&hi, "data: not json\n\n"], Failure::Protocol),
    ];
    for (stream, failure) in cases {
        let pieces: Vec<&[u8]> = stream.iter().map(|piece| piece.as_bytes()).collect();
        assert_eq!(read(&pieces), ("Hi".to_owned(), Err(failure)), "{stream:?}");
        let joined = stream.concat();
        assert_eq!(read(&[joined.as_bytes()]), ("Hi".to_owned(), Err(failure)), "{stream:?}");
    }
}

#[test]
fn data_after_done_fails_in_the_same_read_or_a_later_one() {
    let hi = delta("Hi");
    let after = [hi.as_str(), STOP, DONE, &delta("late")];
    assert_eq!(read(&[after.concat().as_bytes()]).1, Err(Failure::Protocol));
    let pieces: Vec<&[u8]> = after.iter().map(|piece| piece.as_bytes()).collect();
    assert_eq!(read(&pieces).1, Err(Failure::Protocol));
    assert_eq!(read(&[[hi.as_str(), STOP, DONE, DONE].concat().as_bytes()]).1, Err(Failure::Protocol));
    let quiet = [hi.as_str(), STOP, DONE, ": keep-alive\n\n"].concat();
    assert_eq!(read(&[quiet.as_bytes()]), ("Hi".to_owned(), Ok(())));
}

#[test]
fn a_stream_without_done_is_truncated() {
    let hi = delta("Hi");
    assert_eq!(read(&[]), (String::new(), Err(Failure::Truncated)));
    assert_eq!(read(&[hi.as_bytes()]), ("Hi".to_owned(), Err(Failure::Truncated)));
    let stopped = [hi.as_str(), STOP].concat();
    assert_eq!(read(&[stopped.as_bytes()]), ("Hi".to_owned(), Err(Failure::Truncated)));
    let partial = [hi.as_str(), STOP, "data: [DONE]"].concat();
    assert_eq!(read(&[partial.as_bytes()]), ("Hi".to_owned(), Err(Failure::Truncated)));
}

#[test]
fn a_failure_stays_failed_after_valid_bytes() {
    let mut reader = Reader::default();
    let mut text = String::new();
    let bad = "data: {\"choices\":[]}\n\n";
    assert_eq!(reader.feed(bad.as_bytes(), &mut |t| text.push_str(t)), Err(Failure::Protocol));
    assert_eq!(reader.feed(TEXT, &mut |t| text.push_str(t)), Err(Failure::Protocol));
    assert_eq!(reader.finish(), Err(Failure::Protocol));
    assert_eq!(text, "");
}

#[test]
fn the_text_cap_admits_its_exact_limit_and_refuses_one_byte_more() {
    for (len, end) in [(MAX_TEXT_BYTES, Ok(())), (MAX_TEXT_BYTES + 1, Err(Failure::Protocol))] {
        let half = len / 2;
        let stream = [
            delta(&"a".repeat(half)),
            delta(&"b".repeat(len - half)),
            STOP.to_owned(),
            DONE.to_owned(),
        ]
        .concat();
        let (text, got) = read(&[stream.as_bytes()]);
        assert_eq!(got, end);
        assert_eq!(text.len(), if end.is_ok() { len } else { half });
    }
}

#[test]
fn debug_and_errors_carry_no_stream_text() {
    const MARK: &str = "sentinel-7f3a";
    let mut reader = Reader::default();
    let error = format!("data: {{\"error\":{{\"message\":\"{MARK}\"}}}}\n\n");
    reader.feed(delta(MARK).as_bytes(), &mut |_| {}).expect("text");
    let failure = reader.feed(error.as_bytes(), &mut |_| {}).expect_err("error");
    for printed in [format!("{reader:?}"), format!("{failure:?}"), failure.to_string()] {
        assert!(!printed.contains(MARK), "{printed}");
    }
}
