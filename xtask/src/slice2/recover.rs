//! Killing the brain mid-run and starting it again, and reconnecting to a multi-chunk drawing
//! while `Stop` and `ClearDrawing` arrive between its chunks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, MAX_STEP_LABEL, MAX_STEPS,
    PageEventKind, PageOf, Phase, RunId, SceneDescriptor, SceneId, Step,
};

use super::brain::Case;
use super::client::{Client, Frame, Seen};
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
         and ClearDrawing of the second scene after the first chunk. Scenes named 4 distinct \
         whole drawings (committed = revision = {MAX_STEPS}). By the shell's rules (install at \
         the last chunk only, with exactly the committed steps; a wrong revision spoils the scene, \
         an overrun fails; never a cleared scene), the other 3 installed whole \
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

/// The client's applied-message record, kept by the shell's rules (`drawing.rs`): a scene named by
/// `Scenes` stages its chunks and installs only at its last one, with exactly its committed steps; a
/// chunk at another revision spoils the scene for this connection, more steps than committed is an
/// overrun, and a scene cleared locally or not named never installs.
#[derive(Debug, Default)]
struct Applied {
    named: Vec<SceneDescriptor>,
    pending: BTreeMap<SceneId, Vec<Step>>,
    installed: BTreeMap<SceneId, Vec<Step>>,
    cleared: Option<SceneId>,
    chunks: usize,
    failures: Vec<String>,
}

impl Applied {
    /// Starts a connection's record, checking `named` is `scenes` of the fixture's drawings.
    fn new(named: Vec<SceneDescriptor>, scenes: usize, earlier: &[Seen]) -> Self {
        let mut failures = Vec::new();
        let unique: BTreeSet<SceneId> = named.iter().map(|descriptor| descriptor.scene).collect();
        if unique.len() != scenes || named.len() != scenes {
            failures.push(format!("{named:?} isn't {scenes} distinct scenes"));
        }
        let whole = u32::try_from(MAX_STEPS).unwrap_or(u32::MAX);
        for descriptor in &named {
            if descriptor.committed != whole || descriptor.revision != whole {
                failures.push(format!(
                    "{descriptor:?} isn't a whole drawing of {MAX_STEPS} steps"
                ));
            }
        }
        let mut applied = Self {
            pending: unique
                .into_iter()
                .map(|scene| (scene, Vec::new()))
                .collect(),
            named,
            failures,
            ..Self::default()
        };
        for seen in earlier {
            applied.apply(&seen.frame);
        }
        applied
    }

