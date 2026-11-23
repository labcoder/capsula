use std::sync::Arc;

use kumochi_scene::{
    Begin, Step, StopCut, WireId,
    record::{Start, Surface, Version},
};

use super::*;

#[test]
#[ignore = "manual quiet-Rez staging memory measurement, not a CI memory assertion"]
fn measure_four_dense_lifetime_logs_before_semantic_recovery() {
    use kumochi_protocol::{frame, message::DataToShell};
    use kumochi_scene::{Record, SceneState, parse_record};
    use std::time::Duration;

    let data = std::iter::repeat_n("0", 500).collect::<Vec<_>>().join(",");
    let mut put = Vec::new();
    let mut patches = Vec::new();
    for chart in 0..4 {
        put.push(format!(
            r#"{{"chart":{{"id":"c{chart}","kind":"line","box":[0,0,100,100]}}}}"#
        ));
        for series in 0..if chart == 3 { 7 } else { 8 } {
            put.push(format!(
                r#"{{"series":{{"id":"s{chart}_{series}","in":"c{chart}","data":[{data}]}}}}"#
            ));
            patches.push(format!(
                r#"{{"target":"s{chart}_{series}","data":[{data}]}}"#
            ));
        }
    }
    let first = format!(r#"{{"step":{{"id":"first","put":[{}]}}}}"#, put.join(","));
    let updates = patches.join(",");
    drop(put);
    drop(patches);
    println!("staging_memory phase=baseline pid={}", std::process::id());
    std::thread::sleep(Duration::from_secs(3));

    let mut assembler = connected();
    describe(
        &mut assembler,
        (1..=4).map(|scene| descriptor(scene, 64, 64)).collect(),
    )
    .expect("four bounded descriptors");
    let mut encoded_bytes = 0_usize;
    let mut batch = None;
    for scene in 1..=4 {
        for revision in 1..=64 {
            let json = if revision == 1 {
                first.clone()
            } else {
                format!(r#"{{"step":{{"id":"update{revision}","set":[{updates}]}}}}"#)
            };
            let Record::Step(step) = parse_record(json.as_bytes()).expect("bounded step") else {
                panic!("step record")
            };
            let value = DataToShell::SceneSnapshotV1 {
                chunk: snapshot_chunk(
                    scene,
                    64,
                    u16::try_from(revision - 1).expect("ordinal"),
                    vec![SceneEvent::Step {
                        revision,
                        step: Arc::new(step),
                    }],
                    revision == 64,
                ),
            };
            let bytes = frame::encode(&value, Pipe::Data).expect("one whole event fits");
            encoded_bytes += bytes.len();
            let DataToShell::SceneSnapshotV1 { chunk } =
                frame::decode(&bytes[HEADER_LEN..]).expect("bounded wire decoding")
            else {
                panic!("snapshot frame")
            };
            let effect = assembler
                .chunk(
                    1,
                    Deferred::new(chunk, 0),
                    u32::try_from(bytes.len()).expect("frame size"),
                )
                .expect("bounded staging");
            assert_eq!(
                effect.credit,
                u32::try_from(bytes.len()).expect("frame size")
            );
            if let Some(sources) = effect.batch {
                assert!(batch.replace(sources).is_none());
            }
        }
    }
    let batch = batch.expect("all four complete");
    assert_eq!(batch.len(), 4);
    assert!(encoded_bytes < MAX_SCENE_STAGING_BYTES);
    println!(
        "staging_memory phase=retained pid={} scenes=4 steps=256 points_per_step=15500 encoded_bytes={encoded_bytes}",
        std::process::id()
    );
    std::thread::sleep(Duration::from_secs(3));
    for source in batch.iter() {
        assert_eq!(source.log.len(), 64);
        SceneState::replay(source.header.begin.clone(), &source.log)
            .expect("each scene is legal in isolation");
    }
    drop(batch);
    assembler.disconnected();
    println!("staging_memory phase=released pid={}", std::process::id());
    std::thread::sleep(Duration::from_secs(3));
}

fn header() -> SceneHeader {
    SceneHeader {
        begin: Begin {
            v: Version::V1,
            scene: WireId::try_from("private-scene".to_owned()).expect("id"),
            surface: Surface::Overlay,
            size: Some([
                kumochi_scene::Finite::try_from(13337.25).expect("coordinate"),
                kumochi_scene::Finite::try_from(100.0).expect("coordinate"),
            ]),
            title: None,
            alt: "private-alt".to_owned().try_into().expect("alt"),
            others: kumochi_scene::Others::Clear,
            display: None,
        },
        layout_version: 1,
    }
}

fn event(revision: u32) -> SceneEvent {
    SceneEvent::Step {
        revision,
        step: Arc::new(Step {
            id: WireId::try_from(format!("private-step-{revision}")).expect("id"),
            say: Some("private-narration".to_owned().try_into().expect("say")),
            put: Bounded::default(),
            set: Bounded::default(),
            remove: Bounded::default(),
            start: Start::default(),
        }),
    }
}

fn descriptor(scene: u64, revision: u32, entries: u16) -> SceneDescriptorV1 {
    SceneDescriptorV1::new(SceneId(scene), revision, revision, entries).expect("descriptor")
}

fn describe(
    assembler: &mut Assembler,
    descriptors: Vec<SceneDescriptorV1>,
) -> Result<Effect, Broken> {
    assembler.describe(1, &Bounded::new(descriptors).expect("four scenes"))
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture mirrors the snapshot's last wire bit"
)]
fn chunk(
    scene: u64,
    revision: u32,
    ordinal: u16,
    events: Vec<SceneEvent>,
    last: bool,
) -> Deferred<SceneSnapshotChunk> {
    Deferred::new(snapshot_chunk(scene, revision, ordinal, events, last), 0)
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture mirrors the snapshot's last wire bit"
)]
fn snapshot_chunk(
    scene: u64,
    revision: u32,
    ordinal: u16,
    events: Vec<SceneEvent>,
    last: bool,
) -> SceneSnapshotChunk {
    SceneSnapshotChunk {
        scene: SceneId(scene),
        revision,
        ordinal,
        header: (ordinal == 0).then(header),
        events: Bounded::new(events).expect("events"),
        last,
    }
}

