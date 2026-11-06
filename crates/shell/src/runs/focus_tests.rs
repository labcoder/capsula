//! A run's `Focus`, as [`Runs`] fences it.

use kumochi_protocol::message::{Incarnation, Phase, RunId, ThreadId};

use super::{Runs, Target};

const KUMO: ThreadId = ThreadId(1);
const TASK: ThreadId = ThreadId(9);

fn submit(runs: &mut Runs, typed: &str) {
    runs.submit(typed, |_| true).expect("sent");
}

/// Connected to Kumo, with `turns` typed turns sent from Kumo's thread.
fn typed(turns: usize) -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(KUMO);
    for _ in 0..turns {
        submit(&mut runs, "go there");
    }
    runs
}

fn named(runs: &mut Runs, run: u64) {
    runs.apply_state(RunId(run), KUMO, Phase::Thinking);
}

#[test]
fn the_newest_typed_run_moves_the_input_and_a_stopped_one_does_not() {
    let mut runs = typed(1);
    named(&mut runs, 1);
    let revision = runs.revision();
    assert!(runs.focus(RunId(1), TASK));
    assert_eq!(runs.target(), Target::Task(TASK));
    assert_eq!(runs.revision(), revision + 1);

    let mut runs = typed(1);
    named(&mut runs, 1);
    assert!(runs.stop().is_some());
    assert!(!runs.focus(RunId(1), TASK));
    assert_eq!(runs.target(), Target::Kumo);
}

#[test]
fn with_two_turns_pending_only_the_run_answering_the_last_moves_the_input() {
    let mut runs = typed(2);
    named(&mut runs, 1);
    assert!(!runs.focus(RunId(1), TASK), "a turn was sent after its own");
    assert_eq!(runs.target(), Target::Kumo);
    named(&mut runs, 2);
    assert!(!runs.focus(RunId(1), TASK), "no longer the newest run");
    assert!(runs.focus(RunId(2), TASK));
    assert_eq!(runs.target(), Target::Task(TASK));
}

#[test]
fn a_turn_sent_after_the_run_was_named_makes_its_focus_stale() {
    let mut runs = typed(1);
    named(&mut runs, 1);
    submit(&mut runs, "never mind");
    assert!(!runs.focus(RunId(1), TASK));
    assert_eq!(runs.target(), Target::Kumo);
}

#[test]
fn a_newer_turn_the_brain_refused_still_makes_the_old_focus_stale() {
    let mut runs = typed(1);
    named(&mut runs, 1);
    submit(&mut runs, "a newer turn");
    runs.refused();
    let revision = runs.revision();
    assert!(!runs.focus(RunId(1), TASK));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.revision(), revision);
    submit(&mut runs, "again");
    named(&mut runs, 2);
    assert!(runs.focus(RunId(2), TASK));
    assert_eq!(runs.target(), Target::Task(TASK));
}

#[test]
fn a_manual_switch_or_an_unsolicited_run_keeps_the_input_where_it_is() {
    let mut runs = typed(1);
    named(&mut runs, 1);
    assert!(runs.select_thread(Some(ThreadId(7))));
    assert!(!runs.focus(RunId(1), TASK));
    assert_eq!(runs.target(), Target::Task(ThreadId(7)));

    let mut runs = typed(0);
    named(&mut runs, 1);
    assert!(
        !runs.focus(RunId(1), TASK),
        "no turn of this connection named it"
    );
    assert_eq!(runs.target(), Target::Kumo);
}

#[test]
fn a_reconnect_fences_the_last_connections_run_and_its_numbering_restarts() {
    let mut runs = typed(1);
    named(&mut runs, 1);
    runs.disconnected();
    runs.met(Incarnation(1));
    runs.connected(2);
    runs.settings(KUMO);
    assert!(!runs.focus(RunId(1), TASK), "the same brain's old run");
    submit(&mut runs, "again");
    named(&mut runs, 2);
    assert!(runs.focus(RunId(2), TASK), "turn 1 of the new connection");

    let mut runs = typed(1);
    named(&mut runs, 3);
    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(2);
    runs.settings(KUMO);
    assert!(!runs.focus(RunId(3), TASK), "a new brain hasn't named it");
    submit(&mut runs, "again");
    named(&mut runs, 3);
    assert!(
        runs.focus(RunId(3), TASK),
        "the new brain's run reusing the id"
    );
}
