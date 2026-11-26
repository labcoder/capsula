//! The hold lifecycle without windows: terminal notices, unstarted discards, run receipts across
//! the hold's end, a lost brain's words and a native Esc press's continuation; then capture:
//! brain statuses, admission order, release authority, the screen release's vetoes and the
//! physical receipt.

use std::cell::Cell;
use std::num::NonZeroU64;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{
    AudioSession, Content, ControlToBrain, GestureId, HoldFailure, HoldStatus, Incarnation,
    NoticeRevision, Phase, RunId, RunOrigin, ThreadId,
};

use super::{
    Cancel, Collection, Discard, Hold, Offer, ReceiptStep, Recognition, Refused, Release, RunStep,
    Source, Stamp, State, StatusStep, Unsent, VoiceOnly, discard,
};
use crate::link::Session;
use crate::link::output::Denied;
use crate::platform::microphone::{CollectionGate, Finalization, MicError};
use crate::runs::{Applied, Refused as RunRefused};
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
fn a_cancel_ends_a_pending_hold_or_clears_a_notice_without_one() {
    let (mut state, _gate) = holding(Source::Waiting);
    assert!(state.notice(Text::HoldPreparing));
    assert!(!state.owns(GestureId(2)));
    assert!(state.owns(GestureId(1)));
    let (action, _) = state.cancel(|| None);
    assert_eq!(action, Cancel::End);
    assert_eq!(state.status, Some(Text::HoldPreparing));
    assert!(state.recovery.is_none());
    state.end(Text::HoldCancelled);
    let (action, _) = state.cancel(|| None);
    assert_eq!(action, Cancel::Refresh);
    assert_eq!(state.status, None);
    let (action, _) = state.cancel(|| None);
    assert_eq!(action, Cancel::Idle);
}

