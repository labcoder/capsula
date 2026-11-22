use serde::{Serialize, de::DeserializeOwned};

use super::{
    CaptureFailure, CaptureFor, Content, ControlToBrain, ControlToShell, DataToBrain, FocusHint,
    GestureId, LeaseGeneration, Phase, RunId, RunOrigin, ScreenChoice, ThreadId,
};
use crate::frame::{self, HEADER_LEN, Pipe};
use crate::geometry::{
    PathTooLong, PhysicalPoint, PhysicalRect, PointerPath, ReleasePointer, WindowRef,
};
use crate::tap::{self, Direction, Record, Tapped};

const GESTURE: GestureId = GestureId(u32::MAX);
const LEASE: LeaseGeneration = LeaseGeneration(u32::MAX);
const RECT: PhysicalRect = PhysicalRect {
    origin: [i32::MIN, i32::MAX],
    size: [u32::MAX; 2],
};

#[test]
fn voice_only_hold_controls_append_tags_without_screen_authority() {
    let recognized = ControlToBrain::VoiceHoldRecognized {
        gesture: GESTURE,
        thread: Some(ThreadId(u64::MAX)),
        selection: u64::MAX,
    };
    let stopped = ControlToBrain::VoiceHoldStopped { gesture: GESTURE };
    for (message, tag, size, op) in [
        (recognized, 27, 27, "VoiceHoldRecognized"),
        (stopped, 28, 6, "VoiceHoldStopped"),
    ] {
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], tag);
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToBrain, op, size);
    }
    for status in [
        super::HoldStatus::Admitted,
        super::HoldStatus::Loading,
        super::HoldStatus::Transcribing,
        super::HoldStatus::Empty,
        super::HoldStatus::Cancelled,
        super::HoldStatus::Failed(super::HoldFailure::NotLoaded),
        super::HoldStatus::Failed(super::HoldFailure::Busy),
        super::HoldStatus::Failed(super::HoldFailure::Unavailable),
        super::HoldStatus::Failed(super::HoldFailure::InvalidAudio),
        super::HoldStatus::Failed(super::HoldFailure::TooLong),
        super::HoldStatus::Failed(super::HoldFailure::TimedOut),
        super::HoldStatus::Failed(super::HoldFailure::ChoiceChanged),
        super::HoldStatus::Failed(super::HoldFailure::DecodeFailed),
    ] {
        let message = ControlToShell::VoiceHoldStatus {
            gesture: GESTURE,
            status,
        };
        let size = if matches!(status, super::HoldStatus::Failed(_)) {
            8
        } else {
            7
        };
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], 28);
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToShell, "VoiceHoldStatus", size);
    }
}

#[test]
fn voice_stop_acceptance_appends_a_redacted_control_tag() {
    let message = ControlToShell::VoiceHoldStopAccepted { gesture: GESTURE };
    assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], 30);
    round_trip(&message, Pipe::Control, 6);
    redacted(&message, Direction::ToShell, "VoiceHoldStopAccepted", 6);
}

#[test]
fn screen_hold_states_append_a_redacted_control_tag() {
    let model =
        super::ModelName::new(&"\u{1F600}".repeat(super::MAX_MODEL_NAME_CHARS)).expect("valid");
    for (state, size) in [
        (
            super::ScreenHolds::On {
                model,
                connection: super::ConnectionKind::ClaudeCode,
                slug: super::ModelSlug::new(&"a".repeat(super::MAX_SLUG_BYTES)).expect("valid"),
                revision: super::NoticeRevision(std::num::NonZeroU64::MAX),
            },
            1 + 1 + 2 + 4 * super::MAX_MODEL_NAME_CHARS + 1 + 1 + super::MAX_SLUG_BYTES + 10,
        ),
        (super::ScreenHolds::Off(super::ScreenOff::ModelCantSee), 3),
        (super::ScreenHolds::Off(super::ScreenOff::NotReady), 3),
        (super::ScreenHolds::Off(super::ScreenOff::Unsupported), 3),
        (super::ScreenHolds::Off(super::ScreenOff::TurnedOff), 3),
    ] {
        let message = ControlToShell::ScreenHolds(state);
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], 31);
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToShell, "ScreenHolds", size);
    }
    assert_eq!(
        postcard::to_allocvec(&super::ScreenHolds::Off(super::ScreenOff::TurnedOff))
            .expect("encodes"),
        [1, 3]
    );
    assert!(postcard::from_bytes::<super::NoticeRevision>(&[0]).is_err());
    assert_eq!(
        postcard::from_bytes::<super::NoticeRevision>(&[1]),
        Ok(super::NoticeRevision::FIRST)
    );
}

