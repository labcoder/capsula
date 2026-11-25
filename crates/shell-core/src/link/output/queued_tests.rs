use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    AssistantName, DockPosition, GestureId, MaxFrameRate, OnboardingStep, RunId, RunOrigin,
    SceneId, SegmentId, SettingsView, StepRef, SttUnload, ThreadId,
};

use super::super::{Output, Retirement};
use super::{
    Active, Delivery, LOCAL_QUIET, NarratedReservation, OutputOwner, Queued, RECEIPT_QUEUED,
    StopOrigin,
};
use crate::link::{
    Attendance, HANDSHAKE_TIMEOUT, Shared, narrated::Narrated, queued::MetadataFrame,
};
use crate::speaker::conductor::Assignment;

/// A registered, acknowledged Plain pair with Talk-back on and Show Screen off.
fn speaking(deadline: Instant) -> Queued {
    let (ack, _) = mpsc::sync_channel(1);
    let mut owner = Queued::new(
        deadline,
        ack,
        Arc::new(OnceLock::new()),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    owner.ack = None;
    owner.talk_back = Some(true);
    owner.show_screen = Some(false);
    owner
}

fn silent_owner() -> OutputOwner {
    let owner = OutputOwner::default();
    assert!(
        owner
            .begin_queued(
                7,
                Arc::new(|| {}),
                Instant::now() + Duration::from_secs(5),
                || false,
                |deadline| {
                    let mut pair = speaking(deadline);
                    pair.talk_back = Some(false);
                    pair
                },
            )
            .is_some()
    );
    owner
}

fn settings_modes() -> (SettingsView, SettingsView) {
    let off = SettingsView {
        assistant_name: AssistantName::new("Cloud").unwrap(),
        user_name: None,
        onboarded: true,
        main_thread: ThreadId(1),
        max_frame_rate: MaxFrameRate::AUTOMATIC,
        default_model: None,
        dock_position: DockPosition::TopCenter,
        show_in_taskbar: false,
        talk_back: false,
        stt_unload: SttUnload::Kept,
        hold_to_talk: true,
        onboarding_from: OnboardingStep::Welcome,
        screen_holds: true,
        show_screen: false,
    };
    let on = SettingsView {
        talk_back: true,
        ..off.clone()
    };
    (off, on)
}

#[test]
fn the_install_notice_neither_switches_modes_nor_answers_an_explicit_change() {
    let owner = silent_owner();
    let (off, on) = settings_modes();
    let (out, frames) = async_channel::bounded(1);
    assert!(!owner.defer_spoken_replies(6, &on));
    assert!(owner.defer_spoken_replies(7, &on));
    assert!(!owner.defer_spoken_replies(7, &on));
    assert!(!owner.queued_speaking(7));
    let inhibit = owner
        .acquire(GestureId(1))
        .expect("no spoken reply to stop");
    assert!(
        owner.queued_turn(7, Some(1), || true),
        "the registered silent mode is unchanged"
    );
    owner.queued_refused(7, 1);
    assert_eq!(owner.retire(inhibit, Retirement::NoMic), Ok(()));
    assert_eq!(owner.switch_talk_back(7, false, || true), Ok(()));
    assert_eq!(owner.register_queued(7, &on, false, &out), Ok(()));
    assert!(
        !owner.queued_turn(7, Some(2), || true),
        "still waiting for the Off answer"
    );
    assert_eq!(owner.register_queued(7, &off, true, &out), Ok(()));
    assert!(owner.queued_turn(7, Some(2), || true));
    assert!(
        !owner.defer_spoken_replies(7, &on),
        "later Off can't be undone by replay"
    );
    assert_eq!(
        owner.register_queued(7, &on, false, &out),
        Err(Attendance::Lost)
    );
    assert!(frames.is_empty(), "registration was never offered again");
}

#[test]
fn an_explicit_on_racing_the_default_reconnects_only_if_written() {
    for written in [false, true] {
        let owner = silent_owner();
        let (_, on) = settings_modes();
        let (out, frames) = async_channel::bounded(1);
        assert_eq!(owner.switch_talk_back(7, true, || true), Ok(()));
        assert!(owner.defer_spoken_replies(7, &on));
        assert!(!owner.queued_speaking(7));
        if written {
            owner.refuse_talk_back(6);
        } else {
            owner.refuse_talk_back(7);
        }
        assert!(
            !owner.queued_turn(7, Some(1), || true),
            "the setting still owes its own answer"
        );
        if written {
            assert_eq!(
                owner.register_queued(7, &on, true, &out),
                Err(Attendance::TalkBackChanged)
            );
        } else {
            assert_eq!(owner.register_queued(7, &on, true, &out), Ok(()));
            assert!(owner.queued_turn(7, Some(1), || true));
        }
        assert!(frames.is_empty());
    }
}

#[test]
fn a_speaking_hold_needs_the_previous_runs_queued_receipt_not_its_local_quiet() {
    let mut owner = speaking(Instant::now() + Duration::from_secs(5));
    let proof = Arc::new(AtomicU8::new(LOCAL_QUIET));
    owner.active = Some(Active {
        id: RunId(1),
        input: None,
        proof: Arc::clone(&proof),
        delivery: None,
        locally_stopped: false,
        run_end_by: None,
    });
    assert!(!owner.hold_ready(), "local quiet alone admits no hold");
    proof.fetch_or(RECEIPT_QUEUED, Ordering::AcqRel);
    assert!(owner.hold_ready(), "the queued receipt does");
}

#[test]
fn a_typed_turn_cannot_reserve_a_speaking_pair_while_a_gesture_holds_the_inhibit() {
    let owner = OutputOwner::default();
    let opened = owner.begin_queued(
        1,
        Arc::new(|| {}),
        Instant::now() + Duration::from_secs(5),
        || false,
        speaking,
    );
    assert!(opened.is_some());
    let token = owner
        .acquire(GestureId(1))
        .expect("a quiet speaking pair lends the inhibit");
    assert!(
        !owner.queued_turn(1, Some(1), || true),
        "refused under the inhibit"
    );
    assert_eq!(owner.retire(token, Retirement::NoMic), Ok(()));
    assert!(
        owner.queued_turn(1, Some(1), || true),
        "free once it retired"
    );
}

/// An opened narrated speaking pair, session 1.
fn narrated() -> OutputOwner {
    let owner = OutputOwner::default();
    let opened = owner.begin_queued(
        1,
        Arc::new(|| {}),
        Instant::now() + Duration::from_secs(5),
        || false,
        |deadline| {
            let mut owner = speaking(deadline);
            owner.narrated = Some(Arc::new(Narrated::new()));
            owner
        },
    );
    assert!(opened.is_some());
    owner
}

/// Run 1 as control binds it on a narrated pair: no promise yet.
fn bound(proof: u8) -> Active {
    Active {
        id: RunId(1),
        input: None,
        proof: Arc::new(AtomicU8::new(proof)),
        delivery: Some(Delivery::Unpromised),
        locally_stopped: false,
        run_end_by: None,
    }
}

#[test]
fn a_released_holds_early_promise_waits_for_control_to_bind_its_run() {
    let owner = narrated();
    let Output::Queued(output) = owner.lock().output else {
        panic!("a queued pair");
    };
    let assignment = Assignment {
        session: 1,
        output,
        run: RunId(1),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(1),
            index: 0,
        },
    };
    let promise = || owner.narrated_metadata(assignment, MetadataFrame::Commit);
    assert!(promise().is_err(), "nothing names the run");
    owner.hold_released.store(true, Ordering::Release);
    assert!(
        matches!(promise(), Ok(NarratedReservation::Waiting)),
        "a released hold's promise waits unaccepted"
    );
    owner.hold_released.store(false, Ordering::Release);
    owner.lock().queued.as_mut().expect("the pair").active = Some(bound(0));
    assert!(
        matches!(promise(), Ok(NarratedReservation::Reserved)),
        "accepted once control bound the run"
    );
}

