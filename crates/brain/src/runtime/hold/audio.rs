use kumochi_protocol::message::{HoldFailure, MAX_HOLD_CHUNKS, MAX_MIC_SAMPLES, MicPcm};
use zeroize::{Zeroize as _, Zeroizing};

use crate::voice::resample::Resampler;

pub(super) const RATE: u32 = 16_000;
const SAMPLES: usize = 960_000;

pub(super) struct Audio {
    terminal: bool,
    rate: Option<u32>,
    next: u32,
    last: bool,
    ended: Option<u32>,
    stopped: bool,
    samples: Zeroizing<Vec<i16>>,
    resampler: Option<Resampler>,
    raw: Zeroizing<Vec<f32>>,
    output: Zeroizing<Vec<f32>>,
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audio")
            .field("rate", &self.rate)
            .field("chunks", &self.next)
            .field("last", &self.last)
            .field("ended", &self.ended)
            .field("stopped", &self.stopped)
            .field("samples", &self.samples.len())
            .finish_non_exhaustive()
    }
}

impl Audio {
    pub(super) fn new() -> Self {
        Self {
            terminal: false,
            rate: None,
            next: 0,
            last: false,
            ended: None,
            stopped: false,
            samples: Zeroizing::new(Vec::with_capacity(SAMPLES)),
            resampler: None,
            raw: Zeroizing::new(Vec::with_capacity(MAX_MIC_SAMPLES)),
            output: Zeroizing::new(Vec::with_capacity(MAX_MIC_SAMPLES)),
        }
    }

    pub(super) fn push(&mut self, chunk: &MicPcm) -> Result<(), HoldFailure> {
        if self.terminal
            || chunk.check().is_err()
            || self.last
            || chunk.seq != self.next
            || self.rate.is_some_and(|rate| rate != chunk.rate)
            || self.ended.is_some_and(|count| chunk.seq >= count)
        {
            self.seal();
            return Err(HoldFailure::InvalidAudio);
        }
        if self.rate.is_none() {
            self.rate = Some(chunk.rate);
            self.resampler = (chunk.rate != RATE).then(|| {
                let mut resampler = Resampler::new(chunk.rate, RATE);
                resampler.reserve_input(MAX_MIC_SAMPLES);
                resampler
            });
        }
        if let Some(resampler) = &mut self.resampler {
            self.raw.clear();
            self.raw.extend(
                chunk
                    .samples
                    .iter()
                    .map(|&sample| f32::from(sample) / 32_768.0),
            );
            self.output.clear();
            resampler.push(&self.raw, &mut self.output);
            if chunk.last {
                resampler.finish(&mut self.output);
            }
            if self.samples.len() + self.output.len() > SAMPLES {
                self.seal();
                return Err(HoldFailure::TooLong);
            }
            self.samples
                .extend(self.output.iter().copied().map(quantize));
        } else {
            if self.samples.len() + chunk.samples.len() > SAMPLES {
                self.seal();
                return Err(HoldFailure::TooLong);
            }
            self.samples.extend_from_slice(&chunk.samples);
        }
        self.next += 1;
        self.last = chunk.last;
        if self.last && self.ended.is_some_and(|count| count != self.next) {
            self.seal();
            return Err(HoldFailure::InvalidAudio);
        }
        Ok(())
    }

    pub(super) fn end(&mut self, chunks: u32) -> Result<(), HoldFailure> {
        if self.terminal
            || self.ended.is_some()
            || chunks > MAX_HOLD_CHUNKS
            || chunks < self.next
            || (self.last && chunks != self.next)
        {
            self.seal();
            return Err(HoldFailure::InvalidAudio);
        }
        self.ended = Some(chunks);
        Ok(())
    }

    pub(super) fn stopped(&mut self) -> Result<(), HoldFailure> {
        if self.terminal || self.stopped {
            self.seal();
            return Err(HoldFailure::InvalidAudio);
        }
        self.stopped = true;
        Ok(())
    }

    pub(super) fn take(&mut self) -> Option<Vec<i16>> {
        if self.terminal {
            return None;
        }
        let count = self.ended?;
        if !self.stopped || self.next != count || !self.last && count != 0 {
            return None;
        }
        let samples = std::mem::take(&mut *self.samples);
        self.seal();
        Some(samples)
    }

