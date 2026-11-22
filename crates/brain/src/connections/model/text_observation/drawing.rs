//! Local stamps for the fixed offline Responses drawing replay.

use std::io::{self, Write};

use kumochi_protocol::message::{RunId, SceneId};

use super::Observation;

pub(crate) const REPLAY_CALL: &str = "draw-timing";
pub(crate) const REPLAY_STEPS: [&str; 4] =
    ["plot", "timing-first", "timing-second", "timing-third"];

#[derive(Clone, Copy)]
pub(crate) enum DrawStage<'a> {
    Call,
    First(usize),
    Close {
        record: usize,
        bytes: usize,
    },
    Durable {
        scene: SceneId,
        step: &'a str,
    },
    Pipe {
        scene: SceneId,
        index: u32,
        step: &'a str,
    },
}

#[derive(Default)]
pub(super) struct DrawReceipt {
    call: Option<(RunId, u64)>,
    steps: [Step; REPLAY_STEPS.len()],
    invalid: bool,
}

#[derive(Default)]
struct Step {
    first: Option<u64>,
    closed: Option<(u64, usize)>,
    durable: Option<(SceneId, u64)>,
    pipe: Option<u64>,
}

impl Observation {
    pub(crate) fn mark_draw(&self, run: RunId, at: Option<u64>, stage: DrawStage<'_>) {
        if let Some(draw) = &self.draw
            && let Ok(mut draw) = draw.lock()
            && !at.is_some_and(|at| draw.mark(run, at, stage))
        {
            draw.invalid = true;
        }
    }
}

impl DrawReceipt {
    fn mark(&mut self, run: RunId, at: u64, stage: DrawStage<'_>) -> bool {
        if matches!(stage, DrawStage::Call) {
            return self.call.replace((run, at)).is_none();
        }
        if self.call.is_none_or(|(owner, _)| owner != run) {
            return false;
        }
        match stage {
            DrawStage::Call => false,
            DrawStage::First(record) => self
                .steps
                .get_mut(record)
                .is_some_and(|step| step.first.replace(at).is_none()),
            DrawStage::Close { record, bytes } => self
                .steps
                .get_mut(record)
                .is_some_and(|step| bytes > 0 && step.closed.replace((at, bytes)).is_none()),
            DrawStage::Durable { scene, step } => REPLAY_STEPS
                .iter()
                .position(|id| *id == step)
                .is_some_and(|index| self.steps[index].durable.replace((scene, at)).is_none()),
            DrawStage::Pipe { scene, index, step } => usize::try_from(index).is_ok_and(|index| {
                REPLAY_STEPS.get(index) == Some(&step)
                    && self.steps[index]
                        .durable
                        .is_some_and(|(stored, _)| scene == stored)
                    && self.steps[index].pipe.replace(at).is_none()
            }),
        }
    }

    pub(super) fn write(&self, pid: u32, out: &mut impl Write) -> io::Result<()> {
        let invalid = || io::Error::other("incomplete drawing timing receipt");
        let (run, call) = self.call.filter(|_| !self.invalid).ok_or_else(invalid)?;
        let scene = self.steps[0].durable.ok_or_else(invalid)?.0;
        for (index, step) in self.steps.iter().enumerate() {
            let first = step.first.ok_or_else(invalid)?;
            let (close, bytes) = step.closed.ok_or_else(invalid)?;
            let (stored, ack) = step.durable.ok_or_else(invalid)?;
            let pipe = step.pipe.ok_or_else(invalid)?;
            if scene != stored || ![call, first, close, ack, pipe].is_sorted() {
                return Err(invalid());
            }
            writeln!(
                out,
                "draw {pid} {} {} {index} {bytes} {call} {first} {close} {ack} {pipe}",
                run.0, scene.0
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DrawReceipt, DrawStage, REPLAY_STEPS};
    use kumochi_protocol::message::{RunId, SceneId};
    use std::sync::{Arc, Mutex};

    #[test]
    fn receipt_contains_only_key_size_and_ordered_clocks() {
        let (run, scene) = (RunId(9), SceneId(11));
        let pid = std::process::id();
        for pipe_missing in [false, true] {
            let out = Arc::new(Mutex::new(Vec::new()));
            let capture = Arc::clone(&out);
            let observation =
                super::super::fixture_mode(1, Some(DrawReceipt::default()), move |line| {
                    capture.lock().unwrap().push(line.to_owned());
                });
            observation.mark_draw(run, Some(100), DrawStage::Call);
            for (index, step) in REPLAY_STEPS.into_iter().enumerate() {
                observation.mark_draw(run, Some(120), DrawStage::First(index));
                observation.mark_draw(
                    run,
                    Some(130),
                    DrawStage::Close {
                        record: index,
                        bytes: 15,
                    },
                );
                observation.mark_draw(run, Some(140), DrawStage::Durable { scene, step });
                if !pipe_missing || index != 2 {
                    observation.mark_draw(
                        run,
                        Some(155),
                        DrawStage::Pipe {
                            scene,
                            index: u32::try_from(index).unwrap(),
                            step,
                        },
                    );
                }
            }
            observation.finish();
            let rows = out.lock().unwrap();
            assert!(rows[0].starts_with(&format!("ready {pid} 1 ")));
            if pipe_missing {
                assert!(!rows.iter().any(|line| line.starts_with("final ")));
            } else {
                assert_eq!(rows.len(), 6);
                assert!(rows[5].starts_with(&format!("final {pid} 0 0 ")));
                assert_eq!(
                    rows[1..5],
                    [
                        format!("draw {pid} 9 11 0 15 100 120 130 140 155"),
                        format!("draw {pid} 9 11 1 15 100 120 130 140 155"),
                        format!("draw {pid} 9 11 2 15 100 120 130 140 155"),
                        format!("draw {pid} 9 11 3 15 100 120 130 140 155"),
                    ]
                );
            }
        }
    }
}