#[test]
fn a_hold_after_esc_before_the_first_promise_waits_for_that_runs_run_end() {
    let owner = narrated();
    let mut state = owner.lock();
    let queued = state.queued.as_mut().expect("the pair");
    queued.active = Some(bound(LOCAL_QUIET | RECEIPT_QUEUED));
    assert!(
        queued.stop(RunId(1), StopOrigin::Local),
        "Esc stops the run"
    );
    drop(state);
    let hold = || owner.lock().queued_hold().is_some();
    let late = Instant::now() + HANDSHAKE_TIMEOUT;
    assert!(
        !hold(),
        "the stopped run's quiet receipt alone admits no hold"
    );
    assert!(!owner.run_end_overdue(1, Instant::now()));
    assert!(
        owner.run_end_overdue(1, late),
        "a missing RunEnd ends the pair"
    );
    assert!(owner.end_queued_run(1, RunId(1)));
    assert!(hold(), "the exact RunEnd admits the hold");
    assert!(!owner.run_end_overdue(1, late));
}

/// A narrated speaking pair on a real `Shared`, so bind can start its fake speaker.
fn narrated_shared() -> (
    Arc<Shared>,
    mpsc::Receiver<crate::speaker::queued::FixturePlayback>,
) {
    let (device, opened) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared {
        narrated: true,
        fake_playback: Some(device),
        ..Shared::default()
    });
    let pair = shared.output.begin_queued(
        1,
        Arc::new(|| {}),
        Instant::now() + Duration::from_secs(5),
        || false,
        |deadline| {
            let mut owner = speaking(deadline);
            owner.narrated = Some(Arc::new(Narrated::new()));
            owner
        },
    );
    assert!(pair.is_some());
    (shared, opened)
}