    fn seal(&mut self) {
        self.terminal = true;
        self.samples.zeroize();
        self.raw.zeroize();
        self.output.zeroize();
        self.resampler = None;
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "finite resampled PCM is rounded and clamped to the signed 16-bit range"
)]
fn quantize(sample: f32) -> i16 {
    (sample * 32_768.0)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumochi_protocol::message::GestureId;

    fn packet(rate: u32, seq: u32, last: bool, samples: Vec<i16>) -> MicPcm {
        MicPcm {
            gesture: GestureId(1),
            rate,
            seq,
            last,
            samples,
        }
    }

    #[test]
    fn either_pipe_order_waits_for_the_exact_count_and_physical_stop() {
        for control_first in [false, true] {
            let mut audio = Audio::new();
            if control_first {
                audio.end(2).unwrap();
                audio.stopped().unwrap();
                assert_eq!(audio.take(), None);
            }
            audio
                .push(&packet(16_000, 0, false, vec![91; 320]))
                .unwrap();
            assert_eq!(audio.take(), None);
            audio
                .push(&packet(16_000, 1, true, vec![-37, 8, 204]))
                .unwrap();
            if !control_first {
                assert_eq!(audio.take(), None);
                audio.end(2).unwrap();
                assert_eq!(audio.take(), None);
                audio.stopped().unwrap();
            }
            let mut expected = vec![91; 320];
            expected.extend([-37, 8, 204]);
            assert_eq!(audio.take(), Some(expected));
            assert_eq!(audio.take(), None);
            assert_eq!(
                audio.push(&packet(16_000, 0, true, vec![3])),
                Err(HoldFailure::InvalidAudio)
            );
        }
    }

    #[test]
    fn independently_derived_sinc_tail_is_not_lost() {
        let mut audio = Audio::new();
        audio
            .push(&packet(8_000, 0, true, vec![256, 0, -128, 64]))
            .unwrap();
        audio.end(1).unwrap();
        audio.stopped().unwrap();
        assert_eq!(
            audio.take(),
            Some(vec![252, 194, 3, -145, -130, -13, 65, 49])
        );
    }

    #[test]
    fn zero_samples_never_needs_a_fabricated_terminal_packet() {
        let mut audio = Audio::new();
        audio.end(0).unwrap();
        assert_eq!(audio.take(), None);
        audio.stopped().unwrap();
        assert_eq!(audio.take(), Some(Vec::new()));
        assert_eq!(audio.take(), None);
    }

    #[test]
    fn malformed_framing_and_repeated_terminals_are_rejected() {
        for fault in 0..8 {
            let mut audio = Audio::new();
            let rejected = match fault {
                0 => audio.push(&packet(16_000, 1, true, vec![1])),
                1 | 2 => {
                    audio.push(&packet(16_000, 0, false, vec![1; 320])).unwrap();
                    audio.push(&packet(
                        if fault == 1 { 16_000 } else { 8_000 },
                        if fault == 1 { 0 } else { 1 },
                        true,
                        vec![2],
                    ))
                }
                3 | 4 => {
                    audio.push(&packet(16_000, 0, true, vec![3])).unwrap();
                    if fault == 4 {
                        audio.end(1).unwrap();
                    }
                    audio.end(if fault == 3 { 2 } else { 1 })
                }
                5 => {
                    audio.stopped().unwrap();
                    audio.stopped()
                }
                6 => {
                    audio.push(&packet(16_000, 0, true, vec![3])).unwrap();
                    audio.push(&packet(16_000, 1, true, vec![4]))
                }
                _ => audio.end(MAX_HOLD_CHUNKS + 1),
            };
            assert_eq!(rejected, Err(HoldFailure::InvalidAudio));
            assert_eq!(
                audio.push(&packet(16_000, 0, true, vec![9])),
                Err(HoldFailure::InvalidAudio)
            );
            assert_eq!(audio.end(1), Err(HoldFailure::InvalidAudio));
            assert_eq!(audio.stopped(), Err(HoldFailure::InvalidAudio));
            assert_eq!(audio.take(), None);
        }
    }

    #[test]
    fn full_sixty_second_bound_and_one_more_are_distinct() {
        let mut audio = Audio::new();
        for seq in 0..MAX_HOLD_CHUNKS {
            audio
                .push(&packet(
                    16_000,
                    seq,
                    seq + 1 == MAX_HOLD_CHUNKS,
                    vec![7; 320],
                ))
                .unwrap();
        }
        audio.end(MAX_HOLD_CHUNKS).unwrap();
        audio.stopped().unwrap();
        assert_eq!(audio.take().unwrap(), vec![7; SAMPLES]);
        assert_eq!(
            audio.push(&packet(16_000, MAX_HOLD_CHUNKS, true, vec![7])),
            Err(HoldFailure::InvalidAudio)
        );
    }

    #[test]
    fn audio_debug_redacts_both_text_and_byte_list_markers() {
        let marker = b"voice-sentinel";
        let samples: Vec<_> = marker.iter().copied().map(i16::from).collect();
        for rate in [8_000, 16_000] {
            let mut audio = Audio::new();
            audio.push(&packet(rate, 0, true, samples.clone())).unwrap();
            let debug = format!("{audio:?}");
            assert!(!debug.contains("voice-sentinel"));
            assert!(!debug.contains(&format!("{samples:?}")));
        }
    }
}
