use kumochi_scene::{limits::MAX_DRAWING_BYTES, tokenize::Tokenizer};

pub(super) fn measurement_drawing() -> Vec<u8> {
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
            kumochi_scene::parse_record(&bytes).is_ok(),
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

pub(super) fn drain_without_copy(tokenizer: &mut Tokenizer, drawing: &[u8], delta: usize) {
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