fn connected() -> Assembler {
    let mut assembler = Assembler::default();
    assembler.connected(1);
    assembler
}

fn source(scene: u64, revision: u32, log: Vec<SceneEvent>) -> Source {
    Source {
        scene: SceneId(scene),
        header: header(),
        revision,
        committed: revision,
        log,
    }
}

#[test]
fn pre_membership_chunks_withhold_credit_and_return_one_full_batch() {
    let mut assembler = connected();
    assert!(assembler.pending());
    assert!(!assembler.described());
    assert_eq!(
        assembler.chunk(1, chunk(2, 1, 0, vec![event(1)], true), 100),
        Ok(Effect::default())
    );
    assert_eq!(
        assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 200),
        Ok(Effect::default())
    );
    assert_eq!(
        describe(
            &mut assembler,
            vec![descriptor(2, 1, 1), descriptor(1, 1, 1)]
        ),
        Ok(Effect {
            credit: 300,
            clear: Vec::new(),
            batch: Some(Deferred::new(
                vec![source(1, 1, vec![event(1)]), source(2, 1, vec![event(1)])],
                0
            )),
        })
    );
    assert!(assembler.described());
    assert!(!assembler.pending());
}

#[test]
fn stale_generations_and_offline_chunks_never_receive_credit() {
    let mut assembler = connected();
    assert_eq!(
        assembler.chunk(0, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect::default())
    );
    assert_eq!(
        assembler.describe(
            0,
            &Bounded::new(vec![descriptor(1, 1, 1)]).expect("bounded")
        ),
        Ok(Effect::default())
    );
    assembler.disconnected();
    assert_eq!(
        assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect::default())
    );
    assembler.connected(2);
    assert_eq!(
        assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect::default())
    );
    assert_eq!(
        assembler.describe(2, &Bounded::default()),
        Ok(Effect {
            batch: Some(Deferred::new(Vec::new(), 0)),
            ..Effect::default()
        })
    );
}

#[test]
fn descriptors_are_unique_and_locally_checked_even_if_constructed_directly() {
    assert_eq!(
        describe(
            &mut connected(),
            vec![descriptor(1, 1, 1), descriptor(1, 1, 1)]
        ),
        Err(Broken::Membership)
    );
    let mut bad = descriptor(1, 1, 1);
    bad.committed = 0;
    assert_eq!(describe(&mut connected(), vec![bad]), Err(Broken::Counts));
    bad = descriptor(1, 1, 1);
    bad.log_entries = 129;
    assert_eq!(describe(&mut connected(), vec![bad]), Err(Broken::Counts));
    let mut assembler = connected();
    assert_eq!(
        describe(&mut assembler, vec![descriptor(1, 1, 1)]),
        Ok(Effect::default())
    );
    assert_eq!(
        describe(&mut assembler, vec![descriptor(1, 1, 1)]),
        Err(Broken::Membership)
    );
}

