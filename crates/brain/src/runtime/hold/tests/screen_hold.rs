//! The screen hold: armed at recognition, captured at release, its words waiting for the picture.

use std::any::Any;

use kumochi_protocol::geometry::{MAX_PATH_POINTS, PhysicalPoint, PointerPath, ReleasePointer};
use kumochi_protocol::message::{CaptureFailure, CaptureFor};

use super::*;
use crate::context::{Jpeg, Screen, ScreenClass};
use crate::screen::{Closed, Generation, InkBounds, Refusal, ScreenContext};

type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;

/// A stamp is its own age in milliseconds, and one of 10 s or more is later than now.
const AGE: fn(u64) -> Option<Duration> =
    |t_qpc| (t_qpc < 10_000).then(|| Duration::from_millis(t_qpc));

/// A brain whose default model takes images, with the screen shown and a worker rig, and what
/// must outlive the test.
async fn desk(
    scratch: &Scratch,
) -> (
    Brain,
    crate::supervise::fixture::Rig<crate::screen::Holds>,
    Box<dyn Any>,
) {
    let (mut brain, release, decoded, fetch) =
        crate::runtime::tests::holds::input(scratch, Vec::new(), "not submitted").await;
    let (rig, _sol, _luna) = sees_on_sol(&mut brain);
    brain.holds.window_capture = true;
    brain.holds.recognized_age = AGE;
    set(&mut brain, SettingsOp::SetShowScreen(true));
    (brain, rig, Box::new((release, decoded, fetch)))
}

fn recognized(gesture: GestureId) -> ControlToBrain {
    ControlToBrain::ScreenHoldRecognized {
        gesture,
        thread: None,
        selection: 1,
        t_qpc: 1,
    }
}

fn release(gesture: GestureId, t_qpc: u64, excluded_qpc: u64, points: usize) -> ControlToBrain {
    ControlToBrain::ScreenRelease {
        gesture,
        lease: LEASE,
        t_qpc,
        excluded_qpc,
        pointer: ReleasePointer(PhysicalPoint([5, 5])),
        path: PointerPath(vec![PhysicalPoint([5, 5]); points]),
    }
}

fn failed(gesture: GestureId) -> ControlToShell {
    ControlToShell::CaptureFailed {
        capture_for: CaptureFor::Gesture(gesture),
        reason: CaptureFailure::Unavailable,
    }
}

fn key(gesture: u32) -> HoldKey {
    HoldKey {
        gesture: GestureId(gesture),
        ..KEY
    }
}

fn part(brain: &Brain) -> &ScreenPart {
    &brain.holds.pending.as_ref().expect("a pending hold").screen
}

/// A recorded hold of `kind` under `KEY` with `screen` as its part.
fn held(brain: &mut Brain, kind: Kind, screen: ScreenPart) {
    screen_hold(brain, KEY, screen);
    brain.holds.pending.as_mut().unwrap().kind = kind;
}

fn capturing(generation: Option<Generation>, until: Instant) -> ScreenPart {
    ScreenPart::Capturing {
        generation,
        lease: LEASE,
        stop: Arc::default(),
        until,
    }
}

fn answer(generation: Option<Generation>, outcome: Outcome) -> Event {
    Event::Screen(ScreenEvent {
        key: KEY,
        generation,
        outcome,
    })
}

#[tokio::test]
async fn a_screen_hold_arms_at_recognition_and_captures_the_display_at_release() {
    let scratch = Scratch::new("screen-hold-release");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    assert!(
        brain
            .hold_control(&recognized(KEY.gesture), &mut link)
            .unwrap()
    );
    assert!(matches!(part(&brain), ScreenPart::Armed));
    assert!(!controls(&mut output).contains(&failed(KEY.gesture)));
    let path = vec![PhysicalPoint([5, 5]), PhysicalPoint([40, 5])];
    let mut released = release(KEY.gesture, 100, 200, 0);
    if let ControlToBrain::ScreenRelease { path: sent, .. } = &mut released {
        sent.0.clone_from(&path);
    }
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert!(controls(&mut output).is_empty());
    assert!(matches!(
        part(&brain),
        ScreenPart::Capturing {
            generation: None,
            lease: LEASE,
            ..
        }
    ));
    let (_id, request) = rig.next().await;
    let Request::Display {
        point,
        excluded,
        ink,
        ..
    } = request
    else {
        panic!("not the display: {request:?}");
    };
    assert_eq!((point, excluded, ink.0), (PhysicalPoint([5, 5]), 200, path));
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    brain.close().await;
}

