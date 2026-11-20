use std::time::Duration;

use kumochi_protocol::message::{RunId, SegmentId};

use super::{SOURCE_RATE, frame_time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) run: RunId,
    pub(crate) generation: u64,
    pub(crate) segment: SegmentId,
    pub(crate) at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Presented {
    pub(crate) source: Source,
    pub(crate) first: u64,
    pub(crate) through: u64,
    pub(crate) at: Option<Duration>,
}

pub(super) struct Conversion {
    origin: u64,
    frames: u64,
}

impl Conversion {
    pub(super) fn start(origin: u64) -> Self {
        Self { origin, frames: 0 }
    }

    pub(super) fn stamp(&mut self, source: Source, samples: usize, frames: usize) -> Stamp {
        let stamp = Stamp {
            source,
            origin: self.origin,
            first_frame: self.frames,
            limit: source.at + u64::try_from(samples).expect("bounded source packet"),
        };
        self.frames += u64::try_from(frames).expect("bounded device block");
        stamp
    }
}

#[derive(Clone, Copy)]
pub(super) struct Stamp {
    source: Source,
    origin: u64,
    first_frame: u64,
    limit: u64,
}

impl Stamp {
    pub(super) fn copied(
        self,
        offset: usize,
        frames: usize,
        rate: u32,
        at: Option<Duration>,
        output_offset: usize,
    ) -> (u64, Option<Presented>) {
        let device_first = self.first_frame + u64::try_from(offset).expect("bounded device block");
        let device_end = device_first + u64::try_from(frames).expect("bounded output copy");
        let front = device_first * u64::from(SOURCE_RATE);
        let end = device_end * u64::from(SOURCE_RATE);
        let rate_u64 = u64::from(rate);
        let first = (self.origin + front / rate_u64).min(self.limit);
        let through = (self.origin + end / rate_u64).min(self.limit);
        let handed = (self.origin + end.div_ceil(rate_u64)).min(self.limit);
        let report = (first < through).then(|| Presented {
            source: self.source,
            first,
            through,
            at: frame_time(at, output_offset + frames - 1, rate),
        });
        (handed, report)
    }
}