#[test]
fn a_held_screens_outcome_appends_a_redacted_control_tag() {
    use super::{HeldScreen, ScreenClass};
    let mut outcomes = vec![
        (HeldScreen::Attached, 7),
        (HeldScreen::Withdrawn, 7),
        (HeldScreen::Screen, 7),
    ];
    for (index, class) in [
        ScreenClass::NotReady,
        ScreenClass::NothingThere,
        ScreenClass::CantShare,
        ScreenClass::Unreadable,
        ScreenClass::TooLarge,
        ScreenClass::Changed,
        ScreenClass::ModelCantSee,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            postcard::to_allocvec(&class).expect("encodes"),
            [u8::try_from(index).expect("seven classes")]
        );
        outcomes.push((HeldScreen::NotIncluded(class), 8));
    }
    for (outcome, tag) in [
        (HeldScreen::Attached, 0),
        (HeldScreen::NotIncluded(ScreenClass::NotReady), 1),
        (HeldScreen::Withdrawn, 2),
        (HeldScreen::Screen, 3),
    ] {
        assert_eq!(postcard::to_allocvec(&outcome).expect("encodes")[0], tag);
    }
    for (outcome, size) in outcomes {
        let message = ControlToShell::HoldScreen {
            gesture: GESTURE,
            outcome,
        };
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], 32);
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToShell, "HoldScreen", size);
    }
}

fn round_trip<M: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    message: &M,
    pipe: Pipe,
    size: usize,
) {
    let frame = frame::encode(message, pipe).expect("encodes within cap");
    assert_eq!(frame.len(), HEADER_LEN + size);
    assert!(size <= pipe.max_payload());
    assert_eq!(
        frame::decode::<M>(&frame[HEADER_LEN..]).expect("decodes"),
        *message
    );
}

fn redacted(message: &impl Tapped, direction: Direction, op: &str, size: usize) {
    let record = Record::of(Pipe::Control, direction, message, size).expect("records");
    assert_eq!(
        record,
        Record {
            pipe: Pipe::Control,
            direction,
            op: op.to_owned(),
            len: u32::try_from(size).expect("small frame"),
            payload: None,
        }
    );
    assert_eq!(record.describe(), format!("{op} ({size} bytes)"));
    let mut bytes = Vec::new();
    tap::write(&mut bytes, &record).expect("writes");
    let mut expected = vec![
        0,
        u8::from(direction == Direction::ToShell),
        u8::try_from(op.len()).expect("short op"),
    ];
    expected.extend(op.as_bytes());
    expected.extend(u32::try_from(size).expect("small frame").to_le_bytes());
    expected.push(0);
    assert_eq!(
        bytes, expected,
        "only the op, length and framing are recorded"
    );
    assert_eq!(
        tap::read(&mut bytes.as_slice()).expect("reads"),
        Some(record.clone())
    );
    let mut forged = record;
    forged.payload = Some(postcard::to_allocvec(message).expect("encodes"));
    assert_eq!(forged.describe(), format!("{op} ({size} bytes)"));
}

