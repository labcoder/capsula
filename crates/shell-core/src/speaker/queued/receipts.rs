use std::time::Duration;

use kumochi_protocol::message::{
    ControlToBrain, Cursor, PROGRESS_INTERVAL_MS, SegmentId, SpeechRetirement,
};

use super::{
    RATE,
    timeline::{Engine, Tail},
};

const REPORT_SAMPLES: u32 = RATE / 1_000 * PROGRESS_INTERVAL_MS;

#[derive(Default)]
pub(super) struct Receipts {
    next: u32,
    first: u64,
    through: u32,
    heard: Option<Cursor>,
    stopped: bool,
}

impl Receipts {
    pub(super) fn publish(
        &mut self,
        engine: &Engine,
        now: Duration,
        report: &mut impl FnMut(ControlToBrain) -> bool,
    ) -> bool {
        let queue = &engine.queue;
        loop {
            let final_segment = self.next < queue.completed;
            let sample = if final_segment {
                queue.completed_samples[usize::try_from(self.next).expect("bounded segment id")]
            } else {
                u32::try_from(engine.progress.consumed - self.first).expect("one bounded segment")
            };
            if !final_segment
                && (sample == self.through
                    || !queue.stopped && sample - self.through < REPORT_SAMPLES)
            {
                break;
            }
            let segment = SegmentId(self.next);
            let heard = u32::try_from(
                engine
                    .progress
                    .heard
                    .saturating_sub(self.first)
                    .min(u64::from(sample)),
            )
            .expect("heard prefix lies within the segment");
            if !report(ControlToBrain::SpeechRetired(SpeechRetirement {
                run: queue.run,
                output: queue.logical,
                segment,
                retired: sample,
                heard,
                final_segment,
            })) {
                return false;
            }
            if heard != 0 {
                self.heard = Some(Cursor {
                    segment,
                    sample: heard,
                });
            }
            if !final_segment {
                self.through = sample;
                break;
            }
            self.first += u64::from(sample);
            self.next += 1;
            self.through = 0;
        }
        if !self.stopped && engine.final_at(now) {
            self.stopped = report(ControlToBrain::SpeechStopped {
                run: queue.run,
                cursor: self.heard,
            });
            return self.stopped;
        }
        true
    }

    pub(super) fn deadline(&self, engine: &Engine) -> Option<Duration> {
        if engine.queue.stopped
            && !self.stopped
            && let Tail::At(at) = engine.tail()
        {
            return Some(at);
        }
        None
    }
}
