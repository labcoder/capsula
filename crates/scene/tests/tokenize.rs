//! Streaming grammar, cursor boundaries, allocation checks and Rez measurements.

use capsula_scene::{
    limits::{MAX_DRAWING_BYTES, MAX_RECORD_BYTES},
    tokenize::{TokenError, Tokenizer},
};

#[path = "support/allocation.rs"]
mod allocation;

#[expect(
    clippy::panic_in_result_fn,
    reason = "test helper asserts cursor and index invariants while propagating syntax errors"
)]
fn consume(
    tokenizer: &mut Tokenizer,
    chunk: &[u8],
    records: &mut Vec<Vec<u8>>,
) -> Result<(), TokenError> {
    let mut rest = chunk;
    while let Some(record) = tokenizer.next(&mut rest)? {
        assert_eq!(
            usize::try_from(record.index).expect("small fixture"),
            records.len(),
            "indices are consecutive"
        );
        records.push(record.bytes.to_vec());
    }
    assert!(rest.is_empty(), "a drained delta has no remainder");
    Ok(())
}

fn scan(bytes: &[u8], delta: usize) -> Result<Vec<Vec<u8>>, TokenError> {
    let mut tokenizer = Tokenizer::new();
    let mut records = Vec::new();
    for chunk in bytes.chunks(delta) {
        consume(&mut tokenizer, chunk, &mut records)?;
    }
    tokenizer.finish()?;
    Ok(records)
}

#[test]
fn every_split_preserves_records_and_unicode() {
    let records: [&[u8]; 4] = [
        br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"description"}}"#,
        r#"{"step":{"id":"s","say":"é水🙂","put":[{"text":{"id":"t","at":[0,0],"text":"quote \" brace } slash \\ pair \uD83D\uDE42"}}]}}"#.as_bytes(),
        br#"{ "pretty" 	: [ 1 	, true
, false , null , "ends\\" ] }"#,
        br#"{"end":{}}"#,
    ];
    let mut drawing = b" \r\n{ \"records\" : [ ".to_vec();
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            drawing.extend(b" , \t ");
        }
        drawing.extend(*record);
    }
    drawing.extend(b" ] } \n");
    for split in 0..=drawing.len() {
        let mut tokenizer = Tokenizer::new();
        let mut actual = Vec::new();
        consume(&mut tokenizer, &drawing[..split], &mut actual).expect("valid first delta");
        consume(&mut tokenizer, &drawing[split..], &mut actual).expect("valid second delta");
        tokenizer.finish().expect("complete drawing");
        assert!(
            actual.iter().map(Vec::as_slice).eq(records),
            "all byte and UTF-8 splits preserve the exact records"
        );
    }
    let actual = scan(&drawing, 1).expect("single-byte deltas");
    assert!(
        actual.iter().map(Vec::as_slice).eq(records),
        "byte-at-a-time stream agrees"
    );
    assert!(
        scan(br#"{"\u0072ecords":[]}"#, 1)
            .expect("escaped envelope key")
            .is_empty(),
        "empty records array is legal"
    );
}

