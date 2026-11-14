use serde::{Serialize, de::DeserializeOwned};

use super::{
    CaptureFailure, CaptureFor, Content, ControlToBrain, ControlToShell, DataToBrain, FocusHint,
    GestureId, LeaseGeneration, Phase, RunId, RunOrigin, ScreenChoice, ThreadId,
};
use crate::frame::{self, HEADER_LEN, Pipe};
use crate::geometry::{PhysicalPoint, PhysicalRect, WindowRef};
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
            super::ScreenHolds::On { model },
            1 + 1 + 2 + 4 * super::MAX_MODEL_NAME_CHARS,
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
}

#[test]
fn a_held_screens_outcome_appends_a_redacted_control_tag() {
    use super::{HeldScreen, ScreenClass};
    let mut outcomes = vec![(HeldScreen::Attached, 7)];
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
            },
            "HoldRecognized",
            52,
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
        },
        Pipe::Control,
        8,
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
