//! Killing the brain mid-run and starting it again, and reconnecting to a multi-chunk drawing
//! while `Stop` and `ClearDrawing` arrive between its chunks.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, MAX_STEP_LABEL, MAX_STEPS,
    PageEventKind, PageOf, Phase, RunId, SceneDescriptor, SceneId, Step,
};

use super::brain::Case;
use super::client::{Client, Frame};
use super::{PATIENCE, Row, ended, run_end, store, turn};

pub(super) fn kill(folder: &Path) -> Result<Row> {
    let script = "0 text alpha\n100 step one\n0 text beta\n100 step two\n0 text gamma\n\
                  30000 step never\n";
    let case = Case::new(folder, "kill", script)?;
    let brain = case.start()?;
    let mut client = Client::connect()?;
    let (run, thread) = turn(&mut client, None, "gate kill")?;
    let mut steps = Vec::new();
    let mut text = String::new();
    client.until(PATIENCE, |seen| {
        match &seen.frame {
            Frame::Data(DataToShell::StepCommit { step, .. }, _) => steps.push(step.label.clone()),
            Frame::Data(DataToShell::AssistantText { delta, .. }, _) => text.push_str(delta),
            _ => {}
        }
        (steps.len() == 2 && text.ends_with("gamma")).then_some(())
    })?;
    brain.kill()?;
    drop(client);
    let brain = case.start()?;
    let mut client = Client::connect()?;
    client.data(&DataToBrain::PageRequest {
        request: 1,
        page: PageOf::Events {
            thread,
            before: None,
        },
    })?;
    let mut events = client.until(PATIENCE, |seen| match &seen.frame {
        Frame::Data(DataToShell::Page { events, .. }, _) => Some(events.clone()),
        _ => None,
    })?;
    events.reverse();
    client.quit()?;
    brain.exited()?;
    let outcome = store::outcome(&store::open(&case.store())?, run.0)?;
    let stored = |kind| {
        events
            .iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.text.clone())
            .collect::<Vec<_>>()
    };
    let (kept_steps, kept_text) = (
        stored(PageEventKind::SceneStep),
        stored(PageEventKind::AssistantText).concat(),
    );
    let mut failures = Vec::new();
    if outcome != Some(("failed".to_owned(), Some("crash".to_owned()))) {
        failures.push(format!("the run is stored as {outcome:?}"));
    }
    if kept_steps != steps {
        failures.push(format!(
            "{} of {} committed steps kept",
            kept_steps.len(),
            steps.len()
        ));
    }
    if kept_text != "alphabeta" {
        failures.push(format!(
            "{} bytes of text kept, not the 9 committed with the last step",
            kept_text.len()
        ));
    }
    if stored(PageEventKind::User) != ["gate kill"] {
        failures.push("the user turn isn't kept once".to_owned());
    }
    let detail = format!(
        "TerminateProcess after 2 StepCommits and {} bytes of text; after the restart the run \
         is failed(crash), both steps and the 9 bytes stored with the last step are kept, and \
         the 5 bytes streamed after it are gone",
        text.len()
    );
    Ok(Row::checked(
        "Kill mid-run, then recover",
        &failures,
        detail,
    ))
}

/// Drawings made before the reconnects, each one scene of the fixture's `MAX_STEPS` steps.
const DRAWINGS: usize = 4;