#[test]
fn quitting_hands_back_only_a_waiting_source_and_unavailable_hides_lost_words_notice() {
    let (mut state, _gate) = holding(Source::Waiting);
    state.lost = Some(Text::HoldLost);
    state.notice(Text::HoldPreparing);
    assert_eq!(state.lost, Some(Text::HoldLost));
    state.notice(Text::HoldUnavailable);
    assert_eq!(state.lost, None);
    assert_eq!(
        state.quit(|| {}).map(|hold| hold.token().gesture()),
        Some(GestureId(1))
    );
    assert!(state.status.is_none() && state.recovery.is_none());
    assert!(state.quit(|| {}).is_none());
    let (mut state, _gate) = holding(Source::Recording);
    assert!(state.quit(|| {}).is_none());
    assert!(state.pending.is_none());
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

/// A hold of gesture 1 that showed the screen under `seen`, its source recording.
fn showing(seen: Offer) -> (State, CollectionGate) {
    let gate = CollectionGate::default();
    let token = gate.arm(GestureId(1)).expect("armed");
    let mut state = State::default();
    let session = Session::detached(STAMP.session, Incarnation(1));
    state.begin(token, STAMP, session, true, Some(seen), false);
    state.record();
    (state, gate)
}

fn offer(epoch: u64) -> Offer {
    Offer {
        session: 4,
        choice: 2,
        target: (Some(ThreadId(7)), 3),
        revision: NoticeRevision(NonZeroU64::MIN),
        epoch,
    }
}

#[test]
fn a_status_from_another_connection_or_after_the_end_does_nothing() {
    let (mut state, _gate) = holding(Source::Waiting);
    let speech = || Text::VoiceUnloaded;
    let other = state.status(5, GestureId(1), HoldStatus::Admitted, speech);
    assert_eq!(other, StatusStep::Refused(Refused::StaleStatus));
    let own = state.status(4, GestureId(1), HoldStatus::Admitted, speech);
    assert_eq!(own, StatusStep::Admit);
    let failed = state.status(
        4,
        GestureId(1),
        HoldStatus::Failed(HoldFailure::NotLoaded),
        speech,
    );
    assert_eq!(
        failed,
        StatusStep::Failed(HoldFailure::NotLoaded, Text::VoiceUnloaded)
    );
    state.record();
    let admitted = state.status(4, GestureId(1), HoldStatus::Admitted, speech);
    assert_eq!(
        admitted,
        StatusStep::Ignored,
        "its waiter is installed once"
    );
    state.end(Text::HoldCancelled);
    let late = state.status(4, GestureId(1), HoldStatus::Empty, speech);
    assert_eq!(late, StatusStep::Refused(Refused::TerminalStatus));
}

#[test]
fn admission_refuses_in_order_and_fences_only_a_valid_or_unleased_hold() {
    let idle = State::default();
    let never = || -> bool { panic!("asked after a refusal") };
    let fence = idle.admission(|| true, || false, never, never);
    assert_eq!(
        (fence.refused, fence.fence),
        (Some(Refused::InputFence), false)
    );
    let unleased = idle.admission(|| true, || true, || true, || false);
    assert_eq!(
        (unleased.refused, unleased.fence),
        (Some(Refused::EscapeLease), true)
    );
    let armed = idle.admission(|| true, || true, || false, never);
    assert_eq!(armed.refused, Some(Refused::GenerationNotPreparing));
    let valid = idle.admission(|| true, || true, || true, || true);
    assert_eq!((valid.refused, valid.fence), (None, true));
    let (busy, _gate) = holding(Source::Waiting);
    let pending = busy.admission(never, never, never, never);
    assert_eq!(
        (pending.refused, pending.fence),
        (Some(Refused::PendingHold), false)
    );
}

#[test]
fn collection_retries_busy_and_publishes_listening_only_after_its_own_acceptance() {
    let gesture = GestureId(1);
    let (mut state, _gate) = holding(Source::Waiting);
    let no_accept = || panic!("the source must not accept again or out of order");
    assert_eq!(state.collecting(gesture, no_accept), Err(Denied::Stale));
    state.record();
    assert_eq!(
        state.collecting(GestureId(2), no_accept),
        Err(Denied::Stale)
    );
    let before = state.collecting(gesture, || Err(Denied::Busy));
    assert_eq!(before, Err(Denied::Busy));
    assert_eq!(Collection::after(before.map(|_| ())), Collection::Retry);
    assert_eq!(
        state.collecting(gesture, || Ok(())),
        Ok(Some(Text::HoldListening))
    );
    assert_eq!(Collection::after(Err(Denied::Busy)), Collection::Retry);
    assert_eq!(state.collecting(gesture, no_accept), Ok(None));
    assert!(matches!(
        state.release(gesture, false, 5, STAMP),
        Release::Released { .. }
    ));
    assert_eq!(state.collecting(gesture, no_accept), Err(Denied::Stale));
    assert_eq!(Collection::after(Err(Denied::Stale)), Collection::Cancel);
    assert_eq!(Collection::after(Ok(())), Collection::Complete);
}

#[test]
fn only_an_accepted_collection_releases_and_only_once() {
    let (mut state, _gate) = showing(offer(1));
    assert!(matches!(
        state.release(GestureId(1), false, 5, STAMP),
        Release::Cancel
    ));
    let (mut state, _gate) = showing(offer(1));
    assert_eq!(
        state.collecting(GestureId(1), || Ok(())),
        Ok(Some(Text::HoldListening))
    );
    let no_accept = || panic!("the same source already accepted");
    assert_eq!(
        state.collecting(GestureId(1), no_accept),
        Ok(None),
        "the keys are down"
    );
    let moved = Stamp { choice: 3, ..STAMP };
    let Release::Released { screen } = state.release(GestureId(1), false, 5, moved) else {
        panic!("an accepted collection releases");
    };
    assert_eq!(
        screen.map(|(_, same)| same),
        Some(false),
        "another stamp has no authority"
    );
    assert_eq!(state.pending.as_ref().and_then(Hold::released), Some(5));
    assert_eq!(
        state.collecting(GestureId(1), no_accept),
        Err(Denied::Stale),
        "a release while copies wait wins"
    );
    assert!(
        state.take_screen().is_none(),
        "the screen was asked for once"
    );
    let again = state.release(GestureId(1), false, 6, STAMP);
    assert!(matches!(again, Release::Refused(Refused::NoPendingHold)));
    assert!(matches!(
        state.release(GestureId(1), true, 6, STAMP),
        Release::Cancel
    ));
}

#[test]
fn a_release_is_current_only_under_its_own_target_choice_and_pair() {
    let (state, _gate) = showing(offer(1));
    let open = Session::detached(STAMP.session, Incarnation(1));
    let yes = || true;
    let never = || -> bool { panic!("read after the hold refused") };
    let permits = |gesture, target, choice, session| {
        state.permits(GestureId(gesture), yes, || (target, choice, session), never)
    };
    assert!(state.permits(
        GestureId(1),
        yes,
        || (STAMP.target, STAMP.choice, Some(&open)),
        yes
    ));
    assert!(!permits(2, STAMP.target, STAMP.choice, Some(&open)));
    assert!(!permits(
        1,
        (Some(ThreadId(7)), 4),
        STAMP.choice,
        Some(&open)
    ));
    assert!(!permits(1, STAMP.target, 3, Some(&open)));
    let next = Session::detached(STAMP.session + 1, Incarnation(1));
    assert!(!permits(1, STAMP.target, STAMP.choice, Some(&next)));
    let seen = || (STAMP.target, STAMP.choice, Some(&open));
    assert!(
        !state.permits(
            GestureId(1),
            || false,
            || panic!("read after untrusted input"),
            never
        ),
        "an untrusted release"
    );
    assert!(
        !state.permits(GestureId(1), yes, seen, || false),
        "a revoked lease"
    );
    open.close();
    assert!(!permits(1, STAMP.target, STAMP.choice, Some(&open)));
}

#[test]
fn only_an_unclaimed_or_retired_generation_tells_its_session() {
    let session = Session::detached(STAMP.session, Incarnation(1));
    let gate = CollectionGate::default();
    let token = gate.arm(GestureId(1)).expect("armed");
    assert!(matches!(discard(&token, Some(&session)), Discard::Stop(_)));
    assert!(token.retired(), "discarded unstarted");
    assert!(
        matches!(discard(&token, Some(&session)), Discard::Stop(_)),
        "retired stays told"
    );
    assert!(matches!(discard(&token, None), Discard::Cancel));
    let claimed = gate.arm(GestureId(2)).expect("armed");
    let _reserved = claimed.reserve().expect("reserved");
    assert!(matches!(
        discard(&claimed, Some(&session)),
        Discard::Nothing
    ));
    let quiet = CollectionGate::default().arm(GestureId(3)).expect("armed");
    assert!(matches!(discard(&quiet, None), Discard::Nothing));
}

#[test]
fn a_disabled_or_busy_recognition_is_held_back_with_its_notice() {
    let mut state = State::default();
    let unasked = || -> bool { panic!("didn't need this check") };
    let consent_seen = std::cell::Cell::new(false);
    let notice = State::held_back(
        AudioSession::QueuedRequired,
        || {
            consent_seen.set(true);
            false
        },
        || consent_seen.get().then_some(Text::VoiceNotInstalled),
        unasked,
    )
    .expect("hold-to-talk refusal wins after reading consent");
    assert_eq!(notice, Text::VoiceNotInstalled);
    assert_eq!(state.status, None);
    state.admission_refused(notice);
    assert_eq!(state.status, Some(Text::VoiceNotInstalled));
    assert_eq!(
        State::held_back(AudioSession::QueuedRequired, || false, || None, unasked),
        Some(Text::TurnBusy)
    );
    assert_eq!(
        State::held_back(AudioSession::Legacy, unasked, || None, || true),
        Some(Text::TurnBusy)
    );
    assert_eq!(
        State::held_back(AudioSession::Legacy, unasked, || None, || false),
        None
    );
}

#[test]
fn an_admission_refusal_preserves_an_existing_hold_and_cleans_up_only_its_own_attempt() {
    let (state, _gate) = holding(Source::Recording);
    assert_eq!(
        state.no_inhibit(|| panic!("a pending Hold keeps its notice")),
        None
    );
    assert_eq!(state.replace_hold(), Some(Text::HoldCancelled));
    let state = State::default();
    assert_eq!(state.no_inhibit(|| None), Some(Text::TurnBusy));
    assert_eq!(
        state.no_inhibit(|| Some(Text::VoiceUnloaded)),
        Some(Text::VoiceUnloaded)
    );
    assert_eq!(state.replace_hold(), None);
    let own = Session::detached(STAMP.session, Incarnation(1));
    for (reason, notice) in [
        (RunRefused::Busy, Text::TurnBusy),
        (RunRefused::Offline, Text::HoldFailed),
    ] {
        let refused = Recognition::new(&own, &Err(reason));
        assert_eq!(refused.notice, notice);
        assert!(
            matches!(refused.unsent, Unsent::StopOn(session) if std::ptr::eq(session, std::ptr::from_ref(&own)))
        );
    }
    let prepared = Recognition::new(
        &own,
        &Ok(ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: STAMP.target.0,
            selection: STAMP.target.1,
        }),
    );
    assert_eq!(prepared.notice, Text::HoldFailed);
    assert!(matches!(prepared.unsent, Unsent::DiscardRun));
}