#[tokio::test]
async fn a_release_runs_the_gate_the_stamps_and_the_path_before_any_capture() {
    let scratch = Scratch::new("screen-hold-release-checks");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let luna = brain.config.view().default_model.clone().map(|mut sol| {
        sol.model = kumochi_protocol::message::ModelSlug::new("gpt-6-luna").unwrap();
        sol
    });
    let sol = brain.config.view().default_model.clone().unwrap();
    let not_ready = || ScreenPart::Closed(ScreenClass::NotReady);
    // (gesture, what changes during the hold, release stamp, exclusion stamp, points, part)
    let cases: [(u32, Option<SettingsOp>, u64, u64, usize, ScreenPart); 7] = [
        (
            10,
            Some(SettingsOp::SetShowScreen(false)),
            100,
            200,
            2,
            ScreenPart::Discarded,
        ),
        (
            11,
            luna.map(SettingsOp::SetDefaultModel),
            100,
            200,
            2,
            ScreenPart::Closed(ScreenClass::ModelCantSee),
        ),
        (12, None, 1_501, 1_600, 2, not_ready()),
        (13, None, 10_000, 10_000, 2, not_ready()),
        (14, None, 200, 199, 2, not_ready()),
        (15, None, 200, 10_000, 2, not_ready()),
        (16, None, 100, 200, MAX_PATH_POINTS + 1, not_ready()),
    ];
    for (gesture, change, t_qpc, excluded, points, closed) in cases {
        let g = GestureId(gesture);
        assert!(brain.hold_control(&recognized(g), &mut link).unwrap());
        assert!(matches!(part(&brain), ScreenPart::Armed), "{gesture}");
        controls(&mut output);
        if let Some(change) = change {
            set(&mut brain, change);
        }
        let released = release(g, t_qpc, excluded, points);
        assert!(brain.hold_control(&released, &mut link).unwrap());
        assert_eq!(controls(&mut output), [failed(g)], "{gesture}");
        assert_eq!(
            format!("{:?}", part(&brain)),
            format!("{closed:?}"),
            "{gesture}"
        );
        // Valid input afterwards doesn't revive the part, and its lease is still answered.
        set(&mut brain, SettingsOp::SetShowScreen(true));
        set(&mut brain, SettingsOp::SetDefaultModel(sol.clone()));
        assert!(
            brain
                .hold_control(&release(g, 100, 200, 2), &mut link)
                .unwrap()
        );
        assert_eq!(controls(&mut output), [failed(g)], "{gesture}");
        assert_eq!(
            format!("{:?}", part(&brain)),
            format!("{closed:?}"),
            "{gesture}"
        );
        brain.holds.lost();
    }
    // Exactly 1.5 s old, excluded at the release itself, with a full path: it starts, and its
    // capture is the worker's first request, so no refused release asked for one.
    let g = GestureId(17);
    assert!(brain.hold_control(&recognized(g), &mut link).unwrap());
    let released = release(g, 1_500, 1_500, MAX_PATH_POINTS);
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert!(!controls(&mut output).contains(&failed(g)));
    assert!(matches!(part(&brain), ScreenPart::Capturing { .. }));
    let (_id, request) = rig.next().await;
    assert!(
        matches!(
            request,
            Request::Display {
                excluded: 1_500,
                ..
            }
        ),
        "{request:?}"
    );
    brain.holds.lost();
    crate::screen::release_next(&mut rig, key(17)).await;
    brain.close().await;
}

