//! Joins the presenter's accepted sample with the first successful overlay paint.

use std::{cell::Cell, fmt, rc::Rc};

use gpui_kit::App;
#[cfg(debug_assertions)]
use kumochi_protocol::constants::ProductIdentity;
use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_shell_core::presenter::{DrawKey, DrawTiming, Presenter};

#[cfg(debug_assertions)]
use super::super::Launch;
use super::{Observer, Record, Shared};
use crate::platform;

type Observation = (SceneId, u64, u32);
type Observations = [Option<Observation>; MAX_VISIBLE_SCENES];

#[cfg(debug_assertions)]
#[derive(Debug)]
pub(super) struct DrawFixture(bool);

#[cfg(debug_assertions)]
impl DrawFixture {
    pub(super) fn new(identity: ProductIdentity, launch: &Launch, mode: Option<&str>) -> Self {
        Self(identity == ProductIdentity::GATE && launch.draw_timing && mode == Some("draw"))
    }

    pub(super) fn take(&mut self) -> Option<&'static str> {
        std::mem::take(&mut self.0).then_some("Draw the timing fixture.")
    }
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
        narrated: bool,
    },
    Paint {
        key: DrawKey,
        qpc: i64,
    },
}

impl fmt::Display for DrawRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, key, qpc) = match *self {
            Self::Ready { key, qpc } => ("ready", key, qpc),
            Self::Active { key, qpc, .. } => ("active", key, qpc),
            Self::Paint { key, qpc } => ("paint", key, qpc),
        };
        write!(
            f,
            "draw_{kind} run={} scene={} index={} qpc={qpc}",
            key.run.0, key.scene.0, key.index
        )?;
        if let Self::Active {
            segment_ms,
            entrance_ms,
            narrated,
            ..
        } = *self
        {
            write!(
                f,
                " segment_ms={segment_ms} entrance_ms={entrance_ms} narrated={}",
                u8::from(narrated)
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(super) struct PaintReceipts(Cell<[Option<(Observation, i64)>; MAX_VISIBLE_SCENES]>);

impl PaintReceipts {
    fn remember(&self, before: &Observations, after: &Observations, qpc: i64) {
        let pending = self.0.get();
        self.0.set(after.map(|observation| {
            let observation = observation?;
            if !before.contains(&Some(observation)) {
                return Some((observation, qpc));
            }
            pending
                .iter()
                .flatten()
                .find(|(key, _)| *key == observation)
                .copied()
        }));
    }

    fn take(&self, key: DrawKey, epoch: u64) -> Option<i64> {
        let observation = (key.scene, epoch, key.index.checked_add(1)?);
        let mut pending = self.0.get();
        let slot = pending
            .iter_mut()
            .find(|slot| slot.is_some_and(|(key, _)| key == observation))?;
        let (_, qpc) = slot.take()?;
        self.0.set(pending);
        Some(qpc)
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
                narrated,
            } => DrawRecord::Active {
                key,
                qpc: platform::performance_counter(),
                segment_ms,
                entrance_ms,
                narrated,
            },
            DrawTiming::Shown { key, epoch } => {
                let Some(qpc) = self
                    .draw_timing
                    .as_ref()
                    .and_then(|paint| paint.take(key, epoch))
                else {
                    return;
                };
                DrawRecord::Paint { key, qpc }
            }
        };
        self.send(Record::DrawTiming(record));
    }
}

pub(in crate::presence) fn overlay_painted(cx: &App, before: &Observations, after: &Observations) {
    if !cx.has_global::<Observer>() {
        return;
    }
    let shared = &cx.global::<Observer>().shared;
    if shared.active.get()
        && let Some(receipts) = &shared.draw_timing
    {
        receipts.remember(before, after, platform::performance_counter());
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
    const FIRST: i64 = 8132;

    #[test]
    fn exact_scene_epoch_and_bound_keep_the_first_paint_once() {
        let tracker = PaintReceipts::default();
        let observation = Some((SceneId(34), 7, 5));
        let earlier = [observation, None, None, None];
        tracker.remember(&[None; 4], &earlier, FIRST);
        tracker.remember(
            &earlier,
            &[Some((SceneId(22), 8, 2)), observation, None, None],
            8200,
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
                segment_ms: 0,
                entrance_ms: 150,
                narrated: true,
            },
            DrawRecord::Active {
                key: DrawKey { index: 5, ..KEY },
                qpc: 161,
                segment_ms: 0,
                entrance_ms: 0,
                narrated: false,
            },
            DrawRecord::Paint {
                key: KEY,
                qpc: FIRST,
            },
        ] {
            super::super::line(&mut out, &Record::DrawTiming(receipt)).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "draw_ready run=16 scene=34 index=4 qpc=159\n",
                "draw_active run=16 scene=34 index=4 qpc=160 segment_ms=0 entrance_ms=150 narrated=1\n",
                "draw_active run=16 scene=34 index=5 qpc=161 segment_ms=0 entrance_ms=0 narrated=0\n",
                "draw_paint run=16 scene=34 index=4 qpc=8132\n",
            ),
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn only_the_debug_gate_draw_observer_arms_one_fixed_fixture_draft() {
        use ProductIdentity as Identity;

        for (identity, draw_timing, mode) in [
            (Identity::RELEASE, true, Some("draw")),
            (Identity::DEV, true, Some("draw")),
            (Identity::GATE, false, Some("draw")),
            (Identity::GATE, true, None),
            (Identity::GATE, true, Some("1")),
        ] {
            let launch = Launch {
                draw_timing,
                ..Launch::default()
            };
            assert_eq!(DrawFixture::new(identity, &launch, mode).take(), None);
        }
        let launch = Launch {
            draw_timing: true,
            ..Launch::default()
        };
        let mut fixture = DrawFixture::new(Identity::GATE, &launch, Some("draw"));
        assert_eq!(fixture.take(), Some("Draw the timing fixture."));
        assert_eq!(fixture.take(), None);
    }
}