#[test]
fn ordinals_are_consecutive_and_finished_scenes_cannot_append() {
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
    assert_eq!(
        assembler.chunk(1, chunk(1, 2, 1, vec![event(1)], false), 100),
        Err(Broken::Sequence)
    );
    for out_of_order in [
        chunk(1, 2, 0, vec![event(1)], false),
        chunk(1, 2, 2, vec![event(2)], true),
    ] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
        assembler
            .chunk(1, chunk(1, 2, 0, vec![event(1)], false), 100)
            .expect("first");
        assert_eq!(assembler.chunk(1, out_of_order, 100), Err(Broken::Sequence));
    }
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
    assert_eq!(
        assembler.chunk(1, chunk(1, 2, 0, vec![event(1)], false), 100),
        Ok(Effect {
            credit: 100,
            ..Effect::default()
        })
    );
    assert_eq!(
        assembler.chunk(1, chunk(1, 2, 1, vec![event(2)], true), 100),
        Ok(Effect {
            credit: 100,
            batch: Some(Deferred::new(
                vec![source(1, 2, vec![event(1), event(2)])],
                0
            )),
            ..Effect::default()
        })
    );
    assert_eq!(
        assembler.chunk(1, chunk(1, 2, 2, vec![event(2)], true), 100),
        Err(Broken::Sequence)
    );
}

#[test]
fn headers_only_appear_in_chunk_zero_and_revision_matches_membership() {
    let mut missing = chunk(1, 2, 0, vec![event(1)], false);
    missing.header = None;
    for first in [missing, chunk(1, 1, 0, vec![event(1)], true)] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
        assert_eq!(assembler.chunk(1, first, 100), Err(Broken::Sequence));
    }
    let mut extra = chunk(1, 2, 1, vec![event(2)], true);
    extra.header = Some(header());
    for second in [chunk(1, 3, 1, vec![event(2)], true), extra] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
        assembler
            .chunk(1, chunk(1, 2, 0, vec![event(1)], false), 100)
            .expect("first");
        assert_eq!(assembler.chunk(1, second, 100), Err(Broken::Sequence));
    }
}

#[test]
fn revision_boundaries_are_checked_across_chunks_and_cuts() {
    let future_cut = SceneEvent::Stop(StopCut {
        activated: 2,
        revealed: Bounded::default(),
    });
    for wrong in [event(3), future_cut] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 3, 3)]).expect("describe");
        assembler
            .chunk(1, chunk(1, 3, 0, vec![event(1)], false), 100)
            .expect("first");
        assert_eq!(
            assembler.chunk(1, chunk(1, 3, 1, vec![wrong], false), 100),
            Err(Broken::Sequence)
        );
    }
    let mut other = connected();
    describe(&mut other, vec![descriptor(1, 1, 2)]).expect("describe");
    assert_eq!(
        other.chunk(
            1,
            chunk(
                1,
                1,
                0,
                vec![SceneEvent::Stop(StopCut {
                    activated: 0,
                    revealed: Bounded::default()
                })],
                false
            ),
            100
        ),
        Err(Broken::Sequence)
    );
}

#[test]
fn last_flag_requires_the_exact_advertised_entry_and_revision_count() {
    let stop = SceneEvent::Stop(StopCut {
        activated: 1,
        revealed: Bounded::default(),
    });
    for (frame, error) in [
        (chunk(1, 2, 0, vec![event(1)], true), Broken::Counts),
        (
            chunk(1, 2, 0, vec![event(1), event(2)], false),
            Broken::Counts,
        ),
        (
            chunk(1, 2, 0, vec![event(1), event(2), event(2)], true),
            Broken::Counts,
        ),
        (chunk(1, 2, 0, vec![event(1), stop], true), Broken::Counts),
        (chunk(99, 1, 0, vec![event(1)], true), Broken::Membership),
    ] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("describe");
        assert_eq!(assembler.chunk(1, frame, 100), Err(error));
    }
}

