//! Manual Rez measurement of the numeric parser's exact-roundtrip cost.

use std::{hint::black_box, time::Instant};

use kumochi_scene::parse_record;
use serde_json::json;

#[test]
#[ignore = "manual quiet-Rez release measurement, not a CI performance assertion"]
fn measure_number_dense_typed_parse() {
    let mut put = vec![json!({"chart":{"id":"c","kind":"line","box":[0,0,100,100]}})];
    for series in 0..8 {
        put.push(json!({"series":{"id":format!("s{series}"),"in":"c","data":vec![51.248_178_375_505_404;200]}}));
    }
    let bytes =
        serde_json::to_vec(&json!({"step":{"id":"dense","put":put}})).expect("fixture JSON");
    assert_eq!(bytes.len(), 30_814);
    for _ in 0..64 {
        drop(black_box(
            parse_record(black_box(&bytes)).expect("valid fixture"),
        ));
    }
    let samples: Vec<_> = (0..64)
        .map(|_| {
            let start = Instant::now();
            drop(black_box(
                parse_record(black_box(&bytes)).expect("valid fixture"),
            ));
            start.elapsed().as_nanos()
        })
        .collect();
    println!(
        "number_dense_typed_parse bytes=30814 points=1600 warmups=64 samples=64 includes_drop=true"
    );
    for (index, nanos) in samples.iter().enumerate() {
        println!("sample,{index},{nanos}");
    }
}
