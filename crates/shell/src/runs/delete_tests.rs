//! Deleting threads and refused turns, as [`Runs`] settles them.

use capsula_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, Deleted, Incarnation, MAX_PENDING_DELETES, Phase,
    RunId, SceneId, Step, ThreadId,
};

use super::Runs;

const BYTES: u32 = 10;

fn connected() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
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
        delta: "t".to_owned(),
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

fn state(runs: &mut Runs, run: u64, thread: u64, phase: Phase) -> Option<ControlToBrain> {
    runs.apply_state(RunId(run), ThreadId(thread), phase).stop
}

/// After a new brain, which thread the next turn names.
fn target_after_new_brain(runs: &mut Runs) -> Option<ThreadId> {
    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(2);
    submit(runs)
}

#[test]
fn a_new_thread_s_text_or_step_before_answering_proves_it_stored() {
    for proof in [text(1), step(1)] {
        let mut runs = connected();
        assert_eq!(submit(&mut runs), None);
        state(&mut runs, 1, 7, Phase::Thinking);
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
        submit(&mut runs);
        assert_eq!(runs.apply_data(proof.clone(), BYTES), 0, "held");
        assert_eq!(
            runs.apply_state(RunId(1), ThreadId(7), Phase::Thinking)
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
    submit(&mut runs);
    state(&mut runs, 1, 7, Phase::Thinking);
    assert_eq!(target_after_new_brain(&mut runs), None);
}

#[test]
fn an_older_run_s_data_never_proves_a_newer_target() {
    for late in [text(1), step(1)] {
        let mut runs = connected();
        submit(&mut runs);
        state(&mut runs, 1, 7, Phase::Thinking);
        runs.select_thread(None);
        submit(&mut runs);
        state(&mut runs, 2, 8, Phase::Thinking);
        runs.apply_data(late.clone(), BYTES);
        assert_eq!(target_after_new_brain(&mut runs), None, "{late:?}");
    }
}

#[test]
fn a_refusal_consumes_one_cancelled_turn_and_nothing_else() {
    let mut runs = connected();
    submit(&mut runs);
    submit(&mut runs);
    assert_eq!(runs.stop(), None, "no run is named yet");
    runs.refused();
    assert!(!runs.working(), "the turn left is cancelled too");
    assert_eq!(
        state(&mut runs, 3, 1, Phase::Thinking),
        Some(ControlToBrain::Stop {
            run: RunId(3),
            revealed: Vec::new(),
        }),
        "the second cancelled turn is still stopped"
    );
    submit(&mut runs);
    assert_eq!(state(&mut runs, 4, 1, Phase::Thinking), None);
    assert!(runs.working(), "a turn after the refusal isn't cancelled");
}

#[test]
fn a_refusal_consumes_one_selection_pending() {
    let mut runs = connected();
    submit(&mut runs);
    runs.select_thread(None);
    runs.refused();
    assert_eq!(submit(&mut runs), None);
    state(&mut runs, 5, 22, Phase::Thinking);
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
    runs.refused();
    assert!(!runs.working());
    assert_eq!(target_after_new_brain(&mut runs), Some(ThreadId(9)));

    let mut runs = connected();
    submit(&mut runs);
    state(&mut runs, 1, 7, Phase::Thinking);
    submit(&mut runs);
    runs.refused();
    assert_eq!(target_after_new_brain(&mut runs), None);
}

#[test]
fn a_refusal_with_nothing_pending_changes_nothing() {
    let mut runs = connected();
    runs.refused();
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
    state(&mut runs, 1, 9, Phase::Thinking);
    assert_eq!(runs.shown(), None, "run 1 is fenced");
    assert_eq!(
        runs.apply_state(RunId(2), ThreadId(3), Phase::Thinking)
            .credit,
        BYTES,
        "run 2's held step is released once"
    );
    assert_eq!(submit(&mut runs), Some(ThreadId(3)));
}

#[test]
fn a_turn_sent_before_its_thread_was_deleted_never_targets_it() {
    let mut runs = connected();
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs);
    assert!(runs.confirm_delete(ThreadId(9)));
    runs.deleted(ThreadId(9), Deleted::Removed, None);
    assert!(runs.holds(ThreadId(9)), "a turn sent before is unanswered");
    state(&mut runs, 4, 9, Phase::Answering);
    runs.apply_data(text(4), BYTES);
    assert_eq!(runs.shown(), None);
    assert!(!runs.working(), "its run is fenced");
    assert!(!runs.holds(ThreadId(9)));
    assert_eq!(submit(&mut runs), None);
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
        runs.apply_state(RunId(1), ThreadId(9), Phase::Thinking)
            .credit,
        BYTES
    );
    assert!(runs.shown().is_some(), "run 1 isn't fenced");
    assert!(
        runs.confirm_delete(ThreadId(9)),
        "it can be confirmed again"
    );
}

#[test]
fn deletes_and_threads_waiting_on_turns_share_the_slots() {
    let mut runs = connected();
    submit(&mut runs);
    for thread in 1..=MAX_PENDING_DELETES {
        assert!(runs.confirm_delete(ThreadId(thread as u64)));
    }
    assert!(!runs.can_delete());
    assert!(!runs.confirm_delete(ThreadId(99)));
    runs.deleted(ThreadId(1), Deleted::Hidden, None);
    assert!(!runs.can_delete(), "thread 1 waits on the turn sent before");
    state(&mut runs, 1, 5, Phase::Thinking);
    assert!(runs.can_delete());
    assert!(runs.confirm_delete(ThreadId(99)));
    runs.disconnected();
    assert_eq!(
        runs.deleting().len(),
        MAX_PENDING_DELETES,
        "unanswered deletes outlive the connection"
    );
}