#[test]
fn credit_window_and_frame_sizes_include_the_header() {
    let max = u32::try_from(MAX_FRAME_BYTES).expect("frame fits u32");
    for bytes in [max + 1, 4] {
        assert_eq!(
            connected().chunk(1, chunk(1, 1, 0, vec![event(1)], true), bytes),
            Err(Broken::Frame)
        );
    }
    for overflow in [false, true] {
        let mut assembler = connected();
        assert_eq!(
            assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), max),
            Ok(Effect::default())
        );
        assert_eq!(
            assembler.chunk(1, chunk(2, 1, 0, vec![event(1)], true), DATA_WINDOW - max),
            Ok(Effect::default())
        );
        if overflow {
            assert_eq!(
                assembler.chunk(1, chunk(3, 1, 0, vec![event(1)], true), 5),
                Err(Broken::Window)
            );
        }
        assert_eq!(
            assembler.clear(),
            Effect {
                credit: DATA_WINDOW,
                ..Effect::default()
            },
            "clearing refunds only the accepted bytes (overflow: {overflow})"
        );
    }
}

#[test]
fn staging_counters_enforce_scene_and_aggregate_caps_without_large_allocations() {
    for (scene_bytes, bytes) in [
        (MAX_SCENE_SNAPSHOT_BYTES - 99, MAX_SCENE_SNAPSHOT_BYTES - 99),
        (0, MAX_SCENE_STAGING_BYTES - 99),
    ] {
        let mut assembler = connected();
        describe(&mut assembler, vec![descriptor(1, 1, 1)]).expect("describe");
        assembler
            .scenes
            .get_mut(&SceneId(1))
            .expect("pending")
            .bytes = scene_bytes;
        assembler.bytes = bytes;
        assert_eq!(
            assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100),
            Err(Broken::Bytes)
        );
    }
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 1, 1)]).expect("describe");
    assembler
        .scenes
        .get_mut(&SceneId(1))
        .expect("pending")
        .bytes = MAX_SCENE_SNAPSHOT_BYTES - 100;
    assembler.bytes = MAX_SCENE_STAGING_BYTES - 100;
    assert_eq!(
        assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            batch: Some(Deferred::new(vec![source(1, 1, vec![event(1)])], 0)),
            ..Effect::default()
        })
    );
}

#[test]
fn clear_intent_survives_disconnect_and_cancels_future_membership() {
    let mut assembler = connected();
    assembler
        .chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100)
        .expect("hold");
    assert_eq!(
        assembler.clear(),
        Effect {
            credit: 100,
            ..Effect::default()
        }
    );
    assert!(!assembler.pending());
    assembler.disconnected();
    assembler.connected(2);
    assert_eq!(
        assembler.describe(
            2,
            &Bounded::new(vec![descriptor(1, 1, 1), descriptor(2, 1, 1)]).expect("scenes")
        ),
        Ok(Effect {
            clear: vec![SceneId(1), SceneId(2)],
            batch: Some(Deferred::new(Vec::new(), 0)),
            credit: 0
        })
    );
    assert_eq!(
        assembler.chunk(2, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            ..Effect::default()
        })
    );
}

#[test]
fn removing_one_member_releases_the_other_completed_sources() {
    let mut assembler = connected();
    describe(
        &mut assembler,
        vec![descriptor(1, 1, 1), descriptor(2, 1, 1)],
    )
    .expect("describe");
    assert_eq!(
        assembler.chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            ..Effect::default()
        })
    );
    assert_eq!(
        assembler.remove(&[SceneId(2)]),
        Effect {
            clear: vec![SceneId(2)],
            credit: 0,
            batch: Some(Deferred::new(vec![source(1, 1, vec![event(1)])], 0))
        }
    );
    assert_eq!(
        assembler.chunk(1, chunk(2, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            ..Effect::default()
        })
    );
    assert!(!assembler.pending());
}

#[test]
fn clear_while_receiving_invalidates_completed_and_incomplete_sources() {
    let mut assembler = connected();
    describe(
        &mut assembler,
        vec![descriptor(1, 1, 1), descriptor(2, 1, 1)],
    )
    .expect("describe");
    assembler
        .chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100)
        .expect("first complete");
    assert_eq!(
        assembler.clear(),
        Effect {
            clear: vec![SceneId(1), SceneId(2)],
            ..Effect::default()
        }
    );
    assert_eq!(
        assembler.chunk(1, chunk(2, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            ..Effect::default()
        })
    );
    assert!(!assembler.pending());
}

