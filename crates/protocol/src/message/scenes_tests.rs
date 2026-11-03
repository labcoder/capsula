use super::{
    MAX_SCENE_LOG_ENTRIES, SceneCut, SceneCuts, SceneDataError, SceneDescriptorV1, SceneFailure,
    SceneHeader, SceneSnapshotChunk,
};
use crate::message::SceneId;
use kumochi_scene::{
    Begin, Handle, SceneEvent, Step, StopCut, Text, WireId,
    record::{Start, Surface, Version},
    text::Bounded,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct RawDescriptor {
    scene: SceneId,
    revision: u32,
    committed: u32,
    log_entries: u16,
}

#[derive(Serialize)]
struct RawChunk {
    scene: SceneId,
    revision: u32,
    ordinal: u16,
    header: Option<SceneHeader>,
    events: Vec<SceneEvent>,
    last: bool,
}

#[derive(Serialize)]
struct RawCut {
    scene: SceneId,
    cut: RawStop,
}
#[derive(Serialize)]
struct RawStop {
    activated: u32,
    revealed: Vec<Handle>,
}

fn decode<T: serde::de::DeserializeOwned>(raw: &impl Serialize) -> postcard::Result<T> {
    postcard::from_bytes(&postcard::to_allocvec(raw).expect("encodes"))
}

fn header() -> SceneHeader {
    SceneHeader {
        begin: Begin {
            v: Version::V1,
            scene: WireId::try_from("private-model-id".to_owned()).expect("valid id"),
            surface: Surface::Overlay,
            size: None,
            title: Some(Text::try_from("private-title".to_owned()).expect("valid text")),
            alt: Text::try_from("private-alt".to_owned()).expect("valid text"),
        },
        layout_version: 1,
    }
}

fn step(revision: u32) -> SceneEvent {
    SceneEvent::Step {
        revision,
        step: Arc::new(Step {
            id: WireId::try_from("private-step-id".to_owned()).expect("valid id"),
            say: Some(Text::try_from("private-speech".to_owned()).expect("valid text")),
            put: Bounded::default(),
            set: Bounded::default(),
            remove: Bounded::default(),
            start: Start::default(),
        }),
    }
}

fn cut(scene: u64, count: usize) -> SceneCut {
    SceneCut {
        scene: SceneId(scene),
        cut: StopCut {
            activated: 64,
            revealed: Bounded::new(
                (0..count)
                    .map(|slot| Handle::new(64, slot).expect("valid slot"))
                    .collect(),
            )
            .expect("bounded handles"),
        },
    }
}

#[test]
fn descriptor_checks_visible_counts_and_lifetime_log_bounds() {
    for (revision, committed, log_entries) in [(1, 1, 1), (64, 1, 128), (64, 64, 64)] {
        let value = decode::<SceneDescriptorV1>(&RawDescriptor {
            scene: SceneId(1),
            revision,
            committed,
            log_entries,
        })
        .expect("valid descriptor");
        assert_eq!(
            SceneDescriptorV1::new(SceneId(1), revision, committed, log_entries),
            Ok(value)
        );
        assert_eq!(
            (value.revision, value.committed, value.log_entries),
            (revision, committed, log_entries)
        );
    }
    for (revision, committed, log_entries) in [
        (0, 0, 0),
        (65, 1, 65),
        (1, 0, 1),
        (1, 2, 1),
        (2, 1, 1),
        (2, 1, 5),
        (u32::MAX, 1, 1),
    ] {
        assert_eq!(
            SceneDescriptorV1::new(SceneId(1), revision, committed, log_entries),
            Err(SceneDataError::Descriptor)
        );
        assert!(
            decode::<SceneDescriptorV1>(&RawDescriptor {
                scene: SceneId(1),
                revision,
                committed,
                log_entries
            })
            .is_err()
        );
    }
}

#[test]
fn cuts_bound_scenes_and_handles_on_construction_and_decode() {
    let valid = vec![cut(1, 600), cut(2, 600), cut(3, 300), cut(4, 0)];
    let cuts = SceneCuts::new(valid.clone()).expect("at aggregate bound");
    assert_eq!(decode::<SceneCuts>(&valid), Ok(cuts));
    assert_eq!(
        decode::<SceneCuts>(&Vec::<SceneCut>::new()),
        Ok(SceneCuts::default())
    );
    let mut duplicate_handle = cut(1, 1);
    duplicate_handle.cut.revealed =
        Bounded::new(vec![Handle::new(64, 0).expect("valid"); 2]).expect("bounded");
    for invalid in [
        vec![cut(1, 0); 5],
        vec![cut(1, 0); 2],
        vec![cut(1, 600), cut(2, 600), cut(3, 301)],
        vec![duplicate_handle],
    ] {
        assert_eq!(SceneCuts::new(invalid.clone()), Err(SceneDataError::Cuts));
        assert!(decode::<SceneCuts>(&invalid).is_err());
    }
    let handles = (0..601)
        .map(|slot| Handle::new(64, slot).expect("valid distinct slot"))
        .collect();
    assert!(
        decode::<SceneCuts>(&vec![RawCut {
            scene: SceneId(1),
            cut: RawStop {
                activated: 64,
                revealed: handles
            }
        }])
        .is_err()
    );
}

fn chunk() -> RawChunk {
    RawChunk {
        scene: SceneId(1),
        revision: 64,
        ordinal: 0,
        header: Some(header()),
        events: vec![step(1)],
        last: true,
    }
}

#[test]
fn chunks_validate_local_shape_without_cross_chunk_replay() {
    let mut raw = chunk();
    raw.revision = 1;
    raw.events = vec![step(2)];
    assert_eq!(
        decode::<SceneSnapshotChunk>(&raw),
        Err(postcard::Error::SerdeDeCustom)
    );
    let mut raw = chunk();
    raw.events = vec![step(64); MAX_SCENE_LOG_ENTRIES];
    let value = decode::<SceneSnapshotChunk>(&raw).expect("at log bound");
    assert_eq!(
        SceneSnapshotChunk {
            header: None,
            ..value.clone()
        }
        .checked(),
        Err(SceneDataError::Chunk)
    );
    assert_eq!(value.events.len(), 128);
    assert_eq!(decode::<SceneSnapshotChunk>(&value), Ok(value));
    raw.events.push(step(1));
    assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
    for revision in [0, 65, u32::MAX] {
        let mut raw = chunk();
        raw.revision = revision;
        assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
        let mut raw = chunk();
        raw.events = vec![step(revision)];
        assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
    }
    let mut raw = chunk();
    raw.events.clear();
    assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
    let mut raw = chunk();
    raw.header = None;
    assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
    let mut raw = chunk();
    raw.ordinal = 1;
    assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
    raw.header = None;
    raw.ordinal = 127;
    assert!(decode::<SceneSnapshotChunk>(&raw).is_ok());
    raw.ordinal = 128;
    assert!(decode::<SceneSnapshotChunk>(&raw).is_err());
}

#[test]
fn debug_keeps_model_ids_and_text_out_of_headers_and_logs() {
    let mut raw = chunk();
    let kumochi_scene::Record::Step(step) = kumochi_scene::parse_record(
        br#"{"step":{"id":"private-step-id","say":"private-speech","put":[{"text":{"id":"private-object-id","at":[13337.25,0],"text":"private-text"}}]}}"#,
    )
    .expect("valid step") else {
        panic!("step record")
    };
    raw.events = vec![SceneEvent::Step {
        revision: 1,
        step: Arc::new(step),
    }];
    let value = decode::<SceneSnapshotChunk>(&raw).expect("valid chunk");
    let debug = format!("{value:?}");
    for private in [
        "private-model-id",
        "private-title",
        "private-alt",
        "private-step-id",
        "private-speech",
        "private-object-id",
        "private-text",
        "13337.25",
    ] {
        assert!(!debug.contains(private));
    }
    let bytes = postcard::to_allocvec(&value).expect("encodes");
    for end in 0..bytes.len() {
        assert!(postcard::from_bytes::<SceneSnapshotChunk>(&bytes[..end]).is_err());
    }
}

#[test]
fn failures_round_trip_without_content() {
    for (tag, failure) in (0..).zip([
        SceneFailure::Cancelled,
        SceneFailure::Timeout,
        SceneFailure::Validation,
        SceneFailure::Layout,
        SceneFailure::Residency,
        SceneFailure::TextMeasure,
    ]) {
        assert_eq!(postcard::to_allocvec(&failure), Ok(vec![tag]));
        assert_eq!(decode::<SceneFailure>(&failure), Ok(failure));
    }
}
