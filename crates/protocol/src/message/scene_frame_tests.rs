use std::sync::Arc;

use kumochi_scene::{
    Admission, GlobalBudget, Handle, Record, SceneEvent, SceneState, Step, StopCut,
    limits::{MAX_GLOBAL_LIVE_OBJECTS, MAX_LIVE_OBJECTS, MAX_RECORD_BYTES},
    parse_record,
    text::Bounded,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use super::{
    ControlToBrain, ControlToShell, DataToShell, OutputGeneration, RunId, SceneCut, SceneCuts,
    SceneDescriptorV1, SceneFailure, SceneHeader, SceneId, SceneSnapshotChunk, SegmentId,
    SegmentStart, SpeechPromise, StepRef,
};
use crate::frame::{self, HEADER_LEN, Pipe};

fn header() -> SceneHeader {
    let Record::Begin(begin) = parse_record(
        br#"{"begin":{"v":1,"scene":"secret_scene","surface":"overlay","title":"secret title","alt":"secret alt"}}"#,
    )
    .expect("valid begin") else {
        panic!("begin record")
    };
    SceneHeader {
        begin,
        layout_version: 1,
    }
}

fn step() -> Step {
    let Record::Step(step) = parse_record(
        br#"{"step":{"id":"secret_step","say":"secret speech","put":[{"text":{"id":"secret_object","at":[13337.25,0],"text":"secret pixels"}}]}}"#,
    )
    .expect("valid step") else {
        panic!("step record")
    };
    step
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    value: &T,
    pipe: Pipe,
) {
    let encoded = frame::encode(value, pipe).expect("fits the pipe");
    assert_eq!(
        frame::decode::<T>(&encoded[HEADER_LEN..]).expect("decodes"),
        *value
    );
    let json = serde_json::to_vec(value).expect("JSON encodes");
    assert_eq!(
        serde_json::from_slice::<T>(&json).expect("JSON decodes"),
        *value
    );
}

#[test]
fn replacement_marker_round_trips_four_victims_and_rejects_truncation() {
    let marker = DataToShell::ScenesReplaced {
        scenes: vec![SceneId(1), SceneId(2), SceneId(3), SceneId(4)],
        successor: SceneId(5),
    };
    round_trip(&marker, Pipe::Data);
    let encoded = frame::encode(&marker, Pipe::Data).expect("bounded marker");
    assert_eq!(encoded.len() - HEADER_LEN, 7);
    for end in HEADER_LEN..encoded.len() {
        assert!(matches!(
            frame::decode::<DataToShell>(&encoded[HEADER_LEN..end]),
            Err(frame::FrameError::Malformed(_))
        ));
    }
}

#[test]
fn drawing_failure_controls_append_tags_and_keep_their_opposite_bindings() {
    use super::{ControlToShell, ThreadId};
    let failed = ControlToBrain::SceneFailed { scene: SceneId(9) };
    let notice = ControlToShell::DrawingReplacementFailed {
        thread: ThreadId(7),
        run: RunId(3),
    };
    let nothing = ControlToShell::DrawingNotReturned {
        thread: ThreadId(7),
        run: RunId(3),
    };
    round_trip(&failed, Pipe::Control);
    round_trip(&notice, Pipe::Control);
    round_trip(&nothing, Pipe::Control);
    assert_eq!(postcard::to_allocvec(&failed).expect("failure"), [26, 9]);
    assert_eq!(postcard::to_allocvec(&notice).expect("notice"), [26, 7, 3]);
    assert_eq!(
        postcard::to_allocvec(&nothing).expect("nothing"),
        [27, 7, 3]
    );
}

fn snapshot(begin: SceneHeader, step: Step) -> DataToShell {
    DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene: SceneId(u64::MAX),
            revision: 1,
            ordinal: 0,
            header: Some(begin),
            events: Bounded::new(vec![SceneEvent::Step {
                revision: 1,
                step: Arc::new(step),
            }])
            .expect("one event"),
            last: true,
        },
    }
}

fn cut() -> StopCut {
    StopCut {
        activated: 1,
        revealed: Bounded::new(vec![Handle::new(1, 0).expect("handle")]).expect("one handle"),
    }
}

fn controls() -> Vec<ControlToBrain> {
    vec![
        ControlToBrain::StopScenes {
            run: RunId(u64::MAX),
            cuts: SceneCuts::new(vec![SceneCut {
                scene: SceneId(u64::MAX),
                cut: cut(),
            }])
            .expect("one cut"),
        },
        ControlToBrain::StepShown {
            run: RunId(u64::MAX),
            scene: SceneId(u64::MAX),
            index: 0,
        },
        ControlToBrain::StepFailed {
            run: RunId(u64::MAX),
            scene: SceneId(u64::MAX),
            index: 0,
            reason: SceneFailure::Timeout,
            cut: cut(),
        },
    ]
}

