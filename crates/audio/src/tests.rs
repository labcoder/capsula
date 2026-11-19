use std::f64::consts::TAU;

use super::Resampler;

#[test]
fn held_maximum_packets_and_final_padding_keep_the_reserved_history_allocation() {
    use kumochi_protocol::message::MAX_MIC_SAMPLES;
    for (rate, expected) in [(48_000, 1286), (192_000, 1282)] {
        let mut resampler = Resampler::new(rate, 16_000).unwrap();
        resampler.reserve_input(MAX_MIC_SAMPLES);
        let capacity = resampler.history.capacity();
        let allocation = resampler.history.as_ptr();
        let packet = vec![0.125; usize::try_from(rate / 50).unwrap()];
        let mut output = Vec::new();
        for _ in 0..4 {
            resampler.push(&packet, &mut output);
            assert_eq!(resampler.history.capacity(), capacity);
            assert_eq!(resampler.history.as_ptr(), allocation);
        }
        resampler.push(&packet[..17], &mut output);
        resampler.finish(&mut output);
        assert_eq!(resampler.history.capacity(), capacity);
        assert_eq!(resampler.history.as_ptr(), allocation);
        assert_eq!(output.len(), expected);
    }
}

fn tone(hz: f64, rate: u32, samples: usize) -> Vec<f32> {
    (0..samples)
        .map(|n| {
            let n = f64::from(u32::try_from(n).expect("short"));
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                reason = "a test tone"
            )]
            let sample = (0.5 * (TAU * hz * n / f64::from(rate)).sin()) as f32;
            sample
        })
        .collect()
}

fn all(from: u32, to: u32, input: &[f32], pieces: &[usize]) -> Vec<f32> {
    let mut resampler = Resampler::new(from, to).unwrap();
    let mut out = Vec::new();
    let mut rest = input;
    for &piece in pieces.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (now, later) = rest.split_at(piece.min(rest.len()));
        resampler.push(now, &mut out);
        rest = later;
    }
    resampler.finish(&mut out);
    out
}

#[test]
fn the_same_rate_passes_through() {
    let input = tone(1_000.0, 24_000, 100);
    assert_eq!(all(24_000, 24_000, &input, &[7]), input);
    assert_eq!(all(24_000, 24_000, &[], &[7]), Vec::<f32>::new());
}

#[test]
fn every_output_inside_the_input_comes_out_once() {
    let input = vec![0.25; 2_400];
    for (to, want) in [
        (48_000, 4_800),
        (44_100, 4_410),
        (8_000, 800),
        (192_000, 19_200),
    ] {
        assert_eq!(all(24_000, to, &input, &[480]).len(), want, "{to}");
    }
    assert_eq!(all(24_000, 44_100, &[0.5], &[1]).len(), 2);
    assert_eq!(all(24_000, 44_100, &[], &[1]).len(), 0);
}

#[test]
fn how_the_input_is_cut_changes_nothing() {
    let input = tone(440.0, 24_000, 5_000);
    for to in [48_000, 44_100, 16_000] {
        let whole = all(24_000, to, &input, &[5_000]);
        assert_eq!(all(24_000, to, &input, &[1, 7, 333, 2]), whole, "{to}");
    }
}

#[test]
fn a_tone_keeps_its_shape() {
    for to in [48_000, 44_100, 16_000] {
        let out = all(24_000, to, &tone(1_000.0, 24_000, 4_800), &[960]);
        let want = tone(1_000.0, to, out.len());
        // The ends meet the silence around the input.
        let edge = usize::try_from(to / 1_000).expect("small");
        for (n, (got, want)) in out.iter().zip(&want).enumerate().skip(edge) {
            if n + edge >= out.len() {
                break;
            }
            assert!(
                (got - want).abs() < 2e-3,
                "{to} Hz, sample {n}: {got} vs {want}"
            );
        }
    }
}

#[test]
fn a_tone_above_the_output_nyquist_is_removed() {
    let out = all(24_000, 8_000, &tone(6_000.0, 24_000, 4_800), &[960]);
    let middle = &out[100..out.len() - 100];
    let peak = middle
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!(peak < 5e-3, "{peak}");
}

#[test]
fn rate_limits_include_both_edges_and_reject_invalid_values() {
    for rate in [0, 7_999, 192_001, u32::MAX] {
        assert!(Resampler::new(rate, 24_000).is_none());
        assert!(Resampler::new(24_000, rate).is_none());
    }
    assert_eq!(all(192_000, 8_000, &[0.0; 48], &[48]), [0.0; 2]);
    assert_eq!(all(8_000, 192_000, &[0.0; 2], &[2]), [0.0; 48]);
}

#[test]
fn debug_does_not_include_pcm() {
    let mut resampler = Resampler::new(24_000, 48_000).unwrap();
    let samples = [123.25, -456.75];
    resampler.push(&samples, &mut Vec::new());
    let bytes: Vec<u8> = samples.into_iter().flat_map(f32::to_ne_bytes).collect();
    let debug = format!("{resampler:?}");
    assert!(!debug.contains("123.25"));
    assert!(!debug.contains("-456.75"));
    assert!(!debug.contains(&format!("{bytes:?}")));
    assert!(debug.contains("buffered_samples:"));
}
