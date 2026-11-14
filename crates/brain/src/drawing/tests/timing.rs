//! A measurement for a quiet Rez: a draw call at the paint caps, from its input to its end, through
//! the reducer and the store double. It uses only what the reducer had before it laid steps out,
//! so the same file measures both sides of that change.

use std::time::Instant;

use super::*;

/// One step of a 36-box flow with labelled arrows, which fills the outline cap.
fn flow(step: &str) -> String {
    let mut put = vec![r#"{"diagram":{"id":"d","dir":"down"}}"#.to_owned()];
    put.extend((0..36).map(|index| {
        format!(r#"{{"node":{{"id":"n{index}","in":"d","label":"Validate the input {index}"}}}}"#)
    }));
    put.extend((1..36).map(|index| {
        format!(
            r#"{{"edge":{{"id":"e{index}","from":"n{}","to":"n{index}","label":"then"}}}}"#,
            index - 1
        )
    }));
    format!(r#"{{"id":"{step}","put":[{}]}}"#, put.join(","))
}

/// One step of 42 filled circles, which fills the fill cap.
fn circles(step: &str) -> String {
    let put: Vec<String> = (0..42)
        .map(|index| {
            let (x, y) = ((index % 30) * 30, 100 + (index / 30) * 30);
            format!(
                r#"{{"shape":{{"id":"c{index}","kind":"ellipse","box":[{x},{y},24,24],"fill":"blue"}}}}"#
            )
        })
        .collect();
    format!(r#"{{"id":"{step}","put":[{}]}}"#, put.join(","))
}

#[test]
#[ignore = "a measurement for a quiet Rez: cargo test -p kumochi-brain --release --lib measure_draw_call -- --ignored --nocapture"]
fn measure_draw_call_to_its_end() {
    for (name, step) in [("flow36", flow("one")), ("circles42", circles("one"))] {
        let mut samples = Vec::new();
        for sample in 0..48 {
            let mut drawing = Drawing::default();
            let mut store = Store::default();
            let input = call(1, "c1", args("s1", std::slice::from_ref(&step)));
            let start = Instant::now();
            let out = store.run(&mut drawing, input);
            let elapsed = start.elapsed().as_micros();
            assert_eq!(out.calls.len(), 1);
            assert_eq!(out.calls[0].error, None, "{name} fits the caps");
            if sample >= 8 {
                samples.push(elapsed);
            }
        }
        samples.sort_unstable();
        println!(
            "draw_call fixture={name} warmups=8 samples={} p50_us={} p95_us={} max_us={}",
            samples.len(),
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1]
        );
    }
}
