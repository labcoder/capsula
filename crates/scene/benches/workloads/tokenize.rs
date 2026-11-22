use crate::{
    allocation,
    tokenizer_fixture::{drain_without_copy, measurement_drawing},
};
use kumochi_scene::tokenize::Tokenizer;

pub(super) fn measure_tokenizer() {
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