#[test]
fn largest_hold_controls_round_trip_fit_the_cap_and_stay_redacted() {
    for (message, op, size) in [
        (
            ControlToBrain::HoldRecognized {
                gesture: GESTURE,
                thread: Some(ThreadId(u64::MAX)),
                selection: u64::MAX,
                pointer: PhysicalPoint([i32::MIN, i32::MAX]),
                lease: LEASE,
                t_qpc: u64::MAX,
                notice: Some(super::NoticeRevision(std::num::NonZeroU64::MAX)),
            },
            "HoldRecognized",
            63,
        ),
        (
            ControlToBrain::KeyDown {
                gesture: GESTURE,
                t_qpc: u64::MAX,
            },
            "KeyDown",
            16,
        ),
        (
            ControlToBrain::KeyUp {
                gesture: GESTURE,
                t_qpc: u64::MAX,
                chunks: u32::MAX,
            },
            "KeyUp",
            21,
        ),
        (
            ControlToBrain::GestureCancel { gesture: GESTURE },
            "GestureCancel",
            6,
        ),
        (
            ControlToBrain::FocusHint {
                gesture: GESTURE,
                hint: FocusHint::Region(RECT),
            },
            "FocusHint",
            27,
        ),
        (
            ControlToBrain::FocusHint {
                gesture: GESTURE,
                hint: FocusHint::Window(WindowRef(u64::MAX)),
            },
            "FocusHint",
            17,
        ),
        (
            ControlToBrain::ExclusionLapsed { lease: LEASE },
            "ExclusionLapsed",
            6,
        ),
    ] {
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToBrain, op, size);
    }
    round_trip(
        &ControlToBrain::HoldRecognized {
            gesture: GestureId(0),
            thread: None,
            selection: 0,
            pointer: PhysicalPoint([0; 2]),
            lease: LeaseGeneration(0),
            t_qpc: 0,
            notice: None,
        },
        Pipe::Control,
        9,
    );
}

#[test]
fn capture_results_round_trip_fit_the_cap_and_stay_redacted() {
    for capture_for in [CaptureFor::Gesture(GESTURE), CaptureFor::Turn(u32::MAX)] {
        let done = ControlToShell::CaptureDone { capture_for };
        round_trip(&done, Pipe::Control, 7);
        redacted(&done, Direction::ToShell, "CaptureDone", 7);
        for reason in [
            CaptureFailure::ExclusionUnavailable,
            CaptureFailure::Cancelled,
            CaptureFailure::Unavailable,
        ] {
            let failed = ControlToShell::CaptureFailed {
                capture_for,
                reason,
            };
            round_trip(&failed, Pipe::Control, 8);
            redacted(&failed, Direction::ToShell, "CaptureFailed", 8);
        }
    }
}

#[test]
fn run_origins_screen_choices_and_refusals_round_trip() {
    for origin in [
        RunOrigin::Typed { turn: u32::MAX },
        RunOrigin::Hold { gesture: GESTURE },
    ] {
        round_trip(
            &ControlToShell::RunState {
                run: RunId(u64::MAX),
                thread: ThreadId(u64::MAX),
                phase: Phase::Listening,
                error: None,
                origin: Some(origin),
            },
            Pipe::Control,
            30,
        );
    }
    round_trip(
        &ControlToShell::RunState {
            run: RunId(u64::MAX),
            thread: ThreadId(u64::MAX),
            phase: Phase::Answering,
            error: None,
            origin: None,
        },
        Pipe::Control,
        24,
    );
    round_trip(
        &ControlToShell::TurnRefused {
            turn: u32::MAX,
            thread: Some(ThreadId(u64::MAX)),
        },
        Pipe::Control,
        17,
    );
    for (screen, size) in [
        (ScreenChoice::None, 19),
        (ScreenChoice::Include { lease: LEASE }, 24),
    ] {
        round_trip(
            &DataToBrain::TextSubmit {
                turn: u32::MAX,
                screen,
                thread: Some(ThreadId(u64::MAX)),
                text: Content::default(),
            },
            Pipe::Data,
            size,
        );
    }
}

#[test]
fn a_skipped_scene_appends_a_redacted_tag_after_the_screen_release() {
    let skipped = ControlToBrain::SceneSkipped {
        scene: super::SceneId(u64::MAX),
    };
    assert_eq!(postcard::to_allocvec(&skipped).expect("encodes")[0], 31);
    round_trip(&skipped, Pipe::Control, 11);
    redacted(&skipped, Direction::ToBrain, "SceneSkipped", 11);
    let skipped = ControlToBrain::SceneSkipped {
        scene: super::SceneId(2),
    };
    assert_eq!(postcard::to_allocvec(&skipped).expect("encodes"), [31, 2]);
}

