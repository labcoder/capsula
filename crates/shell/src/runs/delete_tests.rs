//! Deleting threads and refused turns, as [`Runs`] settles them.

use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, Deleted, Incarnation, MAX_PENDING_DELETES, Phase,
    RunId, RunOrigin, SceneDescriptor, SceneId, Step, ThreadId,
};

use super::{Runs, Shown};
use crate::drawing::Chunk;

const BYTES: u32 = 10;

#[test]
fn keyed_refusals_shrink_only_the_deletion_prefixes_that_include_them() {
    let mut runs = connected();
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(9)));
    runs.deleted(ThreadId(9), Deleted::Removed, None);
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(8)));
    runs.deleted(ThreadId(8), Deleted::Hidden, None);
    submit(&mut runs);
    assert_eq!(runs.gone, [(ThreadId(9), 1), (ThreadId(8), 2)]);
    runs.refused(3, Some(ThreadId(1)));
    assert_eq!(runs.gone, [(ThreadId(9), 1), (ThreadId(8), 2)]);
    runs.refused(2, Some(ThreadId(1)));
    assert_eq!(runs.gone, [(ThreadId(9), 1), (ThreadId(8), 1)]);
    assert_eq!(runs.submitted.len(), 1);
    assert_eq!(runs.submitted[0].turn, 1);
    runs.refused(2, Some(ThreadId(1)));
    runs.refused(1, None);
    assert_eq!(runs.gone, [(ThreadId(9), 1), (ThreadId(8), 1)]);
    runs.refused(1, Some(ThreadId(1)));
    assert!(runs.gone.is_empty());
    assert!(runs.submitted.is_empty());
}

fn connected() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    runs
}

fn submit(runs: &mut Runs) -> Option<ThreadId> {
    let mut sent = None;
    runs.submit("typed", |turn| {
        sent = Some(turn);
        true
    })
    .expect("sent");
    let Some(DataToBrain::TextSubmit { thread, .. }) = sent else {
        panic!("a turn was sent")
    };
    thread
}

fn text(run: u64) -> DataToShell {
    DataToShell::AssistantText {
        run: RunId(run),
        delta: "t".into(),
    }
}

fn step(run: u64) -> DataToShell {
    DataToShell::StepCommit {
        run: RunId(run),
        scene: SceneId(run),
        index: 0,
        step: Step {
            label: "s".to_owned(),
        },
    }
}

fn state(
    runs: &mut Runs,
    run: u64,
    thread: u64,
    phase: Phase,
    origin: Option<RunOrigin>,
) -> Option<ControlToBrain> {
    runs.apply_state(RunId(run), ThreadId(thread), phase, origin)
        .stop
}

/// After a new brain, which thread the next turn names.
fn target_after_new_brain(runs: &mut Runs) -> Option<ThreadId> {
    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(2);
    runs.settings(ThreadId(1));
    submit(runs)
}