#[test]
fn removing_held_chunks_returns_only_their_credit_and_remembers_a_bounded_tombstone_set() {
    let mut assembler = connected();
    assembler
        .chunk(1, chunk(1, 1, 0, vec![event(1)], true), 100)
        .expect("held");
    assembler
        .chunk(1, chunk(2, 1, 0, vec![event(1)], true), 200)
        .expect("held");
    assert_eq!(
        assembler.remove(&[SceneId(1)]),
        Effect {
            clear: vec![SceneId(1)],
            credit: 100,
            batch: None
        }
    );
    assert_eq!(
        describe(
            &mut assembler,
            vec![descriptor(1, 1, 1), descriptor(2, 1, 1)]
        ),
        Ok(Effect {
            credit: 200,
            clear: vec![SceneId(1)],
            batch: Some(Deferred::new(vec![source(2, 1, vec![event(1)])], 0))
        })
    );
    for id in 3..=100 {
        assembler.remove(&[SceneId(id)]);
    }
    assembler.connected(2);
    assert_eq!(
        assembler.describe(
            2,
            &Bounded::new(
                [36, 37, 100]
                    .into_iter()
                    .map(|id| descriptor(id, 1, 1))
                    .collect()
            )
            .expect("three descriptors")
        ),
        Ok(Effect {
            clear: vec![SceneId(37), SceneId(100)],
            ..Effect::default()
        })
    );
    assert_eq!(
        assembler.chunk(2, chunk(36, 1, 0, vec![event(1)], true), 100),
        Ok(Effect {
            credit: 100,
            batch: Some(Deferred::new(vec![source(36, 1, vec![event(1)])], 0)),
            ..Effect::default()
        })
    );
    for (offset, ids) in (38..100).collect::<Vec<_>>().chunks(4).enumerate() {
        let generation = 3 + u64::try_from(offset).expect("a few reconnects");
        assembler.connected(generation);
        assert_eq!(
            assembler.describe(
                generation,
                &Bounded::new(ids.iter().map(|id| descriptor(*id, 1, 1)).collect())
                    .expect("bounded descriptors")
            ),
            Ok(Effect {
                clear: ids.iter().copied().map(SceneId).collect(),
                batch: Some(Deferred::new(Vec::new(), 0)),
                credit: 0,
            })
        );
    }
}

#[test]
fn four_scenes_install_together_once_and_debug_omits_all_content() {
    let mut assembler = connected();
    describe(
        &mut assembler,
        (1..=4).map(|id| descriptor(id, 1, 1)).collect(),
    )
    .expect("four");
    for id in 1..4 {
        assert_eq!(
            assembler.chunk(1, chunk(id, 1, 0, vec![event(1)], true), 100),
            Ok(Effect {
                credit: 100,
                ..Effect::default()
            })
        );
    }
    let effect = assembler
        .chunk(1, chunk(4, 1, 0, vec![event(1)], true), 100)
        .expect("batch");
    assert_eq!(
        effect,
        Effect {
            credit: 100,
            clear: Vec::new(),
            batch: Some(Deferred::new(
                (1..=4).map(|id| source(id, 1, vec![event(1)])).collect(),
                0
            ))
        }
    );
    let debug = format!("{effect:?} {assembler:?} {}", Broken::Bytes);
    assert!(!debug.contains("private"));
    assert!(!debug.contains("13337.25"));
    let private_bytes = b"private"
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    assert!(!debug.contains(&private_bytes));
    assert!(!assembler.pending());
}

#[test]
fn retained_counts_follow_cuts_across_chunks_without_semantic_replay() {
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 3, 4)]).expect("describe");
    let first = snapshot_chunk(1, 3, 0, vec![event(1), event(2)], false);
    let stop = SceneEvent::Stop(StopCut {
        activated: 1,
        revealed: Bounded::default(),
    });
    let last = snapshot_chunk(1, 3, 1, vec![stop.clone(), event(3)], true);
    assembler
        .chunk(1, Deferred::new(first.clone(), 0), 100)
        .expect("first chunk");
    assert_eq!(
        assembler.chunk(1, Deferred::new(last.clone(), 0), 100),
        Err(Broken::Counts)
    );

    let mut matching = connected();
    let mut named = descriptor(1, 3, 4);
    named.committed = 2;
    describe(&mut matching, vec![named]).expect("matching descriptor");
    matching
        .chunk(1, Deferred::new(first, 0), 100)
        .expect("first chunk");
    let mut expected = source(1, 3, vec![event(1), event(2), stop, event(3)]);
    expected.committed = 2;
    assert_eq!(
        matching.chunk(1, Deferred::new(last, 0), 100),
        Ok(Effect {
            credit: 100,
            clear: Vec::new(),
            batch: Some(Deferred::new(vec![expected], 0)),
        })
    );
}