fn data() -> Vec<DataToShell> {
    vec![
        DataToShell::SceneBegin {
            scene: SceneId(u64::MAX),
            header: header(),
        },
        DataToShell::SceneStepCommit {
            run: RunId(u64::MAX),
            scene: SceneId(u64::MAX),
            index: 0,
            step: step(),
            speech: None,
        },
        snapshot(header(), step()),
        DataToShell::SceneCutApplied {
            scene: SceneId(u64::MAX),
            revision: 1,
            cut: cut(),
        },
        DataToShell::SceneEnd {
            run: RunId(u64::MAX),
            scene: SceneId(u64::MAX),
            complete: false,
        },
    ]
}

#[test]
fn v1_tags_are_appended_and_messages_round_trip_in_both_encodings() {
    for (tag, value) in (8..).zip(controls()) {
        assert_eq!(postcard::to_allocvec(&value).expect("encodes")[0], tag);
        round_trip(&value, Pipe::Control);
    }
    for (tag, value) in (8..).zip(data()) {
        assert_eq!(postcard::to_allocvec(&value).expect("encodes")[0], tag);
        round_trip(&value, Pipe::Data);
    }
    for (tag, value) in [
        (
            9,
            ControlToShell::ScenesV1 {
                scenes: Bounded::new(vec![
                    SceneDescriptorV1 {
                        scene: SceneId(u64::MAX),
                        revision: 64,
                        committed: 1,
                        log_entries: 128
                    };
                    4
                ])
                .expect("four descriptors"),
            },
        ),
        (
            10,
            ControlToShell::NoVisual {
                run: RunId(u64::MAX),
            },
        ),
    ] {
        assert_eq!(postcard::to_allocvec(&value).expect("encodes")[0], tag);
        round_trip(&value, Pipe::Control);
    }
}

#[test]
fn enum_decode_preserves_descriptor_and_cut_bounds() {
    let descriptor = SceneDescriptorV1 {
        scene: SceneId(1),
        revision: 1,
        committed: 1,
        log_entries: 1,
    };
    let mut payload = vec![9];
    payload.extend(postcard::to_allocvec(&vec![descriptor; 5]).expect("raw descriptor list"));
    assert!(matches!(
        frame::decode::<ControlToShell>(&payload),
        Err(frame::FrameError::Malformed(postcard::Error::SerdeDeCustom))
    ));
    let mut payload = vec![8];
    payload.extend(postcard::to_allocvec(&RunId(1)).expect("run"));
    payload.extend(
        postcard::to_allocvec(&vec![
            SceneCut {
                scene: SceneId(1),
                cut: cut()
            };
            2
        ])
        .expect("raw repeated scene cuts"),
    );
    assert!(matches!(
        frame::decode::<ControlToBrain>(&payload),
        Err(frame::FrameError::Malformed(postcard::Error::SerdeDeCustom))
    ));
}

#[test]
fn number_dense_legal_step_and_snapshot_event_fit_the_data_frame() {
    let mut put = Vec::new();
    for chart in 0..4 {
        put.push(json!({"chart":{"id":format!("c{chart}"),"kind":"line","box":[0,0,100,100]}}));
        for series in 0..if chart == 3 { 7 } else { 8 } {
            put.push(json!({"series":{"id":format!("s{chart}_{series}"),"in":format!("c{chart}"),"data":vec![0;500]}}));
        }
    }
    let json = serde_json::to_vec(&json!({"step":{"id":"dense","put":put}})).expect("encodes");
    assert_eq!(json.len(), 32_619);
    assert!(json.len() <= MAX_RECORD_BYTES);
    let Record::Step(step) = parse_record(&json).expect("bounded typed step") else {
        panic!("step")
    };
    let mut state = SceneState::begin(header().begin).expect("begin");
    let mut budget = GlobalBudget::default();
    let Admission::New(admitted) = state.admit(&step, &budget).expect("semantically legal") else {
        panic!("new step")
    };
    state.commit(admitted, &mut budget).expect("commit");
    let live = DataToShell::SceneStepCommit {
        run: RunId(u64::MAX),
        scene: SceneId(u64::MAX),
        index: 63,
        step: step.clone(),
        speech: None,
    };
    let bytes = frame::encode(&live, Pipe::Data).expect("dense live frame fits");
    assert!(
        bytes.len() > json.len() * 4,
        "exercise numeric expansion, not text compression"
    );
    round_trip(&live, Pipe::Data);
    round_trip(&snapshot(header(), step), Pipe::Data);
}

