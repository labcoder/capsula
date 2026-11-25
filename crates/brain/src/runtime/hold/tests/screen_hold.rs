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
    let rig = seeing(&mut brain);
    (brain, rig, Box::new((release, decoded, fetch)))
}

/// Lets `brain`'s default model take images, with the screen shown, and gives it a worker rig.
pub(in crate::runtime) fn seeing(
    brain: &mut Brain,
) -> crate::supervise::fixture::Rig<crate::screen::Holds> {
    let (rig, _sol, _luna) = sees_on_sol(brain);
    brain.holds.window_capture = true;
    brain.holds.display_capture = true;
    brain.holds.stamp_age = AGE;
    set(brain, SettingsOp::SetShowScreen(true));
    let (out, _output) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.screen_holds_changed(&mut link).unwrap();
    rig
}

/// The pending hold's recipient, read as the store reads it.
fn prepared(brain: &mut Brain) {
    let hold = brain.holds.pending.as_ref().unwrap();
    let (key, thread, cancel) = (hold.key, hold.thread, Arc::clone(&hold.cancel));
    let Ok(StoreOp::ReadHold(read)) =
        brain
            .coordinator
            .prepare_hold(key, thread, 1, brain.snapshot(Origin::Kumo), cancel)
    else {
        panic!("no hold read");
    };
    brain.holds.pending.as_mut().unwrap().snapshot = Some(read.snapshot);
}

pub(in crate::runtime) fn image_recipients(brain: &mut Brain) -> (ModelChoice, ModelChoice) {
    use kumochi_protocol::message::{
        ConnectionKind, Effort, EffortSet, ModelInfo, ModelName, ModelSlug,
    };

    let choice = |model, effort| ModelChoice {
        model: ModelSlug::new(model).unwrap(),
        effort,
        connection: ConnectionKind::ChatGpt,
    };
    let a = choice("gpt-6-sol", Effort::Low);
    let b = choice("gpt-6.1-sol", Effort::High);
    brain.account.read_catalog(
        [&a, &b]
            .into_iter()
            .map(|choice| ModelInfo {
                slug: choice.model.clone(),
                display_name: ModelName::new("Fixture model").unwrap(),
                supported_efforts: EffortSet::new([Effort::Low, Effort::High]),
            })
            .collect(),
        vec![a.model.clone(), b.model.clone()],
    );
    set(brain, SettingsOp::SetDefaultModel(a.clone()));
    (a, b)
}

pub(in crate::runtime) fn recognized(gesture: GestureId) -> ControlToBrain {
    ControlToBrain::ScreenHoldRecognized {
        gesture,
        thread: None,
        selection: 1,
        t_qpc: 1,
        notice: Some(kumochi_protocol::message::NoticeRevision::FIRST),
    }
}