pub(super) fn reconnect(folder: &Path) -> Result<Row> {
    let label = "s".repeat(MAX_STEP_LABEL);
    let script = format!("0 step {label}\n").repeat(MAX_STEPS);
    let case = Case::new(folder, "reconnect", &script)?;
    let brain = case.start()?;
    let mut client = Client::connect()?;
    let mut last = RunId(0);
    let mut thread = None;
    for _ in 0..DRAWINGS {
        let (run, named) = turn(&mut client, thread, "gate draw")?;
        thread = Some(named);
        if ended(&mut client, run)?.1 != Phase::Done {
            bail!("drawing run {} didn't finish", run.0);
        }
        run_end(&mut client, run)?;
        last = run;
    }
    drop(client);
    let mut client = Client::connect_paused()?;
    let mut second = recover(&mut client, last, &label)?;
    client.data(&DataToBrain::PageRequest {
        request: 1,
        page: PageOf::Threads {
            search: None,
            after: None,
        },
    })?;
    client
        .until(PATIENCE, |seen| {
            matches!(seen.frame, Frame::Data(DataToShell::ThreadList { .. }, _)).then_some(())
        })
        .context("the connection didn't serve a page after recovery")?;
    drop(client);
    let kept: Vec<SceneDescriptor> = second
        .named
        .iter()
        .filter(|descriptor| Some(descriptor.scene) != second.cleared)
        .copied()
        .collect();
    let mut client = Client::connect()?;
    let third = settle(&mut client, &kept, &label)?;
    client.quit()?;
    brain.exited()?;
    if second.chunks < 3 {
        second
            .failures
            .push(format!("{} snapshot chunks, not 3 or more", second.chunks));
    }
    second.failures.extend(third.failures);
    let detail = format!(
        "{DRAWINGS} drawings of {MAX_STEPS} steps × {MAX_STEP_LABEL} B, {} chunks; Stop for run {} \
         and ClearDrawing of the second scene after the first chunk. By the shell's rules \
         (install at the last chunk only, never a cleared scene), the other 3 installed whole \
         with the fixture's steps and the cleared one never did, checked after every chunk; the \
         next connection named the same 3, unchanged, and installed them whole. One scene can't \
         reach 3 chunks today (32 KiB target, at most {MAX_STEPS} × {MAX_STEP_LABEL} B), so the \
         chunks span scenes",
        second.chunks, last.0
    );
    Ok(Row::checked(
        "Reconnect with a multi-chunk drawing while Stop arrives",
        &second.failures,
        detail,
    ))
}

/// The client's applied-message record, kept by the shell's rules: chunks stage per scene, a
/// scene installs only at its last chunk, and a scene cleared locally or not named by `Scenes`
/// never installs.
#[derive(Debug, Default)]
struct Applied {
    named: Vec<SceneDescriptor>,
    staged: BTreeMap<SceneId, Vec<Step>>,
    installed: BTreeMap<SceneId, Vec<Step>>,
    cleared: Option<SceneId>,
    chunks: usize,
    failures: Vec<String>,
}

impl Applied {
    fn new(named: Vec<SceneDescriptor>) -> Self {
        Self {
            named,
            ..Self::default()
        }
    }

    fn clear(&mut self, scene: SceneId) {
        self.cleared = Some(scene);
        self.staged.remove(&scene);
        self.installed.remove(&scene);
    }

    /// Applies a frame if it's a chunk, and checks the cleared scene is still absent.
    fn apply(&mut self, frame: &Frame) -> bool {
        let Frame::Data(
            DataToShell::SceneSnapshot {
                scene,
                revision,
                chunk,
                last,
            },
            _,
        ) = frame
        else {
            return false;
        };
        self.chunks += 1;
        let named = self
            .named
            .iter()
            .any(|descriptor| descriptor.scene == *scene && descriptor.revision == *revision);
        if named && self.cleared != Some(*scene) {
            if self.installed.contains_key(scene) {
                self.failures
                    .push(format!("{scene:?} sent a chunk after its last"));
            }
            let staged = self.staged.entry(*scene).or_default();
            staged.extend(chunk.iter().cloned());
            if *last {
                let steps = self.staged.remove(scene).unwrap_or_default();
                self.installed.insert(*scene, steps);
            }
        }
        if let Some(cleared) = self.cleared
            && self.installed.contains_key(&cleared)
        {
            self.failures
                .push(format!("the cleared {cleared:?} installed"));
        }
        true
    }

    /// Exactly `scenes` are installed, each the fixture's whole drawing, and nothing is left
    /// staged short of its last chunk.
    fn expect(&mut self, scenes: &[SceneId], label: &str, when: &str) {
        let installed: Vec<SceneId> = self.installed.keys().copied().collect();
        if installed != scenes {
            self.failures
                .push(format!("{when}: {installed:?} installed, not {scenes:?}"));
        }
        for (scene, steps) in &self.installed {
            if steps.len() != MAX_STEPS || steps.iter().any(|step| step.label != label) {
                self.failures.push(format!(
                    "{when}: {scene:?} installed {} steps, not the fixture's {MAX_STEPS}",
                    steps.len()
                ));
            }
        }
        if !self.staged.is_empty() {
            self.failures.push(format!(
                "{when}: {:?} stopped short of their last chunk",
                self.staged.keys().collect::<Vec<_>>()
            ));
        }
    }

    fn scenes(&self) -> Vec<SceneId> {
        self.named
            .iter()
            .map(|descriptor| descriptor.scene)
            .collect()
    }
}

