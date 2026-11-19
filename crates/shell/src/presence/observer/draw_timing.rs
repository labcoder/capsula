//! Joins the presenter's accepted sample with the frame stamped in the overlay paint.

use std::{cell::Cell, fmt, rc::Rc};

use gpui_kit::{App, Window};
use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_shell_core::presenter::{DrawKey, DrawTiming, Presenter};

use super::{Drawn, Observer, Record, Shared, Surface};
use crate::platform;

type Observation = (SceneId, u64, u32);
type Observations = [Option<Observation>; MAX_VISIBLE_SCENES];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PaintStamp {
    frame: u64,
    qpc: i64,
    hwnd: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum DrawRecord {
    Ready {
        key: DrawKey,
        qpc: i64,
    },
    Active {
        key: DrawKey,
        qpc: i64,
        segment_ms: u64,
        entrance_ms: u64,
    },
    Paint {
        key: DrawKey,
        stamp: PaintStamp,
    },
}

impl fmt::Display for DrawRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, key, qpc) = match *self {
            Self::Ready { key, qpc } => ("ready", key, qpc),
            Self::Active { key, qpc, .. } => ("active", key, qpc),
            Self::Paint { key, stamp } => ("paint", key, stamp.qpc),
        };
        write!(
            f,
            "draw_{kind} run={} scene={} index={} qpc={qpc}",
            key.run.0, key.scene.0, key.index
        )?;
        match *self {
            Self::Ready { .. } => Ok(()),
            Self::Active {
                segment_ms,
                entrance_ms,
                ..
            } => {
                write!(f, " segment_ms={segment_ms} entrance_ms={entrance_ms}")
            }
            Self::Paint { stamp, .. } => write!(f, " frame={} hwnd={:#x}", stamp.frame, stamp.hwnd),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PaintReceipts(Cell<[Option<(Observation, PaintStamp)>; MAX_VISIBLE_SCENES]>);

impl PaintReceipts {
    fn remember(&self, before: &Observations, after: &Observations, stamp: PaintStamp) {
        let pending = self.0.get();
        self.0.set(after.map(|observation| {
            let observation = observation?;
            if !before.contains(&Some(observation)) {
                return Some((observation, stamp));
            }
            pending
                .iter()
                .flatten()
                .find(|(key, _)| *key == observation)
                .copied()
        }));
    }

    fn take(&self, key: DrawKey, epoch: u64) -> Option<PaintStamp> {
        let observation = (key.scene, epoch, key.index.checked_add(1)?);
        let mut pending = self.0.get();
        let slot = pending
            .iter_mut()
            .find(|slot| slot.is_some_and(|(key, _)| key == observation))?;
        let (_, stamp) = slot.take()?;
        self.0.set(pending);
        Some(stamp)
    }
}

impl Observer {
    pub(in crate::presence) fn observe_draw_timing(&self, presenter: &mut Presenter) {
        if self.shared.draw_timing.is_some() {
            let shared = Rc::clone(&self.shared);
            presenter.observe_draw_timing(move |event| shared.record_draw_timing(event));
        }
    }
}

impl Shared {
    fn record_draw_timing(&self, event: DrawTiming) {
        if !self.active.get() {
            return;
        }
        let record = match event {
            DrawTiming::Ready(key) => DrawRecord::Ready {
                key,
                qpc: platform::performance_counter(),
            },
            DrawTiming::Active {
                key,
                segment_ms,
                entrance_ms,
            } => DrawRecord::Active {
                key,
                qpc: platform::performance_counter(),
                segment_ms,
                entrance_ms,
            },
            DrawTiming::Shown { key, epoch } => {
                let Some(stamp) = self
                    .draw_timing
                    .as_ref()
                    .and_then(|paint| paint.take(key, epoch))
                else {
                    return;
                };
                DrawRecord::Paint { key, stamp }
            }
        };
        self.send(Record::DrawTiming(record));
    }
}

pub(in crate::presence) fn overlay_painted(
    cx: &mut App,
    window: &Window,
    before: &Observations,
    after: &Observations,
) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let observer = cx.global_mut::<Observer>();
    if observer.shared.draw_timing.is_none() {
        return;
    }
    let Ok(hwnd) = platform::window_handle(window) else {
        observer.shared.count_drop();
        return;
    };
    let qpc = platform::performance_counter();
    let frame = observer.paint(Surface::Overlay, Drawn::default(), qpc);
    if let Some(receipts) = &observer.shared.draw_timing {
        receipts.remember(before, after, PaintStamp { frame, qpc, hwnd });
    }
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::RunId;

    use super::*;

    const KEY: DrawKey = DrawKey {
        run: RunId(16),
        scene: SceneId(34),
        index: 4,
    };
    const FIRST: PaintStamp = PaintStamp {
        frame: 72,
        qpc: 8132,
        hwnd: 0xbd3,
    };

    #[test]
    fn exact_scene_epoch_and_bound_keep_the_first_frame_once() {
        let tracker = PaintReceipts::default();
        let later = PaintStamp {
            frame: 73,
            qpc: 8200,
            ..FIRST
        };
        let observation = Some((SceneId(34), 7, 5));
        let earlier = [observation, None, None, None];
        tracker.remember(&[None; 4], &earlier, FIRST);
        tracker.remember(
            &earlier,
            &[Some((SceneId(22), 8, 2)), observation, None, None],
            later,
        );
        assert_eq!(tracker.take(KEY, 6), None);
        assert_eq!(tracker.take(DrawKey { index: 3, ..KEY }, 7), None);
        assert_eq!(
            tracker.take(
                DrawKey {
                    scene: SceneId(30),
                    ..KEY
                },
                7
            ),
            None
        );
        assert_eq!(tracker.take(KEY, 7), Some(FIRST));
        assert_eq!(tracker.take(KEY, 7), None);
    }

    #[test]
    fn timing_records_are_exact_content_free_observer_lines() {
        let mut out = Vec::new();
        for receipt in [
            DrawRecord::Ready { key: KEY, qpc: 159 },
            DrawRecord::Active {
                key: KEY,
                qpc: 160,
                segment_ms: 1455,
                entrance_ms: 150,
            },
            DrawRecord::Paint {
                key: KEY,
                stamp: FIRST,
            },
        ] {
            super::super::line(&mut out, &Record::DrawTiming(receipt)).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "draw_ready run=16 scene=34 index=4 qpc=159\n",
                "draw_active run=16 scene=34 index=4 qpc=160 segment_ms=1455 entrance_ms=150\n",
                "draw_paint run=16 scene=34 index=4 qpc=8132 frame=72 hwnd=0xbd3\n",
            ),
        );
    }
}