#[test]
fn only_a_listening_hold_before_its_deadline_is_warned() {
    let (mut state, _gate) = holding(Source::Recording);
    let token = state.pending.as_ref().expect("pending").token().clone();
    let alive = AtomicBool::new(true);
    let unasked = || -> bool { panic!("not eligible") };
    state.status = Some(Text::HoldTranscribing);
    assert_eq!(
        state.near_limit(&token, &alive, Instant::now(), unasked),
        None
    );
    state.status = Some(Text::HoldListening);
    let late = token.hold_deadline();
    assert_eq!(state.near_limit(&token, &alive, late, unasked), None);
    let elsewhere = state.near_limit(&token, &alive, Instant::now(), || false);
    assert_eq!(elsewhere, None, "another stamp or input");
    let preparing = state.near_limit(&token, &alive, Instant::now(), || true);
    assert_eq!(preparing, None, "not collecting yet");
}

#[test]
fn a_screen_release_checks_off_then_its_offer_then_the_pointer() {
    let (state, _gate) = showing(offer(1));
    let at = Some(PhysicalPoint([4, 5]));
    let unasked = || -> Option<Offer> { panic!("turned off before publishing") };
    assert_eq!(
        state.screen_release(Some(false), unasked, at),
        Err(VoiceOnly::TurnedOff)
    );
    let republished = state.screen_release(Some(true), || Some(offer(2)), at);
    assert_eq!(republished, Err(VoiceOnly::Refused(Refused::StaleContext)));
    let unread = state.screen_release(Some(true), || Some(offer(1)), None);
    assert_eq!(unread, Err(VoiceOnly::Refused(Refused::PointerUnread)));
    let shown = state.screen_release(None, || Some(offer(1)), at);
    assert_eq!(shown, Ok(PhysicalPoint([4, 5])));
}