#[tokio::test]
async fn a_release_that_isnt_an_armed_holds_is_answered_unless_a_capture_may_still_run() {
    let scratch = Scratch::new("screen-hold-release-routing");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let ours = release(KEY.gesture, 100, 200, 2);
    // No hold at all: the release's own lease is answered.
    assert!(brain.hold_control(&ours, &mut link).unwrap());
    assert_eq!(controls(&mut output), [failed(KEY.gesture)]);
    let far = Instant::now() + Duration::from_secs(60);
    let window = || waiting();
    for (kind, screen, answered) in [
        (Kind::Voice, ScreenPart::None, true),
        (
            Kind::Screen,
            ScreenPart::Closed(ScreenClass::NotReady),
            true,
        ),
        (Kind::Window, window(), false),
        (Kind::Screen, capturing(Some(Generation(0)), far), false),
    ] {
        held(&mut brain, kind, screen);
        let before = format!("{:?}", part(&brain));
        assert!(brain.hold_control(&ours, &mut link).unwrap());
        let expected = if answered {
            vec![failed(KEY.gesture)]
        } else {
            Vec::new()
        };
        assert_eq!(controls(&mut output), expected, "{kind:?}");
        // Another gesture's recognition doesn't disturb the pending hold either.
        assert!(
            brain
                .hold_control(&recognized(GestureId(30)), &mut link)
                .unwrap()
        );
        assert!(controls(&mut output).iter().all(|message| matches!(
            message,
            ControlToShell::VoiceHoldStatus {
                gesture: GestureId(30),
                status: HoldStatus::Failed(_),
            }
        )));
        assert_eq!(brain.holds.pending.as_ref().unwrap().key, KEY);
        assert_eq!(format!("{:?}", part(&brain)), before, "{kind:?}");
        brain.holds.lost();
        if !answered {
            crate::screen::release_next(&mut rig, KEY).await;
        }
        brain.holds.high = 0;
    }
    brain.close().await;
}

#[tokio::test]
async fn words_go_on_at_once_with_an_armed_part_and_wait_for_a_capturing_one() {
    let scratch = Scratch::new("screen-hold-words");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let (out, _output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    let words = || Event::Decoded(KEY, SttResult::Text("words".into()));
    let far = Instant::now() + Duration::from_secs(60);
    let jpeg = Jpeg::new(JPEG.to_vec()).unwrap();

    // A release that never came: the words don't wait, and the part is not ready.
    held(&mut brain, Kind::Screen, ScreenPart::Armed);
    brain.hold_event(words(), &mut link).unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::PictureNotIncluded(ScreenClass::NotReady))
    ));

    // `Held` alone doesn't end the wait; the sealed picture goes with its painted rectangle.
    held(&mut brain, Kind::Screen, capturing(None, far));
    brain.hold_event(words(), &mut link).unwrap();
    assert!(brain.holds.pending.as_ref().unwrap().words.is_some());
    let generation = Some(Generation(0));
    brain
        .hold_event(answer(generation, Outcome::Held), &mut link)
        .unwrap();
    assert!(brain.holds.pending.is_some());
    let bounds = InkBounds {
        left: 1,
        top: 0,
        right: 3,
        bottom: 2,
    };
    let sealed = ScreenContext::fixture(JPEG).with_ink(Some(bounds));
    brain
        .hold_event(answer(generation, Outcome::Sealed(sealed)), &mut link)
        .unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::Picture(image, Some(painted))) if *image == jpeg && *painted == bounds
    ));

    // A closing answer ends the wait with its class.
    held(&mut brain, Kind::Screen, capturing(None, far));
    brain.hold_event(words(), &mut link).unwrap();
    let changed = Closed::Refused(Refusal::DisplayChanged);
    brain
        .hold_event(answer(None, Outcome::Closed(changed)), &mut link)
        .unwrap();
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::PictureNotIncluded(ScreenClass::Changed))
    ));

    // The ticket's deadline ends it not ready, and the frame is retired.
    held(
        &mut brain,
        Kind::Screen,
        capturing(generation, Instant::now()),
    );
    brain.hold_event(words(), &mut link).unwrap();
    brain.hold_event(Event::Deadline, &mut link).unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::PictureNotIncluded(ScreenClass::NotReady))
    ));
    crate::screen::release_next(&mut rig, KEY).await;
    brain.close().await;
}