#[test]
fn per_chunk_credit_flows_before_a_snapshot_larger_than_the_window_finishes() {
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 3, 3)]).expect("describe");
    let bytes = u32::try_from(MAX_FRAME_BYTES).expect("frame size fits u32");
    for ordinal in 0..2 {
        assert_eq!(
            assembler.chunk(
                1,
                chunk(1, 3, ordinal, vec![event(u32::from(ordinal) + 1)], false),
                bytes
            ),
            Ok(Effect {
                credit: bytes,
                ..Effect::default()
            })
        );
    }
    assert!(2 * bytes > DATA_WINDOW);
    assert!(assembler.pending());
    assert_eq!(
        assembler.chunk(1, chunk(1, 3, 2, vec![event(3)], true), bytes),
        Ok(Effect {
            credit: bytes,
            clear: Vec::new(),
            batch: Some(Deferred::new(
                vec![source(1, 3, vec![event(1), event(2), event(3)])],
                0
            )),
        })
    );
}

#[test]
fn actual_snapshot_frame_headers_count_toward_staging_limits() {
    use kumochi_protocol::{frame, message::DataToShell};

    let snapshot = snapshot_chunk(1, 1, 0, vec![event(1)], true);
    let frame = frame::encode(
        &DataToShell::SceneSnapshotV1 {
            chunk: snapshot.clone(),
        },
        Pipe::Data,
    )
    .expect("frame");
    let bytes = u32::try_from(frame.len()).expect("bounded frame");
    let payload_bytes = frame.len() - HEADER_LEN;
    let mut assembler = connected();
    describe(&mut assembler, vec![descriptor(1, 1, 1)]).expect("describe");
    assembler
        .scenes
        .get_mut(&SceneId(1))
        .expect("pending")
        .bytes = MAX_SCENE_SNAPSHOT_BYTES - payload_bytes;
    assembler.bytes = MAX_SCENE_SNAPSHOT_BYTES - payload_bytes;
    assert_eq!(
        assembler.chunk(1, Deferred::new(snapshot, 0), bytes),
        Err(Broken::Bytes)
    );
}

#[test]
fn rejected_staged_and_undelivered_snapshot_owners_retire_off_thread() {
    use crate::presenter::tests::{drained, hold_dropper};

    for case in 0..4 {
        let mut assembler = connected();
        let entry = event(1);
        let SceneEvent::Step { step, .. } = &entry else {
            panic!("one step");
        };
        let probe = Arc::clone(step);
        let release = hold_dropper();
        if case == 0 {
            let mut malformed = chunk(1, 1, 0, vec![entry], true);
            malformed.header = None;
            assert_eq!(assembler.chunk(1, malformed, 100), Err(Broken::Sequence));
        } else if case == 1 {
            describe(&mut assembler, vec![descriptor(1, 2, 2)]).expect("membership");
            assert_eq!(
                assembler.chunk(1, chunk(1, 2, 0, vec![entry], false), 100),
                Ok(Effect {
                    credit: 100,
                    ..Effect::default()
                })
            );
            assert_eq!(assembler.remove(&[SceneId(1)]).clear, [SceneId(1)]);
        } else {
            assert_eq!(
                assembler.chunk(1, chunk(1, 1, 0, vec![entry], true), 100),
                Ok(Effect::default())
            );
            if case == 3 {
                let effect = describe(&mut assembler, vec![descriptor(1, 1, 1)])
                    .expect("the held chunk finishes assembly");
                assert_eq!(effect.credit, 100);
                assert_eq!(effect.batch.as_ref().expect("one batch").len(), 1);
                drop(effect);
            }
        }
        drop(assembler);
        let deferred_owners = if case == 1 || case == 3 { 2 } else { 1 };
        assert_eq!(
            Arc::strong_count(&probe),
            1 + deferred_owners,
            "the chunk and any separately staged log or batch are still with the blocked dropper in case {case}"
        );
        drop(release);
        assert!(drained(&probe, 1));
    }
}
