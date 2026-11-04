use super::{
    Body, Failure, Framing, Head, MAX_CHUNK_LINE_BYTES, MAX_HEAD_BYTES, MAX_TRAILER_BYTES, Step,
    head, request,
};

/// Decodes `wire` fed in chunks of `size`: the data, and the bytes left after the body or why it failed.
fn decode(framing: Framing, wire: &[u8], size: usize) -> (Vec<u8>, Result<usize, Failure>) {
    let mut body = Body::new(framing);
    let mut chunks = wire.chunks(size);
    let (mut buf, mut data) = (Vec::new(), Vec::new());
    loop {
        match body.step(&buf) {
            Ok(Step::Data(length)) => {
                data.extend(buf.drain(..length));
            }
            Ok(Step::Framing(length)) => {
                buf.drain(..length);
            }
            Ok(Step::More) => match chunks.next() {
                Some(chunk) => buf.extend_from_slice(chunk),
                None => {
                    if let Err(failure) = body.end() {
                        return (data, Err(failure));
                    }
                }
            },
            Ok(Step::Done) => {
                let left = buf.len() + chunks.map(<[u8]>::len).sum::<usize>();
                return (data, Ok(left));
            }
            Err(failure) => return (data, Err(failure)),
        }
    }
}

fn parsed(bytes: &[u8]) -> Result<Option<(Head, usize)>, Failure> {
    head(bytes)
}

fn framing(bytes: &[u8]) -> Result<Framing, Failure> {
    Ok(head(bytes)?.expect("a whole head").0.framing)
}

#[test]
fn the_request_is_the_line_the_headers_and_the_body() {
    let bytes = request("tok.en-1", b"{}").expect("a header-safe bearer");
    assert_eq!(
        bytes.as_slice(),
        b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer tok.en-1\r\n\
          Content-Type: application/json\r\nAccept: text/event-stream\r\n\
          Accept-Encoding: identity\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\n{}"
    );
    assert_eq!(bytes.capacity(), bytes.len());
}