pub(in crate::runtime) fn release(
    gesture: GestureId,
    t_qpc: u64,
    excluded_qpc: u64,
    points: usize,
) -> ControlToBrain {
    ControlToBrain::ScreenRelease {
        gesture,
        lease: LEASE,
        t_qpc,
        excluded_qpc,
        pointer: ReleasePointer(PhysicalPoint([5, 5])),
        path: PointerPath(vec![PhysicalPoint([5, 5]); points]),
        notice: Some(kumochi_protocol::message::NoticeRevision::FIRST),
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
        deadline: screen_deadline(),
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
    prepared(&mut brain);
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
async fn display_capture_alone_captures_the_display_under_its_checks_and_never_a_window() {
    use kumochi_protocol::message::{ModelSlug, ScreenHolds, ScreenOff};
    let scratch = Scratch::new("screen-hold-display-only");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    brain.holds.window_capture = false;
    brain.holds.display_capture = true;
    let sol = brain.config.view().default_model.clone().unwrap();
    let luna = ModelChoice {
        model: ModelSlug::new("gpt-6-luna").unwrap(),
        ..sol.clone()
    };
    let off = ScreenHolds::Off;
    assert_eq!(brain.screen_gate(Some(&sol)), off(ScreenOff::Unsupported));
    assert_eq!(
        brain.display_gate(Some(&luna)),
        off(ScreenOff::ModelCantSee)
    );
    set(&mut brain, SettingsOp::SetShowScreen(false));
    assert_eq!(brain.display_gate(Some(&sol)), off(ScreenOff::TurnedOff));
    set(&mut brain, SettingsOp::SetShowScreen(true));
    let client = brain.holds.screen.take();
    assert_eq!(brain.display_gate(Some(&sol)), off(ScreenOff::Unsupported));
    brain.holds.screen = client;
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
    prepared(&mut brain);
    assert!(
        brain
            .hold_control(&release(KEY.gesture, 100, 200, 2), &mut link)
            .unwrap()
    );
    assert!(controls(&mut output).is_empty());
    let (_id, request) = rig.next().await;
    assert!(
        matches!(request, Request::Display { .. }),
        "not the display: {request:?}"
    );
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    held(&mut brain, Kind::Window, ScreenPart::None);
    assert!(!brain.holds.start_screen(
        KEY,
        PhysicalPoint([5, 5]),
        LEASE,
        CaptureDeadline::after(MAX_TTL),
        (sol.connection, sol.model)
    ));
    assert!(matches!(part(&brain), ScreenPart::None));
    brain.holds.lost();
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
        prepared(&mut brain);
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
    prepared(&mut brain);
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
    let mut rig = seeing(&mut brain);
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
    let sealed_geometry = sealed.geometry();
    assert_eq!(
        (
            sealed_geometry.display.origin,
            sealed_geometry.display.size,
            sealed_geometry.display.dpi,
            sealed_geometry.picture
        ),
        ([0, 0], [4, 3], 96, [4, 3])
    );
    brain
        .hold_event(answer(generation, Outcome::Sealed(sealed)), &mut link)
        .unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::Picture(image, Some(painted), geometry))
            if *image == jpeg && *painted == bounds && *geometry == sealed_geometry
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
async fn a_hung_display_capture_lets_the_words_go_at_the_release_budget() {
    use crate::store::Store;
    use kumochi_protocol::message::RunId;

    let scratch = Scratch::new("screen-hold-hung-display");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (out, _output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    assert!(
        brain
            .hold_control(&recognized(KEY.gesture), &mut link)
            .unwrap()
    );
    prepared(&mut brain);

    tokio::time::pause();
    let at = Instant::now();
    assert!(
        brain
            .hold_control(&release(KEY.gesture, 100, 200, 2), &mut link)
            .unwrap()
    );
    let (_, display) = rig.next().await;
    assert!(matches!(display, Request::Display { .. }));
    assert_eq!(
        Instant::now(),
        at,
        "the worker took the request before time advanced"
    );
    let words = "the words without the picture";
    brain
        .hold_event(
            Event::Decoded(KEY, SttResult::Text(words.into())),
            &mut link,
        )
        .unwrap();
    assert!(
        tokio::time::timeout_at(at + Duration::from_millis(1499), next(&mut brain.holds))
            .await
            .is_err(),
        "the unanswered capture does not end the wait early"
    );
    assert_eq!(brain.coordinator.current_run(), None);

    let event = tokio::time::timeout_at(at + Duration::from_millis(1501), next(&mut brain.holds))
        .await
        .expect("the hold timer fires within one clock tick of the screen deadline");
    assert!(matches!(event, Event::Deadline));
    brain.hold_event(event, &mut link).unwrap();
    assert_eq!(brain.coordinator.current_run(), Some(RunId(1)));
    assert!(matches!(
        brain.coordinator.current_screen(),
        Some(Screen::PictureNotIncluded(ScreenClass::NotReady))
    ));

    tokio::time::resume();
    rig.held().await;
    rig.confirm.send(true).unwrap();
    brain.close().await;
    let store = Store::open(&scratch.db()).unwrap();
    let submitted: String = store
        .conn_for_tests()
        .query_row(
            "SELECT text FROM events WHERE run = 1 AND kind = 'user'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(submitted, words);
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

    // A changed choice found at the words' deadline is reported as the change, not a timeout.
    held(
        &mut brain,
        Kind::Screen,
        capturing(generation, Instant::now()),
    );
    brain.hold_event(words(), &mut link).unwrap();
    controls(&mut output);
    let hold = brain.holds.pending.as_ref().unwrap();
    hold.cancel.store(true, Ordering::SeqCst);
    brain.hold_event(Event::Deadline, &mut link).unwrap();
    assert!(brain.holds.pending.is_none());
    assert!(
        controls(&mut output).contains(&ControlToShell::VoiceHoldStatus {
            gesture: KEY.gesture,
            status: HoldStatus::Failed(HoldFailure::ChoiceChanged),
        })
    );
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

#[tokio::test]
async fn a_release_for_a_hold_whose_choice_changed_captures_nothing() {
    let scratch = Scratch::new("screen-hold-release-cancelled");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let g = KEY.gesture;
    assert!(brain.hold_control(&recognized(g), &mut link).unwrap());
    assert!(matches!(part(&brain), ScreenPart::Armed));
    controls(&mut output);
    let hold = brain.holds.pending.as_ref().unwrap();
    hold.cancel.store(true, Ordering::SeqCst);
    assert!(
        brain
            .hold_control(&release(g, 100, 200, 2), &mut link)
            .unwrap()
    );
    assert_eq!(controls(&mut output), [failed(g)]);
    assert!(matches!(
        part(&brain),
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn a_display_release_requires_the_armed_recipient_once_prepared() {
    use kumochi_protocol::message::Effort;

    let scratch = Scratch::new("screen-hold-prepared-recipient");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (sol, other) = image_recipients(&mut brain);
    assert!(matches!(
        brain.display_gate(Some(&other)),
        ScreenHolds::On { .. }
    ));
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let refused_release = eligible(release(KEY.gesture, 100, 200, 2), brain.holds.notice);

    for prepared in [None, Some(other)] {
        held(&mut brain, Kind::Screen, ScreenPart::Armed);
        let notice = brain.holds.notice;
        let hold = brain.holds.pending.as_mut().unwrap();
        hold.armed_notice = notice;
        hold.snapshot.as_mut().unwrap().defaults.choice = prepared;
        assert!(brain.hold_control(&refused_release, &mut link).unwrap());
        assert_eq!(controls(&mut output), [failed(KEY.gesture)]);
        assert!(matches!(
            part(&brain),
            ScreenPart::Closed(ScreenClass::NotReady)
        ));
        assert!(brain.hold_control(&refused_release, &mut link).unwrap());
        assert_eq!(controls(&mut output), [failed(KEY.gesture)]);
        brain.holds.lost();
    }

    held(&mut brain, Kind::Screen, ScreenPart::Armed);
    let notice = brain.holds.notice;
    let hold = brain.holds.pending.as_mut().unwrap();
    hold.armed_notice = notice;
    hold.snapshot.as_mut().unwrap().defaults.choice = Some(ModelChoice {
        effort: Effort::High,
        ..sol
    });
    let allowed_release = eligible(release(KEY.gesture, 300, 400, 1), brain.holds.notice);
    assert!(brain.hold_control(&allowed_release, &mut link).unwrap());
    assert!(controls(&mut output).is_empty());
    let (_, request) = rig.next().await;
    assert!(matches!(request, Request::Display { excluded: 400, ink, .. } if ink.0.len() == 1));
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    brain.close().await;
}

#[tokio::test]
async fn a_picture_sealed_before_preparation_is_not_paired_to_a_different_recipient() {
    let scratch = Scratch::new("screen-hold-recipient-sealed");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (_, other) = image_recipients(&mut brain);
    let (out, mut output) = mpsc::channel(16);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    let generation = Some(Generation(0));
    held(
        &mut brain,
        Kind::Screen,
        capturing(generation, Instant::now() + MAX_TTL),
    );
    brain.holds.debt = None;
    let hold = brain.holds.pending.as_mut().unwrap();
    let mut snapshot = hold.snapshot.take().unwrap();
    snapshot.defaults.choice = Some(other);
    hold.audio = None;
    hold.ask = None;
    brain
        .hold_event(
            answer(generation, Outcome::Sealed(ScreenContext::fixture(JPEG))),
            &mut link,
        )
        .unwrap();
    assert!(matches!(part(&brain), ScreenPart::Ready(_)));
    brain.hold_prepared(KEY, Ok(snapshot), &mut link).unwrap();
    assert_eq!(
        controls(&mut output),
        [ControlToShell::VoiceHoldStatus {
            gesture: KEY.gesture,
            status: HoldStatus::Admitted,
        }]
    );
    assert!(matches!(part(&brain), ScreenPart::Ready(_)));
    assert!(matches!(
        brain.holds.submit_screen(),
        Some(Screen::PictureNotIncluded(ScreenClass::NotReady))
    ));
    assert!(brain.holds.submit_screen().is_none());
    brain.holds.lost();
    brain.close().await;
}

#[tokio::test]
async fn a_screen_hold_reads_only_show_screen_at_recognition_release_and_the_run() {
    let scratch = Scratch::new("screen-hold-key-rule");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let sol = brain.config.view().default_model.clone().unwrap();
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    link.recovery = Some(crate::runtime::Recovery::new(&[], 1024));
    // (show_screen, screen_holds, gesture): only the first decides each step.
    for (show, window, gesture) in [(false, true, 40), (true, false, 41)] {
        brain.holds.debt = None;
        brain.holds.high = 0;
        set(&mut brain, SettingsOp::SetShowScreen(show));
        set(&mut brain, SettingsOp::SetScreenHolds(window));
        brain.screen_holds_changed(&mut link).unwrap();
        let offer = brain.holds.notice;
        let g = GestureId(gesture);
        assert!(
            brain
                .hold_control(&eligible(recognized(g), offer), &mut link)
                .unwrap()
        );
        assert_eq!(
            matches!(part(&brain), ScreenPart::Armed),
            show,
            "{show}: {:?}",
            part(&brain)
        );
        if show {
            prepared(&mut brain);
        }
        controls(&mut output);
        assert!(
            brain
                .hold_control(&eligible(release(g, 100, 200, 2), offer), &mut link)
                .unwrap()
        );
        if show {
            assert!(controls(&mut output).is_empty());
            assert!(matches!(part(&brain), ScreenPart::Capturing { .. }));
            let (_id, request) = rig.next().await;
            assert!(matches!(request, Request::Display { .. }), "{request:?}");
            brain.holds.lost();
            crate::screen::release_next(&mut rig, key(gesture)).await;
        } else {
            assert_eq!(controls(&mut output), [failed(g)]);
            assert!(matches!(part(&brain), ScreenPart::None));
            brain.holds.lost();
        }
        // The run's own gate, on a thread pinned to a model that can see.
        held(&mut brain, Kind::Screen, ready());
        brain.holds.debt = None;
        let hold = brain.holds.pending.as_mut().unwrap();
        let mut snapshot = hold.snapshot.take().unwrap();
        snapshot.defaults.choice = Some(sol.clone());
        hold.audio = None;
        hold.ask = None;
        brain.hold_prepared(KEY, Ok(snapshot), &mut link).unwrap();
        let screen = &brain.holds.pending.as_ref().unwrap().screen;
        if show {
            assert!(matches!(screen, ScreenPart::Ready(_)), "{screen:?}");
        } else {
            assert!(matches!(screen, ScreenPart::Discarded), "{screen:?}");
        }
        brain.holds.lost();
    }
    brain.close().await;
}

/// `message` bringing `revision` as its first input's eligibility.
fn eligible(
    mut message: ControlToBrain,
    revision: Option<kumochi_protocol::message::NoticeRevision>,
) -> ControlToBrain {
    if let ControlToBrain::HoldRecognized { notice, .. }
    | ControlToBrain::ScreenHoldRecognized { notice, .. }
    | ControlToBrain::ScreenRelease { notice, .. } = &mut message
    {
        *notice = revision;
    }
    message
}

/// The `ChatGPT` sign-in `tag`, as the coordinator hears it.
fn signed_in(tag: u64) -> crate::coordinator::Input {
    crate::coordinator::Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Ready(crate::coordinator::Ready {
            connection: crate::context::ConnectionTag(tag),
            models: Vec::new(),
            images: vec![kumochi_protocol::message::ModelSlug::new("gpt-6-sol").unwrap()],
            trusted_draw: false,
        }),
    )
}

/// A screen hold that brings no eligibility from its first input, or one the brain has since
/// revised, arms nothing, and its release captures nothing.
#[tokio::test]
async fn a_screen_hold_without_its_current_eligibility_captures_nothing() {
    use kumochi_protocol::message::NoticeRevision;
    for (n, revision) in [None, Some(NoticeRevision::FIRST.next())]
        .into_iter()
        .enumerate()
    {
        let scratch = Scratch::new(&format!("screen-hold-ineligible-{n}"));
        let (mut brain, _rig, _keep) = desk(&scratch).await;
        let (out, mut output) = mpsc::channel(64);
        let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
        let ready_notify = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
        brain.screen_holds_changed(&mut link).unwrap();
        controls(&mut output);
        let g = KEY.gesture;
        let recognition = eligible(recognized(g), revision);
        assert!(brain.hold_control(&recognition, &mut link).unwrap());
        assert!(!matches!(part(&brain), ScreenPart::Armed), "{revision:?}");
        controls(&mut output);
        let released = eligible(release(g, 100, 200, 0), revision);
        assert!(brain.hold_control(&released, &mut link).unwrap());
        assert_eq!(controls(&mut output), [failed(g)], "{revision:?}");
        assert!(!matches!(part(&brain), ScreenPart::Capturing { .. }));
        brain.holds.lost();
        brain.close().await;
    }
}

/// A new sign-in between a screen hold's recognition and its release, for the same model,
/// captures nothing.
#[tokio::test]
async fn a_new_sign_in_between_recognition_and_release_captures_nothing() {
    let scratch = Scratch::new("screen-hold-new-sign-in");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.coordinator.handle(signed_in(7)).unwrap();
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let g = KEY.gesture;
    let first = brain.holds.notice;
    assert!(
        brain
            .hold_control(&eligible(recognized(g), first), &mut link)
            .unwrap()
    );
    assert!(matches!(part(&brain), ScreenPart::Armed));
    prepared(&mut brain);
    brain.coordinator.handle(signed_in(8)).unwrap();
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let released = eligible(release(g, 100, 200, 0), first);
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert_eq!(controls(&mut output), [failed(g)]);
    assert!(!matches!(part(&brain), ScreenPart::Capturing { .. }));
    brain.holds.lost();
    brain.close().await;
}

/// A screen hold on the main thread under the current offer, its recipient read as the offered
/// model: one capture, and a repeated release starts no second.
#[tokio::test]
async fn a_screen_hold_under_its_current_eligibility_captures() {
    use kumochi_protocol::message::NoticeRevision;
    let scratch = Scratch::new("screen-hold-eligible");
    let (mut brain, mut rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let g = KEY.gesture;
    let first = Some(NoticeRevision::FIRST);
    let mut recognition = eligible(recognized(g), first);
    if let ControlToBrain::ScreenHoldRecognized { thread, .. } = &mut recognition {
        *thread = Some(brain.config.view().main_thread);
    }
    assert!(brain.hold_control(&recognition, &mut link).unwrap());
    assert!(matches!(part(&brain), ScreenPart::Armed));
    prepared(&mut brain);
    controls(&mut output);
    let released = eligible(release(g, 100, 200, 0), first);
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert!(controls(&mut output).is_empty());
    assert!(matches!(part(&brain), ScreenPart::Capturing { .. }));
    let (_id, request) = rig.next().await;
    assert!(matches!(request, Request::Display { .. }), "{request:?}");
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert!(controls(&mut output).is_empty());
    assert!(matches!(
        part(&brain),
        ScreenPart::Capturing { lease: LEASE, .. }
    ));
    brain.holds.lost();
    crate::screen::release_next(&mut rig, KEY).await;
    brain.close().await;
}

/// A window hold whose recognition brings no current eligibility captures nothing.
#[tokio::test]
async fn a_window_hold_without_its_current_eligibility_captures_nothing() {
    use kumochi_protocol::message::NoticeRevision;
    for (n, revision) in [None, Some(NoticeRevision::FIRST.next())]
        .into_iter()
        .enumerate()
    {
        let scratch = Scratch::new(&format!("window-hold-ineligible-{n}"));
        let (mut brain, _rig, _keep) = desk(&scratch).await;
        let (out, mut output) = mpsc::channel(64);
        let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
        let ready_notify = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
        brain.screen_holds_changed(&mut link).unwrap();
        controls(&mut output);
        let g = KEY.gesture;
        let window = ControlToBrain::HoldRecognized {
            gesture: g,
            thread: None,
            selection: 1,
            pointer: PhysicalPoint([5, 5]),
            lease: LEASE,
            t_qpc: 1,
            notice: revision,
        };
        assert!(brain.hold_control(&window, &mut link).unwrap());
        assert_eq!(controls(&mut output), [failed(g)], "{revision:?}");
        assert!(matches!(part(&brain), ScreenPart::None), "{revision:?}");
        brain.holds.lost();
        brain.close().await;
    }
}

/// A release that comes before the hold's recipient is read starts no capture, though the hold
/// is under its current offer: the recipient may be another model's.
#[tokio::test]
async fn a_release_before_its_recipient_is_read_captures_nothing() {
    let scratch = Scratch::new("screen-hold-unprepared");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    let g = KEY.gesture;
    assert!(brain.hold_control(&recognized(g), &mut link).unwrap());
    assert!(matches!(part(&brain), ScreenPart::Armed));
    controls(&mut output);
    assert!(
        brain
            .hold_control(&release(g, 100, 200, 0), &mut link)
            .unwrap()
    );
    assert_eq!(controls(&mut output), [failed(g)]);
    assert!(matches!(
        part(&brain),
        ScreenPart::Closed(ScreenClass::NotReady)
    ));
    brain.holds.lost();
    brain.close().await;
}

/// A pin set on the hold's thread while its recipient is read, even one set back, ends the whole
/// hold: the read in flight never lets a capture start.
#[tokio::test]
async fn a_pin_moved_while_the_recipient_is_read_ends_the_hold_without_a_capture() {
    let scratch = Scratch::new("screen-hold-pin-moved");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (sol, other) = image_recipients(&mut brain);
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let (g, main) = (KEY.gesture, brain.config.view().main_thread);
    let mut recognition = eligible(recognized(g), brain.holds.notice);
    if let ControlToBrain::ScreenHoldRecognized { thread, .. } = &mut recognition {
        *thread = Some(main);
    }
    assert!(brain.hold_control(&recognition, &mut link).unwrap());
    assert!(matches!(part(&brain), ScreenPart::Armed));
    let hold = brain.holds.pending.as_ref().unwrap();
    let key = hold.key;
    let Ok(StoreOp::ReadHold(read)) = brain.coordinator.prepare_hold(
        key,
        Some(main),
        1,
        brain.snapshot(Origin::Kumo),
        Arc::clone(&hold.cancel),
    ) else {
        panic!("no hold read");
    };
    for choice in [other, sol] {
        brain.hold_fence(&Input::ThreadOp(ThreadOp::SetModel {
            thread: main,
            choice,
        }));
    }
    controls(&mut output);
    brain
        .hold_prepared(key, Ok(read.snapshot), &mut link)
        .unwrap();
    assert_eq!(
        controls(&mut output),
        [ControlToShell::VoiceHoldStatus {
            gesture: g,
            status: HoldStatus::Failed(HoldFailure::ChoiceChanged),
        }]
    );
    assert!(brain.holds.pending.is_none());
    let released = eligible(release(g, 100, 200, 0), brain.holds.notice);
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert_eq!(controls(&mut output), [failed(g)]);
    brain.close().await;
}

/// A hold armed under one offer and released with the brain's newer revision for the same model,
/// as a restamped release would carry, captures nothing: only the armed revision releases.
#[tokio::test]
async fn a_release_restamped_with_a_newer_offer_than_its_arm_captures_nothing() {
    let scratch = Scratch::new("screen-hold-restamped");
    let (mut brain, _rig, _keep) = desk(&scratch).await;
    let (out, mut output) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
    let ready_notify = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
    brain.coordinator.handle(signed_in(7)).unwrap();
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let g = KEY.gesture;
    let first = brain.holds.notice;
    assert!(
        brain
            .hold_control(&eligible(recognized(g), first), &mut link)
            .unwrap()
    );
    prepared(&mut brain);
    brain.coordinator.handle(signed_in(8)).unwrap();
    brain.screen_holds_changed(&mut link).unwrap();
    controls(&mut output);
    let newer = brain.holds.notice;
    assert!(newer.is_some() && newer != first);
    assert!(matches!(part(&brain), ScreenPart::Armed));
    let released = eligible(release(g, 100, 200, 0), newer);
    assert!(brain.hold_control(&released, &mut link).unwrap());
    assert_eq!(controls(&mut output), [failed(g)]);
    assert!(!matches!(part(&brain), ScreenPart::Capturing { .. }));
    brain.holds.lost();
    brain.close().await;
}

/// The connected shell's settings change, taken as the brain takes it.
fn taken(brain: &mut Brain, link: &mut Link<'_, '_>, op: SettingsOp) {
    assert_eq!(
        brain.control_frame(ControlToBrain::Settings(op), 0, link, &mut None),
        Ok(None)
    );
}

/// A fresh screen hold `gesture` under `notice` arms nothing, and its release captures nothing;
/// it goes on as voice.
fn captures_nothing(
    brain: &mut Brain,
    link: &mut Link<'_, '_>,
    output: &mut mpsc::Receiver<Vec<u8>>,
    gesture: u32,
    notice: Option<kumochi_protocol::message::NoticeRevision>,
) {
    let g = GestureId(gesture);
    brain.holds.debt = None;
    assert!(
        brain
            .hold_control(&eligible(recognized(g), notice), link)
            .unwrap()
    );
    assert!(
        matches!(part(brain), ScreenPart::None),
        "{gesture}: {:?}",
        part(brain)
    );
    controls(output);
    assert!(
        brain
            .hold_control(&eligible(release(g, 100, 200, 2), notice), link)
            .unwrap()
    );
    assert_eq!(controls(output), [failed(g)], "{gesture}");
    assert!(
        matches!(part(brain), ScreenPart::None),
        "{gesture}: {:?}",
        part(brain)
    );
    brain.holds.lost();
}

/// Show-screen Off closes display admission as it's taken, through a save ahead of it, its own
/// save, a new connection and an On queued behind it: a fresh hold under the old offer or the
/// current one arms and captures nothing. A refused save opens it again at once under a new
/// offer; a saved Off keeps it closed until the On is saved. The old offer never comes back, and
/// the window choice and voice holds go on throughout.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one Off through its queue, a reconnect and its save, in order"
)]
async fn show_screen_off_closes_display_admission_until_its_save_fails() {
    use crate::runtime::settings::{Gate, written};
    for (ahead, save) in [
        (false, Gate::Panic),
        (true, Gate::Panic),
        (false, Gate::Write),
        (true, Gate::Write),
    ] {
        let refused = matches!(save, Gate::Panic);
        let case = format!("ahead={ahead} refused={refused}");
        let scratch = Scratch::new(&format!("screen-hold-off-pending-{ahead}-{refused}"));
        let (mut brain, mut rig, _keep) = desk(&scratch).await;
        let (out, mut output) = mpsc::channel(64);
        let queue = RefCell::new(DataQueue::new(crate::runtime::window()));
        let ready_notify = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready_notify, &mut tap);
        brain.screen_holds_changed(&mut link).unwrap();
        let old = brain.holds.notice;
        let choice = brain.config.view().default_model.clone();
        controls(&mut output);
        let first = brain.hold_config_write();
        if ahead {
            taken(&mut brain, &mut link, SettingsOp::SetTaskbar(true));
        }
        taken(&mut brain, &mut link, SettingsOp::SetShowScreen(false));
        taken(&mut brain, &mut link, SettingsOp::SetShowScreen(true));
        assert_eq!(
            gates(&mut output),
            [ScreenHolds::Off(ScreenOff::TurnedOff)],
            "{case}"
        );
        assert!(
            matches!(brain.screen_gate(choice.as_ref()), ScreenHolds::On { .. }),
            "{case}"
        );
        assert!(brain.hold_to_talk_enabled(), "{case}");
        captures_nothing(&mut brain, &mut link, &mut output, 50, old);
        let current = brain.holds.notice;
        captures_nothing(&mut brain, &mut link, &mut output, 51, current);
        let held = if ahead {
            let own = brain.hold_config_write();
            first.send(Gate::Write).unwrap();
            let done = written(&mut brain.config_write).await;
            brain.settings_done(done, Some(&mut link)).unwrap();
            brain.screen_holds_changed(&mut link).unwrap();
            captures_nothing(&mut brain, &mut link, &mut output, 52, old);
            own
        } else {
            first
        };
        brain.epoch += 1;
        brain.holds.opened();
        brain.screen_holds_changed(&mut link).unwrap();
        assert_eq!(
            gates(&mut output),
            [ScreenHolds::Off(ScreenOff::TurnedOff)],
            "{case}"
        );
        captures_nothing(&mut brain, &mut link, &mut output, 1, old);
        let current = brain.holds.notice;
        captures_nothing(&mut brain, &mut link, &mut output, 2, current);
        held.send(save).unwrap();
        let done = written(&mut brain.config_write).await;
        brain.settings_done(done, Some(&mut link)).unwrap();
        brain.screen_holds_changed(&mut link).unwrap();
        assert_eq!(brain.show_screen_enabled(), refused, "{case}");
        if !refused {
            captures_nothing(&mut brain, &mut link, &mut output, 3, old);
            let current = brain.holds.notice;
            captures_nothing(&mut brain, &mut link, &mut output, 4, current);
        }
        while brain.config_write.is_some() {
            let done = written(&mut brain.config_write).await;
            brain.settings_done(done, Some(&mut link)).unwrap();
        }
        brain.screen_holds_changed(&mut link).unwrap();
        let fresh = brain.holds.notice;
        assert!(
            matches!(gates(&mut output).last(), Some(ScreenHolds::On { revision, .. }) if Some(*revision) == fresh),
            "{case}"
        );
        assert_ne!(fresh, old, "{case}");
        captures_nothing(&mut brain, &mut link, &mut output, 5, old);
        let g = GestureId(6);
        brain.holds.debt = None;
        assert!(
            brain
                .hold_control(&eligible(recognized(g), fresh), &mut link)
                .unwrap()
        );
        assert!(matches!(part(&brain), ScreenPart::Armed), "{case}");
        prepared(&mut brain);
        controls(&mut output);
        assert!(
            brain
                .hold_control(&eligible(release(g, 100, 200, 2), fresh), &mut link)
                .unwrap()
        );
        assert!(
            matches!(part(&brain), ScreenPart::Capturing { .. }),
            "{case}"
        );
        let (_id, request) = rig.next().await;
        assert!(matches!(request, Request::Display { .. }), "{request:?}");
        brain.holds.lost();
        crate::screen::release_next(
            &mut rig,
            HoldKey {
                epoch: brain.epoch,
                gesture: g,
            },
        )
        .await;
        brain.close().await;
    }
}