/// Takes `Scenes` and one chunk, sends `Stop` for `last` and clears the second scene, then
/// takes the rest.
fn recover(client: &mut Client, last: RunId, label: &str) -> Result<Applied> {
    let mut applied = Applied::new(scenes(client)?);
    if applied.named.len() != DRAWINGS {
        bail!(
            "{} scenes named, not the {DRAWINGS} drawn",
            applied.named.len()
        );
    }
    let cleared = applied.named[1].scene;
    client.allow_data(1);
    client.until(PATIENCE, |seen| applied.apply(&seen.frame).then_some(()))?;
    client.control(&ControlToBrain::Stop {
        run: last,
        revealed: vec![0],
    })?;
    client.control(&ControlToBrain::ClearDrawing {
        scene_ids: vec![cleared],
    })?;
    applied.clear(cleared);
    client.resume_data();
    let kept: Vec<SceneId> = applied
        .scenes()
        .into_iter()
        .filter(|scene| *scene != cleared)
        .collect();
    client.until(PATIENCE, |seen| {
        applied.apply(&seen.frame);
        kept.iter()
            .all(|scene| applied.installed.contains_key(scene))
            .then_some(())
    })?;
    let taken = client.log.len();
    client.drain(Duration::from_millis(500));
    for seen in &client.log[taken..] {
        applied.apply(&seen.frame);
    }
    applied.expect(&kept, label, "after the clear");
    Ok(applied)
}

/// Takes `Scenes` and every chunk it names, expecting `kept` unchanged.
fn settle(client: &mut Client, kept: &[SceneDescriptor], label: &str) -> Result<Applied> {
    let mut applied = Applied::new(scenes(client)?);
    if applied.named != kept {
        applied.failures.push(format!(
            "the next connection named {:?}, not {kept:?}",
            applied.named
        ));
    }
    let wanted = applied.named.len();
    if wanted > 0 {
        client.until(PATIENCE, |seen| {
            applied.apply(&seen.frame);
            (applied.installed.len() == wanted).then_some(())
        })?;
    }
    let scenes: Vec<SceneId> = kept.iter().map(|descriptor| descriptor.scene).collect();
    applied.expect(&scenes, label, "on the next connection");
    Ok(applied)
}

fn scenes(client: &mut Client) -> Result<Vec<SceneDescriptor>> {
    client.until(PATIENCE, |seen| match &seen.frame {
        Frame::Control(ControlToShell::Scenes { scenes }) => Some(scenes.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use capsula_protocol::message::{DataToShell, MAX_STEPS, SceneDescriptor, SceneId, Step};

    use super::Applied;
    use crate::slice2::client::Frame;

    fn chunk(scene: u64, steps: usize, label: &str) -> Frame {
        Frame::Data(
            DataToShell::SceneSnapshot {
                scene: SceneId(scene),
                revision: 1,
                chunk: vec![
                    Step {
                        label: label.to_owned(),
                    };
                    steps
                ],
                last: false,
            },
            0,
        )
    }

    fn last(mut frame: Frame) -> Frame {
        if let Frame::Data(DataToShell::SceneSnapshot { last, .. }, _) = &mut frame {
            *last = true;
        }
        frame
    }

    fn named(scenes: &[u64]) -> Applied {
        Applied::new(
            scenes
                .iter()
                .map(|scene| SceneDescriptor {
                    scene: SceneId(*scene),
                    revision: 1,
                    committed: 64,
                })
                .collect(),
        )
    }

    #[test]
    fn a_scene_installs_only_whole_at_its_last_chunk() {
        let mut applied = named(&[1, 2]);
        assert!(applied.apply(&chunk(1, MAX_STEPS - 1, "s")));
        assert!(applied.apply(&last(chunk(1, 1, "s"))));
        assert!(applied.apply(&chunk(2, 3, "s")));
        applied.expect(&[SceneId(1)], "s", "now");
        assert_eq!(
            applied.failures,
            ["now: [SceneId(2)] stopped short of their last chunk"]
        );
    }

    #[test]
    fn a_cleared_scene_never_installs_and_wrong_steps_fail() {
        let mut applied = named(&[1, 2]);
        applied.clear(SceneId(2));
        applied.apply(&last(chunk(2, MAX_STEPS, "s")));
        applied.apply(&last(chunk(1, MAX_STEPS, "other")));
        applied.apply(&last(chunk(3, MAX_STEPS, "s")));
        applied.expect(&[SceneId(1)], "s", "now");
        assert_eq!(
            applied.failures,
            ["now: SceneId(1) installed 64 steps, not the fixture's 64"]
        );
        assert!(!applied.installed.contains_key(&SceneId(2)), "cleared");
        assert!(!applied.installed.contains_key(&SceneId(3)), "never named");
    }
}
