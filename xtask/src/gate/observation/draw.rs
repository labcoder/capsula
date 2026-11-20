//! Keyed draw timing rows in the ordinary native observer.

use anyhow::{Context as _, Result, bail, ensure};

use super::{Observation, fields, number, positive};

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
    pub(in crate::gate) paint: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::gate) struct Active {
    pub(in crate::gate) qpc: i64,
    pub(in crate::gate) segment_ms: u64,
    pub(in crate::gate) entrance_ms: u64,
    pub(in crate::gate) narrated: bool,
}

impl Observation {
    pub(in crate::gate) fn all_painted(&self) -> bool {
        self.draw_steps.iter().all(|step| step.paint.is_some())
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
                let [run, scene, index, qpc, segment, entrance, narrated] = fields(
                    text,
                    [
                        "run",
                        "scene",
                        "index",
                        "qpc",
                        "segment_ms",
                        "entrance_ms",
                        "narrated",
                    ],
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
                    narrated: match narrated {
                        "0" => false,
                        "1" => true,
                        _ => bail!("invalid draw narration bit"),
                    },
                });
            }
            "draw_paint" => {
                let [run, scene, index, qpc] = fields(text, ["run", "scene", "index", "qpc"])?;
                let qpc = positive(qpc)?;
                let step = self.draw_step(key(run, scene, index)?)?;
                ensure!(
                    step.paint.is_none() && step.active.is_some_and(|active| active.qpc <= qpc),
                    "duplicate or unordered draw paint stamp"
                );
                step.paint = Some(qpc);
            }
            _ => bail!("unknown draw timing stamp"),
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