#[test]
fn a_ready_step_appends_a_redacted_tag_after_the_skipped_scene() {
    let ready = ControlToBrain::StepReady {
        run: RunId(u64::MAX),
        scene: super::SceneId(u64::MAX),
        index: u32::MAX,
    };
    assert_eq!(postcard::to_allocvec(&ready).expect("encodes")[0], 32);
    round_trip(&ready, Pipe::Control, 26);
    redacted(&ready, Direction::ToBrain, "StepReady", 26);
    let ready = ControlToBrain::StepReady {
        run: RunId(1),
        scene: super::SceneId(2),
        index: 3,
    };
    assert_eq!(
        postcard::to_allocvec(&ready).expect("encodes"),
        [32, 1, 2, 3]
    );
}

#[test]
fn an_output_withdrawal_appends_a_redacted_tag_after_the_ready_step() {
    let withdrawn = ControlToBrain::AudioOutputUnavailable {
        generation: super::OutputGeneration(u32::MAX),
    };
    assert_eq!(postcard::to_allocvec(&withdrawn).expect("encodes")[0], 33);
    round_trip(&withdrawn, Pipe::Control, 6);
    redacted(&withdrawn, Direction::ToBrain, "AudioOutputUnavailable", 6);
    let withdrawn = ControlToBrain::AudioOutputUnavailable {
        generation: super::OutputGeneration(2),
    };
    assert_eq!(postcard::to_allocvec(&withdrawn).expect("encodes"), [33, 2]);
}

#[test]
fn a_screen_holds_controls_append_redacted_tags_and_fit_the_control_cap() {
    let recognized = ControlToBrain::ScreenHoldRecognized {
        gesture: GESTURE,
        thread: Some(ThreadId(u64::MAX)),
        selection: u64::MAX,
        t_qpc: u64::MAX,
        notice: Some(super::NoticeRevision(std::num::NonZeroU64::MAX)),
    };
    let corner = PhysicalPoint([i32::MIN, i32::MAX]);
    let release = ControlToBrain::ScreenRelease {
        gesture: GESTURE,
        lease: LEASE,
        t_qpc: u64::MAX,
        excluded_qpc: u64::MAX,
        pointer: ReleasePointer(corner),
        path: PointerPath(vec![corner; 2_048]),
        notice: Some(super::NoticeRevision(std::num::NonZeroU64::MAX)),
    };
    for (message, tag, size, op) in [
        (recognized, 29, 48, "ScreenHoldRecognized"),
        (release, 30, 20_534, "ScreenRelease"),
    ] {
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], tag);
        round_trip(&message, Pipe::Control, size);
        redacted(&message, Direction::ToBrain, op, size);
    }
    let recognized = ControlToBrain::ScreenHoldRecognized {
        gesture: GestureId(1),
        thread: None,
        selection: 2,
        t_qpc: 3,
        notice: Some(super::NoticeRevision::FIRST.next()),
    };
    assert_eq!(
        postcard::to_allocvec(&recognized).expect("encodes"),
        [29, 1, 0, 2, 3, 1, 2]
    );
    let release = |path| ControlToBrain::ScreenRelease {
        gesture: GestureId(1),
        lease: LeaseGeneration(2),
        t_qpc: 3,
        excluded_qpc: 4,
        pointer: ReleasePointer(PhysicalPoint([5, -6])),
        path: PointerPath(path),
        notice: None,
    };
    for (path, bytes) in [
        (Vec::new(), &[30, 1, 2, 3, 4, 10, 11, 0, 0][..]),
        (
            vec![PhysicalPoint([7, 8]), PhysicalPoint([-1, 0])],
            &[30, 1, 2, 3, 4, 10, 11, 2, 14, 16, 1, 0, 0][..],
        ),
    ] {
        let message = release(path);
        assert_eq!(postcard::to_allocvec(&message).expect("encodes"), bytes);
        round_trip(&message, Pipe::Control, bytes.len());
    }
}

#[test]
fn a_pointer_path_holds_at_most_2048_points() {
    let path = |points| PointerPath(vec![PhysicalPoint([1, 2]); points]);
    assert_eq!(path(0).check(), Ok(()));
    assert_eq!(path(2_048).check(), Ok(()));
    assert_eq!(path(2_049).check(), Err(PathTooLong));
}

