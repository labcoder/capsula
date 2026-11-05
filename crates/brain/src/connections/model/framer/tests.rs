use std::ops::ControlFlow;

use super::{Framer, Limit, MAX_EVENT_BYTES, MAX_LINE_BYTES, MAX_STREAM_BYTES};

const END: &[u8] = b"data: end\n\n";

/// Each event's data, and how the stream ended.
type Framed = (Vec<Vec<u8>>, Result<ControlFlow<()>, Limit>);

/// Feeds `bytes` in chunks of `size` until an event whose data is `end` stops it, or a cap.
fn frame(bytes: &[u8], size: usize) -> Framed {
    let mut framer = Framer::default();
    let mut events = Vec::new();
    let mut on_data = |data: &[u8]| -> Result<ControlFlow<()>, Limit> {
        events.push(data.to_vec());
        Ok(if data == b"end" {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    };
    let mut end = Ok(ControlFlow::Continue(()));
    for chunk in bytes.chunks(size) {
        end = framer.feed(chunk, &mut on_data);
        if end != Ok(ControlFlow::Continue(())) {
            break;
        }
    }
    (events, end)
}

/// Reads every event.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the framer callback returns Result"
)]
fn keep(_: &[u8]) -> Result<ControlFlow<()>, Limit> {
    Ok(ControlFlow::Continue(()))
}

/// A comment line of exactly `bytes` bytes before its `ending`.
fn comment(bytes: usize, ending: &[u8]) -> Vec<u8> {
    let mut line = vec![b'a'; bytes];
    line[0] = b':';
    line.extend_from_slice(ending);
    line
}

#[test]
fn fragmented_multiline_events_are_joined_the_same_in_any_chunks() {
    let stream = concat!(
        ": a comment\r\nevent: ignored\r\nid: 7\r\n",
        "data: first\r\ndata:second\r\ndata\r\n\r\n",
        "retry: 9\n\ndata:\n\ndata: end\n\ndata: after\n\n",
    )
    .as_bytes();
    let expected = (
        vec![b"first\nsecond\n".to_vec(), Vec::new(), b"end".to_vec()],
        Ok(ControlFlow::Break(())),
    );
    // One byte at a time splits every CRLF and every line.
    for size in [stream.len(), 1, 3] {
        assert_eq!(frame(stream, size), expected, "chunks of {size}");
    }
}

#[test]
fn the_bytes_after_a_break_are_not_counted() {
    let mut framer = Framer::default();
    let mut stream = END.to_vec();
    stream.extend_from_slice(b"data: after\n\n");
    let mut stop = |_: &[u8]| Ok::<_, Limit>(ControlFlow::Break(()));
    assert_eq!(framer.feed(&stream, &mut stop), Ok(ControlFlow::Break(())));
    assert_eq!(framer.received(), END.len());
}

#[test]
fn a_line_may_be_exactly_the_cap_and_no_longer() {
    // A CR counts toward its line, as it did before the framer was shared.
    for (bytes, ending, end) in [
        (MAX_LINE_BYTES, "\n", Ok(ControlFlow::Break(()))),
        (MAX_LINE_BYTES + 1, "\n", Err(Limit)),
        (MAX_LINE_BYTES - 1, "\r\n", Ok(ControlFlow::Break(()))),
        (MAX_LINE_BYTES, "\r\n", Err(Limit)),
    ] {
        let mut stream = comment(bytes, ending.as_bytes());
        stream.extend_from_slice(END);
        let events = if end.is_ok() {
            vec![b"end".to_vec()]
        } else {
            Vec::new()
        };
        // Whole, the line ends in the chunk; in 64 KiB chunks it's buffered first.
        for size in [stream.len(), 64 * 1024] {
            assert_eq!(
                frame(&stream, size),
                (events.clone(), end),
                "{bytes} bytes and {ending:?} in chunks of {size}"
            );
        }
    }
    let mut framer = Framer::default();
    let unended = vec![b'a'; MAX_LINE_BYTES];
    assert_eq!(
        framer.feed(&unended, &mut keep),
        Ok(ControlFlow::Continue(()))
    );
    assert_eq!(framer.feed(b"a", &mut keep), Err(Limit));
}

#[test]
fn an_events_data_may_be_exactly_the_cap_and_no_longer() {
    let first = 600_000;
    for (rest, end) in [
        (MAX_EVENT_BYTES - 1 - first, Ok(ControlFlow::Break(()))),
        (MAX_EVENT_BYTES - first, Err(Limit)),
    ] {
        let mut stream = b"data: ".to_vec();
        stream.extend(vec![b'x'; first]);
        stream.extend_from_slice(b"\ndata: ");
        stream.extend(vec![b'y'; rest]);
        stream.extend_from_slice(b"\n\n");
        stream.extend_from_slice(END);
        let mut joined = vec![b'x'; first];
        joined.push(b'\n');
        joined.extend(vec![b'y'; rest]);
        assert_eq!(joined.len(), MAX_EVENT_BYTES + usize::from(end.is_err()));
        let expected = if end.is_ok() {
            vec![joined, b"end".to_vec()]
        } else {
            Vec::new()
        };
        for size in [stream.len(), 64 * 1024] {
            let (events, got) = frame(&stream, size);
            assert_eq!(got, end, "{rest} in chunks of {size}");
            // Not assert_eq: a failure would print a megabyte.
            assert!(events == expected, "{rest} in chunks of {size}");
        }
    }
}

#[test]
fn a_stream_may_be_exactly_the_cap_and_no_longer() {
    let mut stream = Vec::with_capacity(MAX_STREAM_BYTES + 1);
    let mut padding = MAX_STREAM_BYTES - END.len();
    while padding > 0 {
        let line = padding.min(MAX_LINE_BYTES + 1);
        stream.extend(comment(line - 1, b"\n"));
        padding -= line;
    }
    stream.extend_from_slice(END);
    assert_eq!(stream.len(), MAX_STREAM_BYTES);
    let at_cap = stream.clone();
    // Bytes after the event that stops the stream aren't counted, in its chunk or not.
    stream.extend_from_slice(b"data: after\n\n");
    for size in [stream.len(), MAX_STREAM_BYTES, 64 * 1024] {
        assert_eq!(
            frame(&stream, size),
            (vec![b"end".to_vec()], Ok(ControlFlow::Break(()))),
            "chunks of {size}"
        );
    }
    let mut over = at_cap;
    over.insert(0, b'\n');
    for size in [over.len(), 64 * 1024] {
        assert_eq!(
            frame(&over, size),
            (Vec::new(), Err(Limit)),
            "chunks of {size}"
        );
    }
}

#[test]
fn a_framers_debug_shows_counts_never_bytes() {
    let mut framer = Framer::default();
    let partial = b"data: PLANTED\ndata: PLANTED";
    assert_eq!(
        framer.feed(partial, &mut keep),
        Ok(ControlFlow::Continue(()))
    );
    let debug = format!("{framer:?}");
    assert_eq!(debug, "Framer { received: 27, .. }");
    assert!(!debug.contains("PLANTED"), "{debug}");
    let planted = format!("{:?}", b"PLANTED");
    let planted = planted.trim_start_matches('[').trim_end_matches(']');
    assert!(!debug.contains(planted), "{debug}");
}