#[test]
fn a_hold_binds_through_brief_bridge_contention() {
    let (shared, _opened) = narrated_shared();
    let bridge = shared
        .output
        .lock()
        .queued
        .as_ref()
        .and_then(|q| q.narrated.clone());
    let bridge = bridge.expect("a narrated pair");
    let (out, _control) = async_channel::bounded(8);
    let (held, holding) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _state = bridge.hold_state();
        held.send(()).expect("the test waits");
        std::thread::sleep(Duration::from_millis(200));
    });
    holding.recv().expect("the bridge is held");
    let origin = RunOrigin::Hold {
        gesture: GestureId(1),
    };
    assert!(
        shared
            .output
            .bind_queued(1, RunId(1), origin, &out, |_| {}, Arc::downgrade(&shared)),
        "a busy bridge delays the bind instead of refusing it"
    );
    holder.join().expect("the holder finished");
}

#[test]
fn a_typed_successor_keeps_the_stopped_runs_run_end_bound() {
    let (shared, _opened) = narrated_shared();
    let owner = &shared.output;
    let (out, _control) = async_channel::bounded(8);
    let mut state = owner.lock();
    let queued = state.queued.as_mut().expect("the pair");
    queued.active = Some(bound(LOCAL_QUIET | RECEIPT_QUEUED));
    assert!(
        queued.stop(RunId(1), StopOrigin::Local),
        "Esc stops the run"
    );
    drop(state);
    let late = Instant::now() + HANDSHAKE_TIMEOUT;
    assert!(
        owner.queued_turn(1, Some(1), || true),
        "quiet admits a typed turn"
    );
    let typed = RunOrigin::Typed { turn: 1 };
    assert!(owner.bind_queued(1, RunId(2), typed, &out, |_| {}, Arc::downgrade(&shared)));
    assert!(!owner.run_end_overdue(1, Instant::now()));
    assert!(
        owner.run_end_overdue(1, late),
        "the stopped run's missing RunEnd still ends the pair"
    );
    assert!(owner.end_queued_run(1, RunId(1)));
    assert!(
        !owner.run_end_overdue(1, late),
        "its exact RunEnd clears the bound"
    );
}