#[test]
fn where_the_user_pointed_is_absent_from_debug_and_from_the_tap() {
    const IN_PATH: i32 = 1_357_924_680;
    const AT_RELEASE: i32 = -1_246_813_579;
    let message = ControlToBrain::ScreenRelease {
        gesture: GestureId(1),
        lease: LeaseGeneration(2),
        t_qpc: 3,
        excluded_qpc: 4,
        pointer: ReleasePointer(PhysicalPoint([AT_RELEASE, AT_RELEASE])),
        path: PointerPath(vec![PhysicalPoint([IN_PATH, IN_PATH])]),
        notice: None,
    };
    assert_eq!(
        format!("{message:?}"),
        "ScreenRelease { gesture: GestureId(1), lease: LeaseGeneration(2), t_qpc: 3, \
         excluded_qpc: 4, pointer: ReleasePointer { .. }, path: PointerPath(1), notice: None }"
    );
    let payload = postcard::to_allocvec(&message).expect("encodes");
    let record =
        Record::of(Pipe::Control, Direction::ToBrain, &message, payload.len()).expect("records");
    let mut recording = Vec::new();
    tap::write(&mut recording, &record).expect("writes");
    let texts = [
        format!("{message:?}"),
        format!("{record:?}"),
        record.describe(),
    ];
    let within = |bytes: &[u8], part: &[u8]| bytes.windows(part.len()).any(|at| at == part);
    for sentinel in [IN_PATH, AT_RELEASE] {
        let wire = postcard::to_allocvec(&sentinel).expect("encodes");
        assert!(within(&payload, &wire), "the frame itself carries it");
        let listed = format!("{wire:?}");
        for text in &texts {
            assert!(!text.contains(&sentinel.unsigned_abs().to_string()));
            assert!(!text.contains(listed.trim_matches(['[', ']'])));
        }
        for bytes in [&wire[..], &sentinel.to_le_bytes(), &sentinel.to_be_bytes()] {
            assert!(!within(&recording, bytes));
        }
    }
}

#[test]
fn held_words_and_their_receipt_append_redacted_data_tags() {
    let sentinel = "zqxsentinellabel";
    let words = super::DataToShell::HoldWords {
        gesture: GESTURE,
        words: Content::from(sentinel),
    };
    let stored = super::DataToShell::HoldStored { gesture: GESTURE };
    for (message, tag, size, op) in [
        (words, 22, 7 + sentinel.len(), "HoldWords"),
        (stored, 23, 6, "HoldStored"),
    ] {
        assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], tag);
        round_trip(&message, Pipe::Data, size);
        let record = Record::of(Pipe::Data, Direction::ToShell, &message, size).expect("records");
        assert_eq!((record.op.as_str(), record.payload.as_ref()), (op, None));
        let mut bytes = Vec::new();
        tap::write(&mut bytes, &record).expect("writes");
        assert!(!format!("{message:?} {record:?} {bytes:?}").contains(sentinel));
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes())
        );
    }
    let longest = super::DataToShell::HoldWords {
        gesture: GESTURE,
        words: Content::from("a".repeat(16_384)),
    };
    let bytes = frame::encode(&longest, Pipe::Data).expect("max transcript fits");
    assert_eq!(
        frame::read(&mut bytes.as_slice(), Pipe::Data).expect("decodes"),
        Some(longest)
    );
}
#[test]
fn a_thread_events_change_appends_data_tag_24_and_an_unknown_tag_is_refused() {
    let message = super::DataToShell::EventsChanged {
        thread: ThreadId(u64::MAX),
    };
    assert_eq!(postcard::to_allocvec(&message).expect("encodes")[0], 24);
    round_trip(&message, Pipe::Data, 11);
    let record = Record::of(Pipe::Data, Direction::ToShell, &message, 11).expect("records");
    assert_eq!(
        (record.op.as_str(), record.payload.as_ref()),
        ("EventsChanged", None)
    );
    assert!(matches!(
        frame::decode::<super::DataToShell>(&[0x7f, 7]),
        Err(frame::FrameError::Malformed(_))
    ));
    assert_eq!(
        frame::decode::<super::DataToShell>(&[24, 7]).expect("decodes"),
        super::DataToShell::EventsChanged {
            thread: ThreadId(7)
        }
    );
}