#[test]
fn cursor_stops_at_each_record_in_a_large_delta() {
    let mut tokenizer = Tokenizer::new();
    let mut rest: &[u8] = br#"{"records":[{"a":1},{"b":2},{"c":3}]}"#;
    let first = tokenizer
        .next(&mut rest)
        .expect("first record")
        .expect("record closed");
    assert_eq!(first.index, 0);
    assert_eq!(first.bytes, br#"{"a":1}"#);
    assert_eq!(rest, br#",{"b":2},{"c":3}]}"#);
    let second = tokenizer
        .next(&mut rest)
        .expect("second record")
        .expect("record closed");
    assert_eq!(second.index, 1);
    assert_eq!(second.bytes, br#"{"b":2}"#);
    assert_eq!(
        rest, br#",{"c":3}]}"#,
        "caller can stop without consuming record three"
    );
    let third = tokenizer
        .next(&mut rest)
        .expect("third record")
        .expect("record closed");
    assert_eq!(third.index, 2);
    assert_eq!(third.bytes, br#"{"c":3}"#);
    assert!(
        tokenizer.next(&mut rest).expect("envelope tail").is_none(),
        "no extra records"
    );
    tokenizer.finish().expect("drawing complete");
}

#[test]
fn closing_on_the_last_byte_emits_without_an_extra_delta() {
    let mut tokenizer = Tokenizer::new();
    let mut rest: &[u8] = br#"{"records":[{}"#;
    assert_eq!(
        tokenizer
            .next(&mut rest)
            .expect("record closes")
            .expect("emitted")
            .bytes,
        b"{}"
    );
    assert!(rest.is_empty(), "nothing beyond the close was needed");
    let mut tail: &[u8] = b"]}";
    assert!(
        tokenizer.next(&mut tail).expect("tail closes").is_none(),
        "no more records"
    );
    tokenizer.finish().expect("complete");
}

fn drawing_with_record_size(size: usize) -> Vec<u8> {
    let mut bytes = b"{\"records\":[{".to_vec();
    bytes.extend(std::iter::repeat_n(b' ', size - 2));
    bytes.extend(b"}]}");
    bytes
}

#[test]
fn byte_and_depth_limits_are_exact() {
    assert_eq!(
        scan(&drawing_with_record_size(MAX_RECORD_BYTES), 40).expect("record at cap")[0].len(),
        MAX_RECORD_BYTES
    );
    assert_eq!(
        scan(&drawing_with_record_size(MAX_RECORD_BYTES + 1), 4096).err(),
        Some(TokenError::RecordTooLarge)
    );
    let mut drawing = b"{\"records\":[]}".to_vec();
    drawing.resize(MAX_DRAWING_BYTES, b' ');
    assert!(
        scan(&drawing, 4096).expect("drawing at cap").is_empty(),
        "exact drawing cap is legal"
    );
    drawing.push(b' ');
    assert_eq!(
        scan(&drawing, 4096).err(),
        Some(TokenError::DrawingTooLarge)
    );
    let at_cap = br#"{"v":[[[[[[[[[[[[[0]]]]]]]]]]]]]}"#;
    let drawing = format!(
        r#"{{"records":[{}]}}"#,
        std::str::from_utf8(at_cap).expect("ASCII fixture")
    );
    let actual = scan(drawing.as_bytes(), 1).expect("depth 16 is legal");
    assert!(
        actual.iter().map(Vec::as_slice).eq([at_cap.as_slice()]),
        "exact single record at depth 16"
    );
    assert_eq!(
        scan(br#"{"records":[{"v":[[[[[[[[[[[[[[0]]]]]]]]]]]]]]}]}"#, 1).err(),
        Some(TokenError::TooDeep)
    );
}

#[test]
fn malformed_json_is_terminal_and_never_resynchronizes() {
    use TokenError::Malformed;
    let fixtures: &[(&[u8], TokenError)] = &[
        (b"{}", Malformed { at: 1 }),
        (br#"{"other":[]}"#, Malformed { at: 2 }),
        (br#"{"records":[],"records":[]}"#, Malformed { at: 13 }),
        (br#"{"records":[],"other":0}"#, Malformed { at: 13 }),
        (br#"{"records":[{},]}"#, Malformed { at: 15 }),
        (br#"{"records":[true]}"#, Malformed { at: 12 }),
        (br#"{"records":[{"x":}]}"#, Malformed { at: 17 }),
        (br#"{"records":[{"x":01}]}"#, Malformed { at: 18 }),
        (br#"{"records":[{"x":1.}]}"#, Malformed { at: 19 }),
        (br#"{"records":[{"x":1e+}]}"#, Malformed { at: 20 }),
        (br#"{"records":[{"x":tru}]}"#, Malformed { at: 20 }),
        (br#"{"records":[{"x":null false}]}"#, Malformed { at: 22 }),
        (br#"{"records":[{"x":[1,]}]}"#, Malformed { at: 20 }),
        (br#"{"records":[{"x":1,}]}"#, Malformed { at: 19 }),
        (br#"{"records":[{"x" 1}]}"#, Malformed { at: 17 }),
        (br#"{"records":[{"x":"\q"}]}"#, Malformed { at: 19 }),
        (br#"{"records":[{"x":"\uD800"}]}"#, Malformed { at: 24 }),
        (br#"{"records":[{"x":"\uDC00"}]}"#, Malformed { at: 23 }),
        (
            br#"{"records":[{"x":"\uD800\u0041"}]}"#,
            Malformed { at: 29 },
        ),
        (br#"{"records":[{"x":"\u0x00"}]}"#, Malformed { at: 21 }),
        (br#"{"records":[{"x":]}]}"#, Malformed { at: 17 }),
        (b"\xef\xbb\xbf{\"records\":[]}", Malformed { at: 0 }),
        (b"{\xff\"records\":[]}", Malformed { at: 1 }),
        (b"{\"records\":[\xff]}", Malformed { at: 12 }),
        (b"{\"records\":[]}\xff", Malformed { at: 14 }),
    ];
    for (fixture, expected) in fixtures {
        let mut tokenizer = Tokenizer::new();
        let mut rest = *fixture;
        let error = loop {
            match tokenizer.next(&mut rest) {
                Err(error) => break error,
                Ok(Some(_)) => {}
                Ok(None) => panic!("malformed fixture must fail before end of input"),
            }
        };
        assert_eq!(
            &error, expected,
            "the first invalid byte determines the error"
        );
        let mut valid_tail: &[u8] = b"\n{\"end\":{}}]}";
        assert_eq!(
            tokenizer.next(&mut valid_tail).err(),
            Some(error),
            "errors are sticky"
        );
        assert_eq!(
            tokenizer.finish(),
            Err(error),
            "finish preserves the first error"
        );
    }
    let valid = br#"{"records":[]}"#;
    let mut garbage = valid.to_vec();
    garbage.push(b'x');
    assert_eq!(scan(&garbage, 1).err(), Some(Malformed { at: valid.len() }));
}

#[test]
fn incomplete_stream_and_invalid_utf8_fail() {
    for fixture in [
        b"".as_slice(),
        br#"{"records":["#,
        br#"{"records":[{"x":"unfinished"#,
        br#"{"records":[{}"#,
        br#"{"records":[{"x":"\uD800\u"#,
    ] {
        assert_eq!(scan(fixture, 1).err(), Some(TokenError::Truncated));
    }
    let invalid_utf8: &[(&[u8], usize)] = &[
        (b"\xff", 18),
        (b"\xc0\xaf", 18),
        (b"\xed\xa0\x80", 19),
        (b"\xf4\x90\x80\x80", 19),
        (b"\xe2\x22", 19),
        (b"\x00", 18),
    ];
    for (invalid, at) in invalid_utf8 {
        let mut fixture = b"{\"records\":[{\"x\":\"".to_vec();
        fixture.extend(*invalid);
        fixture.extend(b"\"}]}");
        assert_eq!(
            scan(&fixture, 1).err(),
            Some(TokenError::Malformed { at: *at })
        );
    }
    let numbers = br#"{"records":[{"v":[-0,0,1,-12,0.5,1e4,-0.25E-2,true,false,null]}]}"#;
    assert_eq!(scan(numbers, 1).expect("valid scalar grammar").len(), 1);
}

fn measurement_drawing() -> Vec<u8> {
    let mut records = vec![
        serde_json::json!({"begin":{"v":1,"scene":"measure","surface":"overlay","alt":""}}),
        serde_json::json!({"step":{"id":"initial","put":(0..64).map(|i| serde_json::json!({"text":{"id":format!("t{i}"),"at":[0,0],"text":""}})).collect::<Vec<_>>()}}),
    ];
    let text = "x".repeat(400);
    for update in 0..18 {
        records.push(serde_json::json!({"step":{"id":format!("update{update}"),"set":(0..64).map(|i| serde_json::json!({"target":format!("t{i}"),"text":text})).collect::<Vec<_>>()}}));
    }
    records.push(serde_json::json!({"end":{}}));
    for record in &records {
        let bytes = serde_json::to_vec(record).expect("generated record encodes");
        assert!(
            capsula_scene::parse_record(&bytes).is_ok(),
            "benchmark uses typed-valid records"
        );
    }
    let mut drawing = serde_json::to_vec(&serde_json::json!({"records":records}))
        .expect("generated drawing encodes");
    assert!(
        drawing.len() <= MAX_DRAWING_BYTES,
        "fixture fits the call cap"
    );
    drawing.resize(MAX_DRAWING_BYTES, b' ');
    drawing
}

fn drain_without_copy(tokenizer: &mut Tokenizer, drawing: &[u8], delta: usize) {
    let mut count = 0;
    for chunk in drawing.chunks(delta) {
        let mut rest = chunk;
        while let Some(record) = tokenizer.next(&mut rest).expect("valid generated envelope") {
            std::hint::black_box(record.bytes);
            count += 1;
        }
    }
    tokenizer.finish().expect("complete generated drawing");
    assert_eq!(count, 21, "every record emitted");
}

#[test]
fn steady_state_uses_the_one_record_buffer() {
    let drawing = measurement_drawing();
    for delta in [40, 4096] {
        let mut tokenizer = Tokenizer::new();
        let ((), count) = allocation::count(|| drain_without_copy(&mut tokenizer, &drawing, delta));
        assert_eq!(count, 0, "no allocations after the buffer is created");
    }
}

#[test]
fn allocation_counter_observes_allocation_zeroing_and_reallocation() {
    let size = std::hint::black_box(32);
    let (buffers, count) = allocation::count(|| {
        let mut growing = Vec::<u8>::with_capacity(size);
        growing.reserve_exact(size * 2);
        let zeroed = vec![0_u8; size];
        std::hint::black_box((growing, zeroed))
    });
    assert_eq!(count, 3, "the counter sees all three allocation operations");
    drop(buffers);
}

#[test]
fn debug_and_errors_do_not_expose_buffered_content() {
    let mut tokenizer = Tokenizer::new();
    let mut input: &[u8] = br#"{"records":[{"label":"private-content-sentinel"}"#;
    let record = tokenizer
        .next(&mut input)
        .expect("closed record")
        .expect("emission");
    assert!(
        !format!("{record:?}").contains("private"),
        "closed-record Debug is redacted"
    );
    assert!(
        !format!("{tokenizer:?}").contains("private"),
        "tokenizer Debug is redacted"
    );
    let error = tokenizer.finish().expect_err("missing envelope tail");
    assert!(
        !format!("{error} {error:?}").contains("private"),
        "errors contain no buffered content"
    );
}

#[test]
#[ignore = "normal-priority timing on a quiet Rez with the measurement lock"]
fn measure_tokenizer() {
    use std::time::{Duration, Instant};
    let drawing = measurement_drawing();
    for delta in [40_usize, 4096] {
        for _ in 0..5 {
            drain_without_copy(&mut Tokenizer::new(), &drawing, delta);
        }
        let mut elapsed = Duration::ZERO;
        for _ in 0..64 {
            let mut tokenizer = Tokenizer::new();
            let start = Instant::now();
            drain_without_copy(&mut tokenizer, &drawing, delta);
            elapsed += start.elapsed();
        }
        let mut tokenizer = Tokenizer::new();
        let ((), allocations) =
            allocation::count(|| drain_without_copy(&mut tokenizer, &drawing, delta));
        let bytes = u32::try_from(drawing.len()).expect("bounded drawing");
        let deltas = u32::try_from(drawing.len().div_ceil(delta)).expect("bounded delta count");
        let seconds = elapsed.as_secs_f64();
        let mb_s = f64::from(bytes) * 64.0 / seconds / 1_000_000.0;
        let ns_delta = seconds * 1_000_000_000.0 / (f64::from(deltas) * 64.0);
        println!(
            "delta_bytes={delta} drawing_bytes={bytes} runs=64 MB/s={mb_s:.3} ns/delta={ns_delta:.3} steady_allocations={allocations}"
        );
        assert_eq!(allocations, 0, "one reused record buffer");
    }
}