#[test]
fn a_new_thread_s_text_or_step_before_answering_proves_it_stored() {
    for proof in [text(1), step(1)] {
        let mut runs = connected();
        runs.select_thread(None);
        assert_eq!(submit(&mut runs), None);
        state(
            &mut runs,
            1,
            7,
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        runs.apply_data(proof.clone(), BYTES);
        assert_eq!(
            target_after_new_brain(&mut runs),
            Some(ThreadId(7)),
            "{proof:?}"
        );
    }
}

#[test]
fn held_data_released_by_its_run_state_proves_the_thread_stored() {
    for proof in [text(1), step(1)] {
        let mut runs = connected();
        runs.select_thread(None);
        submit(&mut runs);
        assert_eq!(runs.apply_data(proof.clone(), BYTES), 0, "held");
        assert_eq!(
            runs.apply_state(
                RunId(1),
                ThreadId(7),
                Phase::Thinking,
                Some(RunOrigin::Typed { turn: 1 })
            )
            .credit,
            BYTES
        );
        assert_eq!(
            target_after_new_brain(&mut runs),
            Some(ThreadId(7)),
            "{proof:?}"
        );
    }
}

#[test]
fn an_unproved_new_thread_is_dropped_by_a_new_brain() {
    let mut runs = connected();
    runs.select_thread(None);
    submit(&mut runs);
    state(
        &mut runs,
        1,
        7,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert_eq!(target_after_new_brain(&mut runs), Some(ThreadId(1)));
}

#[test]
fn an_older_run_s_data_never_proves_a_newer_target() {
    for late in [text(1), step(1)] {
        let mut runs = connected();
        runs.select_thread(None);
        submit(&mut runs);
        state(
            &mut runs,
            1,
            7,
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        runs.select_thread(None);
        submit(&mut runs);
        state(
            &mut runs,
            2,
            8,
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 2 }),
        );
        runs.apply_data(late.clone(), BYTES);
        assert_eq!(
            target_after_new_brain(&mut runs),
            Some(ThreadId(1)),
            "{late:?}"
        );
    }
}

#[test]
fn a_refusal_consumes_one_cancelled_turn_and_nothing_else() {
    let mut runs = connected();
    submit(&mut runs);
    submit(&mut runs);
    assert_eq!(runs.stop(), None, "no run is named yet");
    runs.refused(1, Some(ThreadId(1)));
    assert!(!runs.working(), "the turn left is cancelled too");
    assert_eq!(
        state(
            &mut runs,
            3,
            1,
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 2 })
        ),
        Some(ControlToBrain::Stop {
            run: RunId(3),
            revealed: Vec::new(),
        }),
        "the second cancelled turn is still stopped"
    );
    submit(&mut runs);
    assert_eq!(
        state(
            &mut runs,
            4,
            1,
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 3 })
        ),
        None
    );
    assert!(runs.working(), "a turn after the refusal isn't cancelled");
}

#[test]
fn a_refusal_consumes_one_selection_pending() {
    let mut runs = connected();
    submit(&mut runs);
    runs.select_thread(None);
    runs.refused(1, Some(ThreadId(1)));
    assert_eq!(submit(&mut runs), None);
    state(
        &mut runs,
        5,
        22,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 2 }),
    );
    assert_eq!(
        submit(&mut runs),
        Some(ThreadId(22)),
        "the refused turn no longer guards the selection"
    );
}

#[test]
fn a_refusal_leaves_the_target_and_its_durability_alone() {
    let mut runs = connected();
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs);
    runs.refused(1, Some(ThreadId(9)));
    assert!(!runs.working());
    assert_eq!(target_after_new_brain(&mut runs), Some(ThreadId(9)));

    let mut runs = connected();
    runs.select_thread(None);
    submit(&mut runs);
    state(
        &mut runs,
        1,
        7,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    submit(&mut runs);
    runs.refused(2, Some(ThreadId(7)));
    assert_eq!(target_after_new_brain(&mut runs), Some(ThreadId(1)));
}

#[test]
fn a_refusal_with_nothing_pending_changes_nothing() {
    let mut runs = connected();
    runs.refused(1, Some(ThreadId(1)));
    submit(&mut runs);
    assert!(runs.working());
}

#[test]
fn a_confirmed_delete_untargets_the_thread_and_a_hide_fences_its_held_frames_once() {
    let mut runs = connected();
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs);
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(9)));
    assert!(!runs.confirm_delete(ThreadId(9)), "it already holds a slot");
    assert_eq!(runs.apply_data(text(1), BYTES), 0);
    assert_eq!(runs.apply_data(step(2), BYTES), 0);
    assert_eq!(
        runs.deleted(ThreadId(9), Deleted::Hidden, Some(RunId(1))),
        BYTES
    );
    assert_eq!(
        runs.deleted(ThreadId(9), Deleted::Hidden, Some(RunId(1))),
        0
    );
    state(
        &mut runs,
        1,
        9,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert_eq!(runs.shown(), None, "run 1 is fenced");
    assert_eq!(
        runs.apply_state(
            RunId(2),
            ThreadId(9),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 2 })
        )
        .credit,
        BYTES,
        "run 2's held step is released once"
    );
    assert_eq!(submit(&mut runs), Some(ThreadId(1)));
}

