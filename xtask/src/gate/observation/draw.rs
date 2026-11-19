//! Keyed draw timing rows in the ordinary native observer.

use anyhow::{Context as _, Result, ensure};

use super::{Kind, Observation, fields, hex, number, positive};

pub(in crate::gate) const DRAW_STEPS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gate) struct DrawKey {
    pub(in crate::gate) run: u64,
    pub(in crate::gate) scene: u64,
    pub(in crate::gate) index: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub(in crate::gate) struct DrawStep {
    pub(in crate::gate) key: Option<DrawKey>,
    pub(in crate::gate) ready: Option<i64>,
    pub(in crate::gate) active: Option<Active>,
    pub(in crate::gate) paint: Option<Paint>,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::gate) struct Active {
    pub(in crate::gate) qpc: i64,
    pub(in crate::gate) segment_ms: u64,
    pub(in crate::gate) entrance_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::gate) struct Paint {
    pub(in crate::gate) qpc: i64,
    pub(in crate::gate) frame: u64,
    pub(in crate::gate) hwnd: u64,
}

impl Observation {
    pub(in crate::gate) fn all_drawn(&self) -> bool {
        self.draw_steps.iter().all(|step| {
            step.paint
                .and_then(|paint| self.frame(paint.frame))
                .is_some_and(|frame| frame.completion_qpc.is_some())
        })
    }

    pub(in crate::gate) fn frame(&self, id: u64) -> Option<&super::Frame> {
        id.checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.frames.get(index))
    }

    pub(super) fn accept_draw_timing(&mut self, kind: &str, text: &str) -> Result<()> {
        match kind {
            "draw_ready" => {
                let [run, scene, index, qpc] = fields(text, ["run", "scene", "index", "qpc"])?;
                let step = self.draw_step(key(run, scene, index)?)?;
                ensure!(step.ready.is_none(), "duplicate draw ready stamp");
                step.ready = Some(positive(qpc)?);
            }
            "draw_active" => {
                let [run, scene, index, qpc, segment, entrance] = fields(
                    text,
                    ["run", "scene", "index", "qpc", "segment_ms", "entrance_ms"],
                )?;
                let qpc = positive(qpc)?;
                let step = self.draw_step(key(run, scene, index)?)?;
                ensure!(
                    step.active.is_none() && step.ready.is_some_and(|ready| ready <= qpc),
                    "duplicate or unordered draw active stamp"
                );
                step.active = Some(Active {
                    qpc,
                    segment_ms: number(segment)?,
                    entrance_ms: number(entrance)?,
                });
            }
            _ => {
                let [run, scene, index, qpc, frame, hwnd] =
                    fields(text, ["run", "scene", "index", "qpc", "frame", "hwnd"])?;
                let paint = Paint {
                    qpc: positive(qpc)?,
                    frame: number(frame)?,
                    hwnd: hex(hwnd)?,
                };
                ensure!(
                    paint.hwnd > 0
                        && self.frame(paint.frame).is_some_and(|frame| {
                            frame.kind == Kind::Overlay && frame.paint_qpc == paint.qpc
                        })
                        && self.draw_steps.iter().all(|step| {
                            step.paint
                                .is_none_or(|previous| previous.hwnd == paint.hwnd)
                        }),
                    "draw paint does not identify the same observed overlay"
                );
                let step = self.draw_step(key(run, scene, index)?)?;
                ensure!(
                    step.paint.is_none()
                        && step.active.is_some_and(|active| active.qpc <= paint.qpc),
                    "duplicate or unordered confirmed draw paint"
                );
                step.paint = Some(paint);
            }
        }
        Ok(())
    }

    fn draw_step(&mut self, key: DrawKey) -> Result<&mut DrawStep> {
        ensure!(
            self.draw_steps
                .iter()
                .filter_map(|step| step.key)
                .all(|previous| previous.run == key.run && previous.scene == key.scene),
            "shell draw rows changed run or scene"
        );
        let step = self
            .draw_steps
            .get_mut(key.index)
            .context("unexpected shell draw index")?;
        step.key = Some(key);
        Ok(step)
    }
}

fn key(run: &str, scene: &str, index: &str) -> Result<DrawKey> {
    let (run, scene, index) = (number(run)?, number(scene)?, number(index)?);
    ensure!(run > 0 && scene > 0, "zero shell draw identifier");
    Ok(DrawKey { run, scene, index })
}