#[test]
fn a_receipt_for_another_hold_publishes_nothing() {
    let (mut state, _gate) = holding(Source::Recording);
    state.pending.as_mut().expect("pending").token =
        CollectionGate::default().arm(GestureId(2)).expect("armed");
    let settled = state.receipt(GestureId(1), Ok(Finalization::Released { packets: 0 }), 0);
    assert_eq!(settled, ReceiptStep::Stale);
    assert!(
        state.recording(GestureId(2)),
        "the pending hold keeps recording"
    );
}

#[test]
fn a_cancel_before_the_receipt_keeps_its_notice() {
    let (mut state, _gate) = holding(Source::Recording);
    state.end(Text::HoldCancelled);
    let receipt = state.receipt(GestureId(1), Ok(Finalization::Released { packets: 2 }), 2);
    assert_eq!(receipt, ReceiptStep::End(Text::HoldCancelled));
    let (mut state, _gate) = holding(Source::Recording);
    state.end(Text::HoldUnavailable);
    let lost = state.receipt(
        GestureId(1),
        Ok(Finalization::Discarded(MicError::Format)),
        0,
    );
    assert_eq!(lost, ReceiptStep::End(Text::HoldMicrophoneUnavailable));
}

#[test]
fn only_the_exact_packet_count_joins_and_sends_once_retired() {
    let (mut state, _gate) = holding(Source::Recording);
    let short = state.receipt(GestureId(1), Ok(Finalization::Released { packets: 3 }), 2);
    assert_eq!(short, ReceiptStep::End(Text::HoldFailed));
    let (mut state, _gate) = holding(Source::Recording);
    let lost = state.receipt(GestureId(1), Err(MicError::SessionLost), 0);
    assert_eq!(lost, ReceiptStep::End(Text::HoldUnavailable));
    let gate = CollectionGate::default();
    let token = gate.arm(GestureId(1)).expect("armed");
    let mut state = State::default();
    let session = Session::detached(STAMP.session, Incarnation(1));
    state.begin(token, STAMP, session, false, None, true);
    state.record();
    assert_eq!(
        state.collecting(GestureId(1), || Ok(())),
        Ok(Some(Text::HoldListening))
    );
    assert!(matches!(
        state.release(GestureId(1), false, 9, STAMP),
        Release::Released { .. }
    ));
    let joined = state.receipt(GestureId(1), Ok(Finalization::Released { packets: 3 }), 3);
    assert_eq!(joined, ReceiptStep::Finish);
    let unsent = |_: &Session, _| -> bool { panic!("its inhibit hasn't retired") };
    assert!(state.finish(unsent).is_none());
    assert!(!state.retired(GestureId(2)));
    assert!(state.retired(GestureId(1)));
    let sent = Cell::new(None);
    assert!(
        state
            .finish(|_, message| sent.replace(Some(message)).is_none())
            .is_none()
    );
    let key_up = ControlToBrain::KeyUp {
        gesture: GestureId(1),
        t_qpc: 9,
        chunks: 3,
    };
    assert_eq!(sent.take(), Some(key_up));
    assert!(matches!(
        state.pending.as_ref().map(Hold::source),
        Some(Source::Sent)
    ));
    assert!(state.finish(unsent).is_none(), "sent once");
}
