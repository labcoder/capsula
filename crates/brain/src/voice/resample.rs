//! Streaming windowed-sinc resampling of mono audio between any two rates: TTS makes 24 kHz and
//! the shell plays at its output device's rate. Each output sample sits at its exact rational
//! position in the input, so lengths and timing never drift, and it waits for the input it
//! needs on both sides, so there's no delay to trim.

#[cfg(test)]
mod tests;

use std::f64::consts::PI;
use std::sync::LazyLock;
use zeroize::Zeroize;

/// Zero crossings of the kernel on each side.
const ZEROS: u32 = 16;
/// Table points per zero crossing; the kernel is interpolated linearly between them.
const DENSITY: u32 = 128;
/// The cutoff, as a fraction of the lower rate's Nyquist, so the window's transition band
/// stays below it: 19/20.
const ROLLOFF: (u64, u64) = (19, 20);

/// A Blackman-windowed sinc from 0 to [`ZEROS`], at [`DENSITY`] points per zero crossing.
static TABLE: LazyLock<Vec<f64>> = LazyLock::new(|| {
    (0..=ZEROS * DENSITY)
        .map(|at| {
            let s = f64::from(at) / f64::from(DENSITY);
            let sinc = if at == 0 {
                1.0
            } else {
                (PI * s).sin() / (PI * s)
            };
            let x = s / f64::from(ZEROS);
            sinc * (0.42 + 0.5 * (PI * x).cos() + 0.08 * (2.0 * PI * x).cos())
        })
        .collect()
});

/// One stream's resampler; a Speak owns one.
pub(crate) struct Resampler {
    from: u64,
    to: u64,
    /// The cutoff relative to the input's Nyquist.
    cutoff: f64,
    /// Input samples on each side of an output that weigh in.
    reach: u32,
    /// Input from absolute index `dropped - reach` on; the input before index 0 is silence.
    history: Vec<f32>,
    dropped: u64,
    /// Input samples taken and output samples made.
    taken: u64,
    made: u64,
}

impl Drop for Resampler {
    fn drop(&mut self) {
        self.history.zeroize();
    }
}

impl Resampler {
    /// From `from` Hz to `to` Hz; both are checked rates, so neither is 0.
    pub(crate) fn new(from: u32, to: u32) -> Self {
        let (from, to) = (u64::from(from), u64::from(to));
        let (rolloff, of) = ROLLOFF;
        let lower = from.min(to);
        let reach = (u64::from(ZEROS) * of * from).div_ceil(rolloff * lower);
        let reach = u32::try_from(reach).expect("at most 16 * 20 / 19 * 192 kHz / 8 kHz");
        let cutoff = f64::from(u32::try_from(rolloff * lower).expect("under 192 kHz * 19"))
            / f64::from(u32::try_from(of * from).expect("under 192 kHz * 20"));
        Self {
            from,
            to,
            cutoff,
            reach,
            history: vec![0.0; usize::try_from(reach).unwrap_or(0)],
            dropped: 0,
            taken: 0,
            made: 0,
        }
    }

    /// Reserves a whole packet and retained kernel before the first private sample.
    pub(crate) fn reserve_input(&mut self, maximum: usize) {
        let reach = usize::try_from(self.reach).expect("a bounded kernel");
        self.history.reserve(maximum + reach);
    }

    /// Takes more input and appends every output sample it completes.
    pub(crate) fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        self.history.extend_from_slice(input);
        self.taken += u64::try_from(input.len()).unwrap_or(u64::MAX);
        self.emit(out);
    }

    /// The input ended: appends the rest, which is silence past the last input sample.
    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) {
        if self.from == self.to {
            return;
        }
        let silence = self.history.len() + usize::try_from(self.reach).unwrap_or(0);
        self.history.resize(silence, 0.0);
        self.emit(out);
    }

    /// Makes each output whose position is inside the input and whose inputs have all arrived.
    fn emit(&mut self, out: &mut Vec<f32>) {
        let reach = u64::from(self.reach);
        while self.made * self.from < self.taken * self.to {
            let position = self.made * self.from;
            let at = position / self.to;
            // The first tap is input `at - reach + 1`, which is history index `at + 1`.
            let first = usize::try_from(at + 1 - self.dropped).unwrap_or(usize::MAX);
            let taps = usize::try_from(2 * reach).unwrap_or(usize::MAX);
            let Some(window) = self.history.get(first..first.saturating_add(taps)) else {
                break;
            };
            let fraction = f64::from(u32::try_from(position % self.to).expect("under `to`"))
                / f64::from(u32::try_from(self.to).expect("a checked rate"));
            let mut distance = f64::from(self.reach - 1) + fraction;
            let mut sum = 0.0;
            for &sample in window {
                sum += f64::from(sample) * kernel(distance * self.cutoff);
                distance -= 1.0;
            }
            out.push(narrow(sum * self.cutoff));
            self.made += 1;
        }
        let next = self.made * self.from / self.to + 1;
        let unneeded = usize::try_from(next.saturating_sub(self.dropped))
            .unwrap_or(usize::MAX)
            .min(self.history.len());
        self.history.drain(..unneeded);
        self.dropped += u64::try_from(unneeded).unwrap_or(0);
    }
}

/// The kernel at `s` zero crossings from its centre.
fn kernel(s: f64) -> f64 {
    let s = s.abs();
    if s >= f64::from(ZEROS) {
        return 0.0;
    }
    let position = s * f64::from(DENSITY);
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0 <= position < ZEROS * DENSITY, so it indexes the table"
    )]
    let at = position as usize;
    let t = position - position.floor();
    TABLE[at] * (1.0 - t) + TABLE[at + 1] * t
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "audio samples near ±1, well inside f32"
)]
fn narrow(sample: f64) -> f32 {
    sample as f32
}