#[test]
fn a_content_coding_other_than_identity_is_refused() {
    for coding in ["gzip", "GZIP", "br", "deflate", "identity, gzip", ""] {
        let bytes =
            format!("HTTP/1.1 200 OK\r\nContent-Encoding: {coding}\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(parsed(bytes.as_bytes()), Err(Failure::Protocol), "{coding}");
        let bytes =
            format!("HTTP/1.1 400 Bad\r\ncontent-encoding: {coding}\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(parsed(bytes.as_bytes()), Err(Failure::Protocol), "{coding}");
    }
    for coding in ["identity", "Identity"] {
        let bytes =
            format!("HTTP/1.1 200 OK\r\nContent-Encoding: {coding}\r\nContent-Length: 0\r\n\r\n");
        assert!(matches!(parsed(bytes.as_bytes()), Ok(Some(_))), "{coding}");
    }
}

#[test]
fn a_bearer_that_isnt_a_header_value_is_refused() {
    for bearer in [
        "",
        "a\r\nX-Injected: 1",
        "a\nb",
        "a\0b",
        "a b",
        "a\tb",
        "caf\u{e9}",
    ] {
        assert_eq!(
            request(bearer, b"{}"),
            Err(Failure::Credential),
            "{bearer:?}"
        );
    }
}

/// A 200 head padded to exactly `length` bytes.
fn padded_head(length: usize) -> Vec<u8> {
    let start = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Pad: ";
    let mut bytes = start.to_vec();
    bytes.extend(vec![b'a'; length - start.len() - 4]);
    bytes.extend_from_slice(b"\r\n\r\n");
    assert_eq!(bytes.len(), length);
    bytes
}

#[test]
fn a_head_may_be_exactly_the_cap_and_no_longer() {
    let exact = padded_head(MAX_HEAD_BYTES);
    assert_eq!(
        parsed(&exact)
            .expect("within the cap")
            .map(|(_, length)| length),
        Some(MAX_HEAD_BYTES)
    );
    assert_eq!(parsed(&exact[..MAX_HEAD_BYTES - 1]), Ok(None));
    let over = padded_head(MAX_HEAD_BYTES + 1);
    assert_eq!(parsed(&over), Err(Failure::Protocol));
    assert_eq!(parsed(&over[..MAX_HEAD_BYTES]), Err(Failure::Protocol));
}

#[test]
fn a_head_may_have_32_headers_and_no_more() {
    for (count, ok) in [(32, true), (33, false)] {
        let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n".to_vec();
        for index in 1..count {
            bytes.extend(format!("X-{index}: v\r\n").into_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        let expected = if ok {
            Ok(Some(bytes.len()))
        } else {
            Err(Failure::Protocol)
        };
        assert_eq!(
            parsed(&bytes).map(|head| head.map(|(_, length)| length)),
            expected,
            "{count} headers"
        );
    }
}

#[test]
fn framing_is_one_clear_choice_read_case_insensitively() {
    let cases: [(&[u8], Result<Framing, Failure>); 12] = [
        (
            b"HTTP/1.1 200 OK\r\ncontent-LENGTH:  42 \r\n\r\n",
            Ok(Framing::Length(42)),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n",
            Ok(Framing::Chunked),
        ),
        (
            b"HTTP/1.1 200 OK\r\nConnection: keep-alive, Close\r\n\r\n",
            Ok(Framing::Close),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (b"HTTP/1.1 200 OK\r\n\r\n", Err(Failure::Protocol)),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5 5\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n",
            Err(Failure::Protocol),
        ),
        (
            b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n",
            Err(Failure::Protocol),
        ),
    ];
    for (bytes, expected) in cases {
        assert_eq!(
            framing(bytes),
            expected,
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn interim_heads_are_100_and_103_and_101_is_refused() {
    for status in [100, 103] {
        let bytes = format!("HTTP/1.1 {status} Continue\r\n\r\n");
        let (head, length) = parsed(bytes.as_bytes())
            .expect("an interim head")
            .expect("whole");
        assert_eq!(
            (head.status, head.framing, length),
            (status, Framing::Length(0), bytes.len())
        );
    }
    for status in [101, 102] {
        let bytes = format!("HTTP/1.1 {status} Other\r\n\r\n");
        assert_eq!(parsed(bytes.as_bytes()), Err(Failure::Protocol), "{status}");
    }
}

#[test]
fn the_media_type_and_close_are_read_from_their_headers() {
    let bytes = b"HTTP/1.1 200 OK\r\nContent-Type: Text/Event-Stream; charset=utf-8\r\nContent-Length: 0\r\n\r\n";
    let head = parsed(bytes).expect("a head").expect("whole").0;
    assert!(head.event_stream && !head.close);
    let bytes = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    let head = parsed(bytes).expect("a head").expect("whole").0;
    assert!(!head.event_stream && head.close);
}

#[test]
fn chunks_extensions_and_trailers_decode_in_any_chunking() {
    let wire = b"5;name=value\r\nhello\r\n1\r\n \r\n6\r\nworld!\r\n0\r\nX-Trailer: t\r\n\r\nNEXT";
    for size in [wire.len(), 1, 3] {
        assert_eq!(
            decode(Framing::Chunked, wire, size),
            (b"hello world!".to_vec(), Ok(4)),
            "chunks of {size}"
        );
    }
}

#[test]
fn a_chunk_size_line_may_be_exactly_the_cap_and_no_longer() {
    for (length, ok) in [
        (MAX_CHUNK_LINE_BYTES, true),
        (MAX_CHUNK_LINE_BYTES + 1, false),
    ] {
        let mut wire = b"1;".to_vec();
        wire.extend(vec![b'x'; length - 2]);
        wire.extend_from_slice(b"\r\na\r\n0\r\n\r\n");
        let expected = if ok {
            (b"a".to_vec(), Ok(0))
        } else {
            (Vec::new(), Err(Failure::Protocol))
        };
        for size in [wire.len(), 64] {
            assert_eq!(
                decode(Framing::Chunked, &wire, size),
                expected,
                "{length} in chunks of {size}"
            );
        }
    }
}

#[test]
fn malformed_chunks_are_refused() {
    let cases: [(&[u8], Result<usize, Failure>); 7] = [
        (b"10000000000000000\r\n", Err(Failure::Protocol)),
        (b"\r\n", Err(Failure::Protocol)),
        (b"g\r\n", Err(Failure::Protocol)),
        (b"2\r\nabX\r\n", Err(Failure::Protocol)),
        (b"2\nab\r\n0\r\n\r\n", Err(Failure::Protocol)),
        (b"5\r\nab", Err(Failure::Truncated)),
        (b"FFFFFFFFFFFFFFFF\r\nab", Err(Failure::Truncated)),
    ];
    for (wire, end) in cases {
        assert_eq!(
            decode(Framing::Chunked, wire, 2).1,
            end,
            "{}",
            String::from_utf8_lossy(wire)
        );
    }
}

#[test]
fn trailers_may_be_exactly_the_cap_and_no_longer() {
    for (length, ok) in [(MAX_TRAILER_BYTES, true), (MAX_TRAILER_BYTES + 1, false)] {
        let mut wire = b"0\r\nX: ".to_vec();
        wire.extend(vec![b't'; length - 7]);
        wire.extend_from_slice(b"\r\n\r\n");
        let end = if ok { Ok(0) } else { Err(Failure::Protocol) };
        assert_eq!(decode(Framing::Chunked, &wire, 4096).1, end, "{length}");
    }
}

#[test]
#[ignore = "a measurement for a quiet Rez: cargo test -p kumochi-brain --release measure_ -- --ignored --nocapture"]
fn measure_chunked_framing() {
    use std::time::Instant;
    let payload = vec![b'x'; 1024 * 1024];
    let mut wire = Vec::new();
    for chunk in payload.chunks(16 * 1024) {
        wire.extend(format!("{:x}\r\n", chunk.len()).into_bytes());
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    let rounds: u32 = 200;
    let started = Instant::now();
    for _ in 0..rounds {
        let (data, end) = decode(Framing::Chunked, &wire, 16 * 1024);
        assert_eq!(data.len(), payload.len());
        assert!(end.is_ok());
    }
    let took = started.elapsed();
    let mib = f64::from(rounds) * f64::from(u32::try_from(wire.len()).expect("about 1 MiB"))
        / (1024.0 * 1024.0);
    eprintln!(
        "chunked framing: {:.0} MiB/s over 16 KiB reads, copy out included",
        mib / took.as_secs_f64()
    );
}

#[test]
fn length_and_close_bodies_end_where_they_say() {
    assert_eq!(
        decode(Framing::Length(5), b"helloNEXT", 2),
        (b"hello".to_vec(), Ok(4))
    );
    assert_eq!(
        decode(Framing::Length(5), b"hel", 2),
        (b"hel".to_vec(), Err(Failure::Truncated))
    );
    assert_eq!(decode(Framing::Length(0), b"NEXT", 2), (Vec::new(), Ok(4)));
    assert_eq!(
        decode(Framing::Close, b"all of it", 2),
        (b"all of it".to_vec(), Ok(0))
    );
}