#[test]
fn a_removal_clears_a_view_whose_owner_is_unknown_and_spares_the_next_run() {
    let (t, u) = (ThreadId(9), ThreadId(3));
    let mut runs = connected();
    runs.select_thread(Some(t));
    submit(&mut runs);
    state(
        &mut runs,
        1,
        9,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert!(runs.select_thread(Some(u)));
    submit(&mut runs);
    state(
        &mut runs,
        2,
        3,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 2 }),
    );
    runs.apply_data(text(1), BYTES);
    assert_eq!(
        runs.shown().map(|shown| (shown.run, shown.thread)),
        Some((RunId(1), None)),
        "run 2 superseded run 1 before its text"
    );
    assert!(runs.confirm_delete(t));
    runs.deleted(t, Deleted::Removed, Some(RunId(1)));
    assert_eq!(runs.shown(), None, "T's text may be in that view");
    runs.apply_data(text(2), BYTES);
    assert_eq!(
        runs.shown(),
        Some(&Shown {
            run: RunId(2),
            thread: Some(u),
            text: "t".to_owned(),
            cut: false,
            done: false,
        })
    );
    assert_eq!(
        runs.stop(),
        Some(ControlToBrain::Stop {
            run: RunId(2),
            revealed: Vec::new(),
        })
    );
}

#[test]
fn a_thread_being_deleted_cant_be_selected() {
    let (t, u) = (ThreadId(9), ThreadId(3));
    let mut runs = connected();
    assert!(runs.select_thread(Some(u)));
    assert!(runs.confirm_delete(t));
    assert!(
        !runs.select_thread(Some(t)),
        "the caller leaves the open thread on U"
    );
    assert_eq!(submit(&mut runs), Some(u));
}

#[test]
fn a_turn_sent_before_its_thread_was_deleted_never_targets_it() {
    let mut runs = connected();
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(9)));
    runs.deleted(ThreadId(9), Deleted::Removed, None);
    assert!(runs.holds(ThreadId(9)), "a turn sent before is unanswered");
    state(
        &mut runs,
        4,
        9,
        Phase::Answering,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    runs.apply_data(text(4), BYTES);
    assert_eq!(runs.shown(), None);
    assert!(!runs.working(), "its run is fenced");
    assert!(!runs.holds(ThreadId(9)));
    assert_eq!(submit(&mut runs), Some(ThreadId(1)));
}

#[test]
fn a_delete_that_hid_nothing_frees_its_slot_and_fences_nothing() {
    let mut runs = connected();
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.apply_data(text(1), BYTES), 0);
    assert_eq!(
        runs.deleted(ThreadId(9), Deleted::NotHidden, Some(RunId(1))),
        0
    );
    assert!(!runs.holds(ThreadId(9)));
    assert_eq!(
        runs.apply_state(
            RunId(1),
            ThreadId(9),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 })
        )
        .credit,
        BYTES
    );
    assert_eq!(
        runs.shown(),
        Some(&Shown {
            run: RunId(1),
            thread: Some(ThreadId(9)),
            text: "t".to_owned(),
            cut: false,
            done: false,
        }),
        "run 1 isn't fenced"
    );
    assert!(
        runs.confirm_delete(ThreadId(9)),
        "it can be confirmed again"
    );
}