#[tokio::test]
async fn a_cancel_or_a_lost_shell_during_the_wait_ends_the_hold_and_retires_the_frame() {
    let scratch = Scratch::new("screen-hold-words-cancel");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let far = Instant::now() + Duration::from_secs(60);
    let words = || Event::Decoded(KEY, SttResult::Text("words".into()));
    let generation = Some(Generation(0));

    held(&mut brain, Kind::Screen, capturing(generation, far));
    brain.hold_event(words(), &mut link).unwrap();
    let cancel = ControlToBrain::GestureCancel {
        gesture: KEY.gesture,
    };
    assert!(brain.hold_control(&cancel, &mut link).unwrap());
    assert!(brain.holds.pending.is_none());
    assert!(
        controls(&mut output).contains(&ControlToShell::VoiceHoldStatus {
            gesture: KEY.gesture,
            status: HoldStatus::Cancelled,
        })
    );
    crate::screen::release_next(&mut rig, KEY).await;
    assert!(brain.coordinator.current_screen().is_none());

    held(&mut brain, Kind::Screen, capturing(generation, far));
    brain.hold_event(words(), &mut link).unwrap();
    brain.holds.lost();
    assert!(brain.holds.pending.is_none());
    crate::screen::release_next(&mut rig, KEY).await;
    assert!(brain.coordinator.current_screen().is_none());
    brain.close().await;
}

#[tokio::test]
async fn a_lapse_before_the_frame_closes_a_capturing_part_and_one_after_is_ignored() {
    let scratch = Scratch::new("screen-hold-lapse");
    let mut brain = test_support::brain(&scratch, "");
    let mut rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.holds.screen = Some(Arc::new(HoldClient::new(rig.handle.clone())));
    let (out, _output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let far = Instant::now() + Duration::from_secs(60);
    let lapsed = ControlToBrain::ExclusionLapsed { lease: LEASE };

    held(&mut brain, Kind::Screen, capturing(None, far));
    assert!(brain.hold_control(&lapsed, &mut link).unwrap());
    assert!(matches!(
        part(&brain),
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    crate::screen::release_next(&mut rig, KEY).await;
    brain.holds.lost();

    held(
        &mut brain,
        Kind::Screen,
        capturing(Some(Generation(0)), far),
    );
    assert!(!brain.hold_control(&lapsed, &mut link).unwrap());
    assert!(matches!(part(&brain), ScreenPart::Capturing { .. }));
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    brain.close().await;
}

#[tokio::test]
async fn each_setting_closes_only_its_own_holds_pending_part() {
    let scratch = Scratch::new("screen-hold-own-setting");
    let (mut brain, _release, _decoded, _fetch) =
        crate::runtime::tests::holds::input(&scratch, Vec::new(), "not submitted").await;
    let switched = |brain: &mut Brain, op: SettingsOp| {
        let pending = brain.config.stage(op).unwrap();
        pending.write().unwrap();
        brain
            .settings_done((0, None, None, Ok((pending, Ok(())))), None)
            .unwrap();
    };
    for (kind, own, other) in [
        (
            Kind::Screen,
            SettingsOp::SetShowScreen(false),
            SettingsOp::SetScreenHolds(false),
        ),
        (
            Kind::Window,
            SettingsOp::SetScreenHolds(false),
            SettingsOp::SetShowScreen(false),
        ),
    ] {
        switched(&mut brain, SettingsOp::SetShowScreen(true));
        switched(&mut brain, SettingsOp::SetScreenHolds(true));
        held(&mut brain, kind, ready());
        switched(&mut brain, other);
        assert!(matches!(part(&brain), ScreenPart::Ready(_)), "{kind:?}");
        switched(&mut brain, own);
        assert!(matches!(part(&brain), ScreenPart::Discarded), "{kind:?}");
        brain.holds.lost();
    }
    brain.close().await;
}
