//! The hold lifecycle without windows: terminal notices, unstarted discards, run receipts across
//! the hold's end, a lost brain's words and a native Esc press's continuation.

use kumochi_protocol::message::{
    Content, GestureId, Incarnation, Phase, RunId, RunOrigin, ThreadId,
};

use super::{RunStep, Source, Stamp, State};
use crate::link::Session;
use crate::platform::microphone::CollectionGate;
use crate::runs::Applied;
use crate::strings::Text;

const STAMP: Stamp = Stamp {
    target: (Some(ThreadId(7)), 3),
    session: 4,
    choice: 2,
};

const HOLD: Option<RunOrigin> = Some(RunOrigin::Hold {
    gesture: GestureId(1),
});

/// A pending voice-only hold of gesture 1 on its own armed generation.
fn holding(source: Source) -> (State, CollectionGate) {
    let gate = CollectionGate::default();
    let token = gate.arm(GestureId(1)).expect("armed");
    let mut state = State::default();
    let session = Session::detached(STAMP.session, Incarnation(1));
    state.begin(token, STAMP, session, false, None, false);
    state.pending.as_mut().expect("begun").source = source;
    (state, gate)
}

fn visible() -> Applied {
    Applied {
        visible: true,
        ..Applied::default()
    }
}

#[test]
fn a_recording_hold_keeps_its_first_end_until_its_receipt() {
    let (mut state, _gate) = holding(Source::Recording);
    let first = state.end(Text::HoldCancelled);
    assert_eq!(first.gesture, Some(GestureId(1)));
    assert_eq!(first.notice, Text::HoldStopping);
    assert!(first.unused.is_none(), "a recording source has started");
    let lost = state.end(Text::HoldUnavailable);
    assert_eq!(lost.notice, Text::HoldUnavailable);
    let hold = state.pending.as_ref().expect("kept for its receipt");
    assert_eq!(hold.terminal(), Some(Text::HoldCancelled));
    assert_eq!(
        state.escape_owner(),
        None,
        "an ended hold no longer owns Esc"
    );
}

#[test]
fn an_unstarted_hold_is_handed_back_to_discard_once() {
    let (mut state, _gate) = holding(Source::Waiting);
    let ended = state.end(Text::HoldFailed);
    assert_eq!(
        ended.unused.map(|(token, _)| token.gesture()),
        Some(GestureId(1))
    );
    assert!(state.pending.is_none() && state.recovery.is_none());
    assert_eq!(state.ended.map(|(gesture, _)| gesture), Some(GestureId(1)));
    assert!(state.end(Text::HoldFailed).unused.is_none());
}

#[test]
fn an_answered_hold_hands_its_run_to_the_ended_receipt() {
    let (mut state, _gate) = holding(Source::Sent);
    let (run, thread) = (RunId(11), ThreadId(7));
    let think = state.run_state(run, thread, HOLD, Phase::Thinking, &visible());
    assert_eq!(think, RunStep::Thinking);
    let answer = state.run_state(run, thread, None, Phase::Answering, &visible());
    assert_eq!(answer, RunStep::Answered(GestureId(1)));
    assert!(state.pending.is_none() && state.status.is_none());
    state.run_state(run, thread, None, Phase::Done, &Applied::default());
    let (gesture, tail) = state.ended.expect("ended");
    let tail = tail.expect("named");
    assert_eq!(
        (gesture, tail.run, tail.control_closed),
        (GestureId(1), run, true)
    );
    let other = state.run_state(RunId(12), thread, None, Phase::Done, &visible());
    assert_eq!(other, RunStep::None);
}

#[test]
fn a_run_before_the_release_was_sent_fails_the_hold() {
    let (mut state, _gate) = holding(Source::Joined(2));
    let step = state.run_state(RunId(1), ThreadId(7), HOLD, Phase::Thinking, &visible());
    assert_eq!(step, RunStep::BeforeStop);
}

#[test]
fn a_lost_brain_restores_words_only_on_their_own_target() {
    let (mut state, _gate) = holding(Source::Sent);
    state.words(4, GestureId(1), Content::from("kept"));
    let elsewhere = |_: Content| -> bool { panic!("another target") };
    assert!(state.lose(|_| false, (Some(ThreadId(8)), 3), elsewhere));
    assert_eq!(state.lost, Some(Text::HoldLost));
    let (mut state, _gate) = holding(Source::Sent);
    state.words(4, GestureId(1), Content::from("kept"));
    assert!(state.lose(|_| false, STAMP.target, |words| words.as_str() == "kept"));
    assert_eq!(state.lost, Some(Text::HoldLostRestored));
    let (mut state, _gate) = holding(Source::Sent);
    assert!(
        !state.lose(|_| true, STAMP.target, elsewhere),
        "its Esc revoked it"
    );
    assert!(state.recovery.is_none() && state.lost.is_none());
}

#[test]
fn an_esc_press_stops_only_its_own_hold_run_while_it_still_moves() {
    let (mut state, _gate) = holding(Source::Sent);
    let run = RunId(11);
    state.run_state(run, ThreadId(7), HOLD, Phase::Answering, &visible());
    assert!(
        state.revoked(GestureId(2)).is_none(),
        "another gesture's press"
    );
    let press = state.revoked(GestureId(1)).expect("its own press");
    assert!(state.ended.is_none() && state.recovery.is_none());
    // The cancel the press applies may record the hold again; settling forgets it.
    state.ended = Some((GestureId(1), None));
    let stopped = press.settle(&mut state, Some(run));
    assert_eq!(stopped.map(|tail| tail.run), Some(run));
    assert!(state.ended.is_none());
    assert!(state.revoked(GestureId(1)).is_none(), "one press only");
}

#[test]
fn an_esc_press_leaves_a_run_that_stopped_moving() {
    let (mut state, _gate) = holding(Source::Sent);
    state.run_state(RunId(11), ThreadId(7), HOLD, Phase::Answering, &visible());
    let press = state.revoked(GestureId(1)).expect("its own press");
    assert!(press.settle(&mut state, Some(RunId(12))).is_none());
    let (mut state, _gate) = holding(Source::Recording);
    let press = state.revoked(GestureId(1)).expect("the pending hold");
    assert!(
        press.settle(&mut state, Some(RunId(11))).is_none(),
        "it named no run"
    );
}
