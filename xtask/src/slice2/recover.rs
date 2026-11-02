//! Killing the brain mid-run and starting it again, and reconnecting to a multi-chunk drawing
//! while `Stop` and `ClearDrawing` arrive between its chunks.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use capsula_protocol::message::{
    ControlToBrain, ControlToShell, DataToBrain, DataToShell, MAX_STEP_LABEL, MAX_STEPS,
    PageEventKind, PageOf, Phase, RunId, SceneDescriptor, SceneId,
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

/// Each visible scene's descriptor and the steps its chunks carried, in order.
#[derive(Debug, Default)]
struct Recovered {
    descriptors: Vec<SceneDescriptor>,
    chunks: usize,
    steps: BTreeMap<SceneId, usize>,
    /// Scenes whose last chunk arrived.
    complete: Vec<SceneId>,
    failures: Vec<String>,
}

pub(super) fn reconnect(folder: &Path) -> Result<Row> {
    let label = "s".repeat(MAX_STEP_LABEL);
    let script = format!("0 step {label}\n").repeat(MAX_STEPS);
    let case = Case::new(folder, "reconnect", &script)?;
    let brain = case.start()?;
    let mut client = Client::connect()?;
    let mut last = RunId(0);
    let mut thread = None;
    for _ in 0..4 {
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
    let mut second = recover(&mut client, last)?;
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
    let mut client = Client::connect()?;
    let third = settle(&mut client)?;
    client.quit()?;
    brain.exited()?;
    let cleared = second.descriptors.get(1).map(|descriptor| descriptor.scene);
    let kept: Vec<_> = second
        .descriptors
        .iter()
        .filter(|descriptor| Some(descriptor.scene) != cleared)
        .copied()
        .collect();
    if third.descriptors != kept {
        second.failures.push(format!(
            "after the clear and the stale Stop the scenes are {:?}, not {kept:?}",
            third.descriptors
        ));
    }
    if second.chunks < 3 {
        second
            .failures
            .push(format!("{} snapshot chunks, not 3 or more", second.chunks));
    }
    second.failures.extend(third.failures);
    let detail = format!(
        "4 scenes of {MAX_STEPS} steps × {MAX_STEP_LABEL} B, {} chunks; Stop for run {} and \
         ClearDrawing of one scene sent after the first chunk; every installed scene was \
         complete at its last chunk, none partial; the next connection named {} scenes, without \
         the cleared one, each unchanged and complete. One scene can't reach 3 chunks today \
         (32 KiB target, at most {MAX_STEPS} × {MAX_STEP_LABEL} B), so the chunks span scenes",
        second.chunks,
        last.0,
        third.descriptors.len()
    );
    Ok(Row::checked(
        "Reconnect with a multi-chunk drawing while Stop arrives",
        &second.failures,
        detail,
    ))
}

/// Takes `Scenes` and one chunk, sends `Stop` for `last` and clears the second scene, then
/// takes the rest.
fn recover(client: &mut Client, last: RunId) -> Result<Recovered> {
    let descriptors = scenes(client)?;
    let cleared = descriptors
        .get(1)
        .map(|descriptor| descriptor.scene)
        .context("fewer than two scenes are visible")?;
    client.allow_data(1);
    let mut recovered = Recovered {
        descriptors,
        ..Recovered::default()
    };
    client.until(PATIENCE, |seen| take(&mut recovered, &seen.frame))?;
    client.control(&ControlToBrain::Stop {
        run: last,
        revealed: vec![0],
    })?;
    client.control(&ControlToBrain::ClearDrawing {
        scene_ids: vec![cleared],
    })?;
    client.resume_data();
    let wanted: Vec<SceneId> = recovered
        .descriptors
        .iter()
        .map(|descriptor| descriptor.scene)
        .filter(|scene| *scene != cleared)
        .collect();
    client.until(PATIENCE, |seen| {
        take(&mut recovered, &seen.frame);
        wanted
            .iter()
            .all(|scene| recovered.complete.contains(scene))
            .then_some(())
    })?;
    let taken = client.log.len();
    client.drain(Duration::from_millis(500));
    for seen in &client.log[taken..] {
        let _late = take(&mut recovered, &seen.frame);
    }
    check(&mut recovered, Some(cleared));
    Ok(recovered)
}

/// Takes `Scenes` and every chunk it names.
fn settle(client: &mut Client) -> Result<Recovered> {
    let descriptors = scenes(client)?;
    let wanted = descriptors.len();
    let mut recovered = Recovered {
        descriptors,
        ..Recovered::default()
    };
    if wanted > 0 {
        client.until(PATIENCE, |seen| {
            take(&mut recovered, &seen.frame);
            (recovered.complete.len() == wanted).then_some(())
        })?;
    }
    check(&mut recovered, None);
    Ok(recovered)
}

fn scenes(client: &mut Client) -> Result<Vec<SceneDescriptor>> {
    client.until(PATIENCE, |seen| match &seen.frame {
        Frame::Control(ControlToShell::Scenes { scenes }) => Some(scenes.clone()),
        _ => None,
    })
}

/// Applies one chunk as the shell would: a scene installs only at its last chunk.
fn take(recovered: &mut Recovered, frame: &Frame) -> Option<()> {
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
        return None;
    };
    recovered.chunks += 1;
    let named = recovered
        .descriptors
        .iter()
        .find(|descriptor| descriptor.scene == *scene);
    if named.is_none_or(|descriptor| descriptor.revision != *revision) {
        recovered.failures.push(format!(
            "a chunk for {scene:?} at revision {revision} wasn't named"
        ));
    }
    if recovered.complete.contains(scene) {
        recovered
            .failures
            .push(format!("{scene:?} sent a chunk after its last"));
    }
    *recovered.steps.entry(*scene).or_default() += chunk.len();
    if *last {
        recovered.complete.push(*scene);
    }
    Some(())
}

/// Every scene installed carries exactly the steps its descriptor keeps; a scene the client
/// cleared is dropped, as the shell drops it.
fn check(recovered: &mut Recovered, cleared: Option<SceneId>) {
    for descriptor in &recovered.descriptors {
        if Some(descriptor.scene) == cleared {
            continue;
        }
        let steps = recovered.steps.get(&descriptor.scene).copied().unwrap_or(0);
        let committed = usize::try_from(descriptor.committed).unwrap_or(usize::MAX);
        if !recovered.complete.contains(&descriptor.scene) || steps != committed {
            recovered.failures.push(format!(
                "{:?} installed {steps} of {committed} steps",
                descriptor.scene
            ));
        }
    }
}