    fn clear(&mut self, scene: SceneId) {
        self.cleared = Some(scene);
        self.pending.remove(&scene);
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
        let descriptor = self
            .named
            .iter()
            .find(|descriptor| descriptor.scene == *scene)
            .copied();
        match (self.pending.get_mut(scene), descriptor) {
            (Some(staged), Some(descriptor)) => {
                let committed = usize::try_from(descriptor.committed).unwrap_or(usize::MAX);
                if *revision != descriptor.revision {
                    self.pending.remove(scene);
                    self.failures.push(format!(
                        "{scene:?} sent revision {revision}, not {}: its snapshot is spoiled",
                        descriptor.revision
                    ));
                } else if staged.len() + chunk.len() > committed {
                    self.pending.remove(scene);
                    self.failures
                        .push(format!("{scene:?} overran its {committed} committed steps"));
                } else {
                    staged.extend(chunk.iter().cloned());
                    if *last {
                        let steps = self.pending.remove(scene).unwrap_or_default();
                        if steps.len() == committed {
                            self.installed.insert(*scene, steps);
                        } else {
                            self.failures.push(format!(
                                "{scene:?} ended at {} of {committed} steps",
                                steps.len()
                            ));
                        }
                    }
                }
            }
            _ if self.installed.contains_key(scene) => self
                .failures
                .push(format!("{scene:?} sent a chunk after its last")),
            _ => {}
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
            if steps.len() != MAX_STEPS {
                self.failures.push(format!(
                    "{when}: {scene:?} installed {} steps, not the fixture's {MAX_STEPS}",
                    steps.len()
                ));
            }
            if steps.iter().any(|step| step.label != label) {
                self.failures.push(format!(
                    "{when}: {scene:?} installed steps whose label isn't the fixture's"
                ));
            }
        }
        if !self.pending.is_empty() {
            self.failures.push(format!(
                "{when}: {:?} stopped short of their last chunk",
                self.pending.keys().collect::<Vec<_>>()
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
    let mut applied = scenes(client, DRAWINGS)?;
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
    let mut applied = scenes(client, kept.len())?;
    if applied.named != kept {
        applied.failures.push(format!(
            "the next connection named {:?}, not {kept:?}",
            applied.named
        ));
    }
    let wanted = applied.named.len();
    if applied.installed.len() < wanted {
        client.until(PATIENCE, |seen| {
            applied.apply(&seen.frame);
            (applied.installed.len() == wanted).then_some(())
        })?;
    }
    let scenes: Vec<SceneId> = kept.iter().map(|descriptor| descriptor.scene).collect();
    applied.expect(&scenes, label, "on the next connection");
    Ok(applied)
}

fn scenes(client: &mut Client, expected: usize) -> Result<Applied> {
    let start = client.log.len();
    let named = client.until(PATIENCE, |seen| match &seen.frame {
        Frame::Control(ControlToShell::Scenes { scenes }) => Some(scenes.clone()),
        _ => None,
    })?;
    Ok(Applied::new(named, expected, &client.log[start..]))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use kumochi_protocol::message::{
        ControlToShell, DataToShell, MAX_STEPS, SceneDescriptor, SceneId, Step,
    };

    use super::Applied;
    use crate::slice2::client::{Frame, Seen};

    /// The fixture's whole drawing: every one of its steps committed.
    const WHOLE: u32 = 64;

    fn chunk(scene: u64, steps: usize, label: &str) -> Frame {
        Frame::Data(
            DataToShell::SceneSnapshot {
                scene: SceneId(scene),
                revision: WHOLE,
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

    fn at(mut frame: Frame, at: u32) -> Frame {
        if let Frame::Data(DataToShell::SceneSnapshot { revision, .. }, _) = &mut frame {
            *revision = at;
        }
        frame
    }

    fn described(scenes: &[u64], committed: u32) -> Applied {
        Applied::new(
            scenes
                .iter()
                .map(|scene| SceneDescriptor {
                    scene: SceneId(*scene),
                    revision: WHOLE,
                    committed,
                })
                .collect(),
            scenes.len(),
            &[],
        )
    }

    #[test]
    fn snapshots_before_scenes_and_split_around_it_install_all_three_exact_drawings() {
        let drawings = [
            (1, "a-first", "a-last"),
            (2, "b-first", "b-last"),
            (3, "c-first", "c-last"),
        ];
        for before in [3, 2] {
            let named: Vec<_> = drawings
                .iter()
                .map(|&(id, _, _)| SceneDescriptor {
                    scene: SceneId(id),
                    revision: WHOLE,
                    committed: WHOLE,
                })
                .collect();
            let mut frames: Vec<_> = drawings
                .iter()
                .map(|&(id, first, tail)| {
                    let mut frame = last(chunk(id, MAX_STEPS, first));
                    let Frame::Data(DataToShell::SceneSnapshot { chunk, .. }, _) = &mut frame
                    else {
                        unreachable!("the fixture builds snapshot chunks");
                    };
                    chunk[MAX_STEPS - 1].label = tail.to_owned();
                    frame
                })
                .collect();
            frames.insert(
                before,
                Frame::Control(ControlToShell::Scenes {
                    scenes: named.clone(),
                }),
            );
            let seen: Vec<Seen> = frames
                .into_iter()
                .map(|frame| Seen {
                    at: Instant::now(),
                    frame,
                })
                .collect();
            let mut applied = Applied::new(named, 3, &seen[..=before]);
            for next in &seen[before + 1..] {
                applied.apply(&next.frame);
            }
            assert!(applied.failures.is_empty(), "{:?}", applied.failures);
            assert!(
                applied.pending.is_empty(),
                "completed drawings are no longer staged"
            );
            assert_eq!(applied.installed.len(), drawings.len(), "no extra scenes");
            for (id, first, tail) in drawings {
                assert!(
                    applied.installed[&SceneId(id)]
                        .iter()
                        .map(|step| step.label.as_str())
                        .eq(std::iter::repeat_n(first, MAX_STEPS - 1).chain([tail])),
                    "scene {id}'s exact steps, with {before} snapshots before Scenes"
                );
            }
        }
    }

    #[test]
    fn a_scene_installs_only_whole_at_its_last_chunk() {
        let mut applied = described(&[1, 2], WHOLE);
        assert!(applied.apply(&chunk(1, MAX_STEPS - 1, "s")));
        assert!(
            applied.installed.is_empty(),
            "the split drawing is not complete"
        );
        assert!(applied.apply(&last(chunk(1, 1, "s"))));
        assert!(applied.apply(&chunk(2, MAX_STEPS, "s")));
        assert!(
            applied.installed.keys().copied().eq([SceneId(1)]),
            "all steps without a last chunk do not install scene 2"
        );
        assert!(
            applied.installed[&SceneId(1)]
                .iter()
                .map(|step| step.label.as_str())
                .eq(std::iter::repeat_n("s", MAX_STEPS)),
            "the completed split installs all of scene 1"
        );
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        applied.expect(&[SceneId(1)], "s", "now");
        assert!(
            !applied.failures.is_empty(),
            "a scene still missing its final chunk invalidates the gate"
        );
    }

    #[test]
    fn a_cleared_scene_never_installs_and_wrong_labels_fail() {
        let mut applied = described(&[1, 2], WHOLE);
        applied.clear(SceneId(2));
        applied.apply(&last(chunk(2, MAX_STEPS, "s")));
        applied.apply(&last(chunk(1, MAX_STEPS, "other")));
        applied.apply(&last(chunk(3, MAX_STEPS, "s")));
        assert!(
            applied.installed[&SceneId(1)]
                .iter()
                .map(|step| step.label.as_str())
                .eq(std::iter::repeat_n("other", MAX_STEPS)),
            "scene 1 was installed with the wrong label"
        );
        applied.expect(&[SceneId(1)], "s", "now");
        let [wrong_label] = applied.failures.as_slice() else {
            panic!("expected exactly one label failure: {:?}", applied.failures);
        };
        assert!(
            wrong_label.contains("label") && numbers(wrong_label).eq(["1"]),
            "{wrong_label}"
        );
        assert!(!applied.installed.contains_key(&SceneId(2)), "cleared");
        assert!(!applied.installed.contains_key(&SceneId(3)), "never named");
    }

    #[test]
    fn a_whole_payload_over_a_short_commit_overruns_and_never_installs() {
        let mut applied = described(&[7], 1);
        applied.apply(&last(chunk(7, MAX_STEPS, "s")));
        applied.expect(&[SceneId(7)], "s", "now");
        assert!(applied.installed.is_empty(), "the shell restores nothing");
        assert!(
            applied.failures.iter().any(|report| {
                let mut values: Vec<_> = numbers(report).collect();
                values.sort_unstable();
                let fields: Vec<_> = report
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .filter(|part| !part.is_empty())
                    .collect();
                report.contains("whole drawing")
                    && values == ["1", "64", "64", "7"]
                    && fields.windows(2).any(|field| field == ["revision", "64"])
                    && fields.windows(2).any(|field| field == ["committed", "1"])
            }),
            "{:?}",
            applied.failures
        );
        assert!(
            applied.failures.iter().any(|report| {
                report.contains("overran")
                    && report.contains("1 committed steps")
                    && numbers(report).eq(["7", "1"])
            }),
            "{:?}",
            applied.failures
        );
        assert_missing_scene(&applied.failures, "7");
    }

    #[test]
    fn a_wrong_revision_spoils_the_scene_for_the_connection() {
        let mut applied = described(&[1], WHOLE);
        applied.apply(&at(chunk(1, 1, "s"), WHOLE - 1));
        applied.apply(&last(chunk(1, MAX_STEPS, "s")));
        applied.expect(&[SceneId(1)], "s", "now");
        assert!(
            applied.installed.is_empty(),
            "the later chunk can't install"
        );
        assert!(
            applied.failures.iter().any(|report| {
                report.contains("sent revision 63")
                    && report.contains("not 64")
                    && numbers(report).eq(["1", "63", "64"])
            }),
            "{:?}",
            applied.failures
        );
        assert_missing_scene(&applied.failures, "1");
    }

    fn numbers(report: &str) -> impl Iterator<Item = &str> {
        report
            .split(|ch: char| !ch.is_ascii_digit())
            .filter(|word| !word.is_empty())
    }

    fn assert_missing_scene(failures: &[String], scene: &str) {
        assert!(
            failures.iter().any(|report| {
                report.contains("installed")
                    && report.contains("not")
                    && numbers(report).eq([scene])
            }),
            "{failures:?}"
        );
    }
}