#[test]
fn live_speech_metadata_is_required_and_not_part_of_snapshots() {
    let run = RunId(1);
    let scene = SceneId(2);
    let step_ref = StepRef { scene, index: 0 };
    let segment = SegmentId(3);
    let output_generation = OutputGeneration(17);
    let promise = SpeechPromise {
        output_generation,
        segment,
    };
    let step = step();
    for (message, suffix) in [
        (
            DataToShell::SegmentQueued {
                run,
                segment,
                step: Some(step_ref),
                start: SegmentStart::Speech,
                output_generation,
            },
            2,
        ),
        (
            DataToShell::SceneStepCommit {
                run,
                scene,
                index: 0,
                step: step.clone(),
                speech: Some(promise),
            },
            3,
        ),
        (
            DataToShell::SceneStepCommit {
                run,
                scene,
                index: 0,
                step: step.clone(),
                speech: None,
            },
            1,
        ),
    ] {
        round_trip(&message, Pipe::Data);
        let mut payload = frame::encode_payload(&message).expect("payload");
        for missing in 1..=suffix {
            assert!(matches!(
                frame::decode::<DataToShell>(&payload[..payload.len() - missing]),
                Err(frame::FrameError::Malformed(
                    postcard::Error::DeserializeUnexpectedEnd
                ))
            ));
        }
        payload.push(0);
        assert!(matches!(
            frame::decode::<DataToShell>(&payload),
            Err(frame::FrameError::Malformed(
                postcard::Error::DeserializeBadEncoding
            ))
        ));
    }
    let snapshot = snapshot(header(), step.clone());
    let DataToShell::SceneSnapshotV1 { chunk } = &snapshot else {
        panic!("snapshot fixture")
    };
    let event = &chunk.events[0];
    let mut expected = vec![0, 1];
    expected.extend(postcard::to_allocvec(&step).unwrap());
    assert_eq!(postcard::to_allocvec(event).unwrap(), expected);
    assert_eq!(frame::decode::<SceneEvent>(&expected).unwrap(), *event);
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({"step":{"revision":1,"step":step}})
    );
    round_trip(&snapshot, Pipe::Data);
}

#[test]
fn global_handle_cut_fits_the_control_cap() {
    assert_eq!(MAX_GLOBAL_LIVE_OBJECTS, 1_500);
    let mut remaining = 1_500;
    let cuts = (0..4)
        .map(|scene| {
            let count = remaining.min(MAX_LIVE_OBJECTS);
            remaining -= count;
            SceneCut {
                scene: SceneId(u64::MAX - scene),
                cut: StopCut {
                    activated: 64,
                    revealed: Bounded::new(
                        (0..count)
                            .map(|slot| Handle::new(64, slot).expect("maximum revision handle"))
                            .collect(),
                    )
                    .expect("bounded scene"),
                },
            }
        })
        .collect();
    assert_eq!(remaining, 0);
    let message = ControlToBrain::StopScenes {
        run: RunId(u64::MAX),
        cuts: SceneCuts::new(cuts).expect("global cap"),
    };
    round_trip(&message, Pipe::Control);
}

#[test]
fn all_scene_messages_are_redacted_even_in_forged_tap_records() {
    use crate::tap::{self, Direction, Record as TapRecord, Tapped};

    fn check<M: Tapped + std::fmt::Debug>(message: &M, pipe: Pipe, direction: Direction) {
        assert!(!message.content_free());
        let bytes = frame::encode(message, pipe).expect("encodes");
        let record =
            TapRecord::of(pipe, direction, message, bytes.len() - HEADER_LEN).expect("records");
        assert_eq!(record.payload, None);
        let forged = TapRecord {
            payload: Some(bytes[HEADER_LEN..].to_vec()),
            ..record.clone()
        };
        assert_eq!(
            forged.describe(),
            format!("{} ({} bytes)", message.op(), bytes.len() - HEADER_LEN)
        );
        let mut file = Vec::new();
        tap::write_header(&mut file).expect("header");
        tap::write(&mut file, &record).expect("writes");
        let mut forged_file = Vec::new();
        tap::write_header(&mut forged_file).expect("header");
        tap::write(&mut forged_file, &forged).expect("writes forged payload");
        let mut dump = Vec::new();
        tap::dump(&mut forged_file.as_slice(), &mut dump)
            .expect("dumps forged payload without fields");
        let dump = String::from_utf8(dump).expect("utf8");
        let debug = format!("{message:?}");
        for secret in [
            "secret_scene",
            "secret title",
            "secret alt",
            "secret_step",
            "secret speech",
            "secret_object",
            "secret pixels",
            "13337.25",
        ] {
            assert!(!file.windows(secret.len()).any(|w| w == secret.as_bytes()));
            assert!(!dump.contains(secret));
            assert!(!debug.contains(secret));
        }
    }
    for value in controls() {
        check(&value, Pipe::Control, Direction::ToBrain);
    }
    let messages = data();
    let snapshot = messages
        .iter()
        .find(|value| matches!(value, DataToShell::SceneSnapshotV1 { .. }))
        .expect("snapshot fixture");
    let payload = frame::encode_payload(snapshot).expect("encodes");
    for end in 0..payload.len() {
        assert!(frame::decode::<DataToShell>(&payload[..end]).is_err());
    }
    for value in messages {
        check(&value, Pipe::Data, Direction::ToShell);
    }
    check(
        &ControlToShell::ScenesV1 {
            scenes: Bounded::default(),
        },
        Pipe::Control,
        Direction::ToShell,
    );
    check(
        &ControlToShell::NoVisual { run: RunId(1) },
        Pipe::Control,
        Direction::ToShell,
    );
}