#[test]
fn deletes_and_threads_waiting_on_turns_share_the_slots() {
    let mut runs = connected();
    submit(&mut runs);
    for thread in (1..).take(MAX_PENDING_DELETES) {
        assert!(runs.confirm_delete(ThreadId(thread)));
    }
    assert!(!runs.can_delete());
    assert!(!runs.confirm_delete(ThreadId(99)));
    runs.deleted(ThreadId(1), Deleted::Hidden, None);
    assert!(!runs.can_delete(), "thread 1 waits on the turn sent before");
    state(
        &mut runs,
        1,
        1,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert!(runs.can_delete());
    assert!(runs.confirm_delete(ThreadId(99)));
    runs.disconnected();
    assert_eq!(
        runs.deleting().len(),
        MAX_PENDING_DELETES,
        "unanswered deletes outlive the connection"
    );
}

#[test]
fn an_answer_for_the_shown_thread_drops_its_view_and_a_newer_run_s_stays() {
    let mut runs = connected();
    assert!(runs.select_thread(Some(ThreadId(9))));
    submit(&mut runs);
    state(
        &mut runs,
        1,
        9,
        Phase::Answering,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    runs.apply_data(text(1), BYTES);
    assert!(runs.select_thread(Some(ThreadId(4))));
    submit(&mut runs);
    state(&mut runs, 1, 9, Phase::Done, None);
    runs.apply_data(DataToShell::RunEnd { run: RunId(1) }, BYTES);
    state(
        &mut runs,
        2,
        4,
        Phase::Answering,
        Some(RunOrigin::Typed { turn: 2 }),
    );
    assert_eq!(runs.shown().map(|shown| shown.run), Some(RunId(1)));
    assert!(runs.confirm_delete(ThreadId(9)));
    runs.deleted(ThreadId(9), Deleted::Removed, Some(RunId(1)));
    assert_eq!(runs.shown(), None, "T's answer goes with the result");
    runs.apply_data(text(1), BYTES);
    assert_eq!(runs.shown(), None, "T's late text shows nothing");
    runs.apply_data(text(2), BYTES);
    assert_eq!(
        runs.shown().map(|shown| (shown.run, shown.text.as_str())),
        Some((RunId(2), "t"))
    );
    assert!(runs.working(), "U is still live");
}

#[test]
fn an_answer_for_another_thread_keeps_the_view() {
    let mut runs = connected();
    assert!(runs.select_thread(Some(ThreadId(4))));
    submit(&mut runs);
    state(
        &mut runs,
        2,
        4,
        Phase::Answering,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    runs.apply_data(text(2), BYTES);
    assert!(runs.confirm_delete(ThreadId(9)));
    runs.deleted(ThreadId(9), Deleted::Removed, Some(RunId(1)));
    runs.apply_data(text(2), BYTES);
    assert_eq!(
        runs.shown().map(|shown| (shown.run, shown.text.as_str())),
        Some((RunId(2), "tt"))
    );
}

fn restored(scene: u64) -> SceneDescriptor {
    SceneDescriptor {
        scene: SceneId(scene),
        revision: 1,
        committed: 1,
    }
}

fn chunk(scene: u64) -> Chunk {
    Chunk {
        scene: SceneId(scene),
        revision: 1,
        steps: vec![Step {
            label: "s".to_owned(),
        }],
        last: true,
        bytes: BYTES,
    }
}

#[test]
fn a_chunk_held_across_the_answer_installs_and_leaves_at_the_marker() {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[restored(5)]).expect("valid");
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.deleted(ThreadId(9), Deleted::Removed, None), 0);
    assert_eq!(runs.apply_snapshot(1, chunk(5)), Ok(BYTES));
    assert_eq!(runs.scenes().count(), 1, "installed until the marker");
    assert_eq!(
        runs.remove_scenes(&[SceneId(5)]),
        0,
        "its credit was granted"
    );
    assert_eq!(runs.scenes().count(), 0);
}

#[test]
fn a_chunk_held_for_scenes_is_dropped_by_the_marker_with_its_credit_once() {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    assert_eq!(runs.apply_snapshot(1, chunk(5)), Ok(0), "held for Scenes");
    assert_eq!(runs.remove_scenes(&[SceneId(5)]), BYTES);
    assert_eq!(runs.remove_scenes(&[SceneId(5)]), 0);
    runs.apply_scenes(&[]).expect("valid");
    assert_eq!(runs.scenes().count(), 0);
}

#[test]
fn a_removed_scene_s_late_chunk_never_installs() {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[restored(5)]).expect("valid");
    assert_eq!(runs.remove_scenes(&[SceneId(5)]), 0);
    assert_eq!(runs.apply_snapshot(1, chunk(5)), Ok(BYTES));
    assert_eq!(runs.scenes().count(), 0);
}
