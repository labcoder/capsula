use std::cell::RefCell;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ConnectionKind, DataToShell, Effort, ModelChoice, ModelInfo, ModelName, ModelSlug, SceneId,
    SettingsOp,
};
use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::data::DataQueue;
use super::{Brain, Link, Seen, window};
use crate::connections::model::sse::{Progress, Reader};
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, BuildKey, Input, Ready, StoreOp};
use crate::drawing::{Reply, Write};
use crate::engines::{CallId, EngineFailure, Finish, ToolName, TurnEvent};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

const WAIT: Duration = Duration::from_secs(60);
const FIRST: &str = r#"{"records":[{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":"diagram"}},{"step":{"id":"s1","put":[{"shape":{"id":"a","kind":"rect","box":[0,0,10,10]}}]}}"#;
const SECOND: &str =
    r#",{"step":{"id":"s2","put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10]}}]}}"#;
const THIRD: &str =
    r#",{"step":{"id":"s3","put":[{"shape":{"id":"c","kind":"rect","box":[40,0,10,10]}}]}}"#;

fn brain(scratch: &Scratch) -> Brain {
    let mut brain = test_support::brain(scratch, "60000 text never");
    let slug = ModelSlug::new("offline-model").expect("model slug");
    let pending = brain
        .config
        .stage(SettingsOp::SetDefaultModel(ModelChoice {
            model: slug.clone(),
            effort: Effort::Low,
            connection: ConnectionKind::ChatGpt,
        }))
        .expect("scratch model choice");
    pending.write().expect("scratch settings");
    brain.config.commit(pending);
    brain.offline(Input::Engines(
        kumochi_protocol::message::ConnectionKind::ChatGpt,
        Availability::Ready(Ready {
            connection: ConnectionTag(0),
            models: vec![ModelInfo {
                slug,
                display_name: ModelName::new("Offline model").expect("model name"),
            }],
            trusted_draw: false,
        }),
    ));
    brain
}

fn apply(brain: &mut Brain, link: &mut Link<'_, '_>, input: Input) {
    let effects = brain.handle(input).expect("current input");
    brain.apply(effects, Some(link)).expect("runtime effects");
}

async fn reply(brain: &mut Brain) -> Input {
    timeout(WAIT, brain.inputs.recv())
        .await
        .expect("store reply deadline")
        .expect("store thread remains open")
}

async fn start(brain: &mut Brain, link: &mut Link<'_, '_>) -> BuildKey {
    apply(
        brain,
        link,
        Input::Turn {
            turn: 1,
            thread: None,
            text: "draw a diagram".into(),
        },
    );
    loop {
        let input = reply(brain).await;
        let key = if let Input::ContextBuilt(built) = &input {
            let request = &built.result.as_ref().expect("real context build").request;
            assert!(request.tools.iter().any(|tool| tool.name == "draw"));
            Some(built.key)
        } else {
            None
        };
        apply(brain, link, input);
        if let Some(key) = key {
            assert_eq!(brain.coordinator.current_run(), Some(key.run));
            return key;
        }
    }
}

async fn step_landed(brain: &mut Brain, link: &mut Link<'_, '_>) -> SceneId {
    loop {
        let input = reply(brain).await;
        let scene = match &input {
            Input::DrawingWritten {
                reply: Ok(Reply::Step(scene)),
                ..
            } => Some(*scene),
            _ => None,
        };
        apply(brain, link, input);
        if let Some(scene) = scene {
            return scene;
        }
    }
}

async fn settled(brain: &mut Brain, link: &mut Link<'_, '_>) {
    timeout(WAIT, async {
        while !brain.coordinator.settled() {
            let input = reply(brain).await;
            apply(brain, link, input);
        }
    })
    .await
    .expect("closed run settlement deadline");
    assert_eq!(brain.coordinator.current_run(), None);
    assert_eq!(brain.coordinator.ending_runs(), 0);
    assert!(!brain.coordinator.drawing_in_flight());
    assert!(brain.round.is_none());
}

fn frames(queue: &RefCell<DataQueue>) -> Vec<DataToShell> {
    let mut queue = queue.borrow_mut();
    let mut messages = Vec::new();
    while let Some(bytes) = queue.pop(std::time::Instant::now()) {
        let message = frame::read::<DataToShell>(&mut bytes.as_slice(), Pipe::Data)
            .expect("data frame")
            .expect("whole data frame");
        queue.wrote();
        if matches!(
            message,
            DataToShell::SceneBegin { .. }
                | DataToShell::SceneStepCommit { .. }
                | DataToShell::SceneEnd { .. }
        ) {
            messages.push(message);
        }
    }
    messages
}

fn drawing_rows(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare(
        "SELECT kind || coalesce(':' || json_extract(body, '$.step.id'), '')
         FROM events WHERE kind LIKE 'scene_%' ORDER BY id",
    )
    .expect("drawing history")
    .query_map([], |row| row.get(0))
    .expect("drawing rows")
    .collect::<Result<_, _>>()
    .expect("durable drawing history")
}

fn delta(bytes: &str) -> TurnEvent {
    TurnEvent::ToolArgsDelta {
        call: CallId::new("provider-draw").expect("call id"),
        delta: bytes.to_owned(),
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real held FIFO covers stop and clear settlement together"
)]
async fn a_held_provider_step_survives_stop_but_clear_suppresses_its_late_frames() {
    for clear in [false, true] {
        let scratch = Scratch::new(if clear {
            "provider-stop-clear"
        } else {
            "provider-stop"
        });
        let mut brain = brain(&scratch);
        let mut observed = brain.observe();
        let (out, _controls) = mpsc::channel(64);
        let queue = RefCell::new(DataQueue::new(window()));
        let ready = Notify::new();
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        link.recovery = brain.describe(&mut link).expect("initial membership");
        let key = start(&mut brain, &mut link).await;
        apply(
            &mut brain,
            &mut link,
            Input::Engine {
                key,
                event: TurnEvent::ToolCallStarted {
                    call: CallId::new("provider-draw").expect("call id"),
                    name: ToolName::Draw,
                },
            },
        );
        apply(
            &mut brain,
            &mut link,
            Input::Engine {
                key,
                event: delta(FIRST),
            },
        );
        let scene = step_landed(&mut brain, &mut link).await;
        let first = frames(&queue);
        assert!(matches!(
            first.as_slice(),
            [DataToShell::SceneBegin { scene: began, .. },
             DataToShell::SceneStepCommit { scene: committed, index: 0, step, .. }]
                if *began == scene && *committed == scene && step.id.as_str() == "s1"
        ));

        let release = brain.hold_store(|op| {
            matches!(
                op,
                StoreOp::Drawing {
                    write: Write::Step { revision: 2, .. },
                    ..
                }
            )
        });
        apply(
            &mut brain,
            &mut link,
            Input::Engine {
                key,
                event: delta(SECOND),
            },
        );
        timeout(WAIT, async {
            while !matches!(
                observed.recv().await.expect("store observation"),
                Seen::StoreOpStarted(StoreOp::Drawing {
                    write: Write::Step { revision: 2, .. },
                    ..
                })
            ) {}
        })
        .await
        .expect("second step reaches actual FIFO gate");
        apply(
            &mut brain,
            &mut link,
            Input::Engine {
                key,
                event: delta(THIRD),
            },
        );
        apply(
            &mut brain,
            &mut link,
            Input::Stop {
                run: key.run,
                revealed: Vec::new(),
            },
        );
        if clear {
            apply(
                &mut brain,
                &mut link,
                Input::Clear {
                    scenes: vec![scene],
                },
            );
        }
        let reader = rusqlite::Connection::open(scratch.db()).expect("read held prefix");
        assert_eq!(drawing_rows(&reader), ["scene_begin", "scene_step:s1"]);
        drop(reader);
        assert_eq!(frames(&queue), []);
        release.send(()).expect("release actual second write");
        settled(&mut brain, &mut link).await;
        let late = frames(&queue);
        if clear {
            assert_eq!(late, []);
        } else {
            assert!(matches!(
                late.as_slice(),
                [DataToShell::SceneStepCommit { scene: committed, index: 1, step, .. },
                 DataToShell::SceneEnd { scene: ended, complete: false, .. }]
                    if *committed == scene && *ended == scene && step.id.as_str() == "s2"
            ));
        }
        for event in [
            delta(",{\"end\":{}}]}"),
            TurnEvent::ToolCallReady {
                call: CallId::new("provider-draw").expect("call id"),
                args: format!("{FIRST}{SECOND}{THIRD},{{\"end\":{{}}}}]}}"),
            },
            TurnEvent::Finished(Finish::Completed),
        ] {
            assert!(brain.handle(Input::Engine { key, event }).is_none());
        }
        assert_eq!(frames(&queue), []);
        assert!(brain.coordinator.settled());
        brain.close().await;
        let store = Store::open(&scratch.db()).expect("reopen after closed runtime");
        let expected = if clear {
            vec![
                "scene_begin",
                "scene_step:s1",
                "scene_step:s2",
                "scene_cleared",
            ]
        } else {
            vec!["scene_begin", "scene_step:s1", "scene_step:s2"]
        };
        assert_eq!(drawing_rows(store.conn_for_tests()), expected);
        let recovered = store.drawing_scenes().expect("recover durable prefix");
        assert_eq!(recovered.skipped, []);
        assert_eq!(
            recovered
                .scenes
                .iter()
                .map(|held| held.scene)
                .collect::<Vec<_>>(),
            if clear { vec![] } else { vec![scene] }
        );
        let outcome: (String, String) = store
            .conn_for_tests()
            .query_row(
                "SELECT state, reason FROM runs WHERE id=?1",
                [i64::try_from(key.run.0).expect("small run id")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("durable stop");
        assert_eq!(outcome, ("interrupted".into(), "stop".into()));
    }
}

#[tokio::test]
async fn a_mapper_changed_final_item_keeps_the_prefix_without_a_new_tool_result() {
    let scratch = Scratch::new("provider-mapper-changed-item");
    let mut brain = brain(&scratch);
    let (out, _controls) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("initial membership");
    let key = start(&mut brain, &mut link).await;
    let args = format!("{FIRST},{{\"end\":{{}}}}]}}");
    let mut reader = Reader::default();
    let mut mapper = crate::engines::responses::Round::default();
    for value in [
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"provider-draw","name":"draw"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":args}),
    ] {
        let bytes = format!("data: {value}\n\n");
        assert_eq!(
            reader.feed(bytes.as_bytes(), &mut |event| mapper.event(event)),
            Ok(Progress::More)
        );
        for event in mapper.take() {
            apply(&mut brain, &mut link, Input::Engine { key, event });
        }
    }
    let scene = step_landed(&mut brain, &mut link).await;
    let prefix = frames(&queue);
    assert!(matches!(
        prefix.as_slice(),
        [DataToShell::SceneBegin { .. },
         DataToShell::SceneStepCommit { scene: committed, index: 0, step, .. }]
            if *committed == scene && step.id.as_str() == "s1"
    ));
    let changed = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"item-1","call_id":"provider-draw","name":"draw","arguments":args.replace("s1", "changed")}});
    let bytes = format!("data: {changed}\n\n");
    assert_eq!(
        reader.feed(bytes.as_bytes(), &mut |event| mapper.event(event)),
        Ok(Progress::More)
    );
    assert_eq!(mapper.take(), []);
    let failed = mapper
        .end(Ok(Progress::More))
        .expect("mapper rejects changed final item");
    assert_eq!(
        failed,
        TurnEvent::Finished(Finish::Failed(EngineFailure::Stream(
            crate::connections::model::error::Failure::Protocol
        )))
    );
    apply(&mut brain, &mut link, Input::Engine { key, event: failed });
    settled(&mut brain, &mut link).await;
    assert!(matches!(
        frames(&queue).as_slice(),
        [DataToShell::SceneEnd { scene: ended, complete: false, .. }] if *ended == scene
    ));
    assert!(
        brain
            .handle(Input::Engine {
                key,
                event: TurnEvent::Finished(Finish::Completed),
            })
            .is_none()
    );
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen after failed round");
    assert_eq!(
        drawing_rows(store.conn_for_tests()),
        ["scene_begin", "scene_step:s1"]
    );
    let results: u32 = store
        .conn_for_tests()
        .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
        .expect("no unsubmitted result transaction");
    assert_eq!(results, 0);
    let outcome: (String, String) = store
        .conn_for_tests()
        .query_row(
            "SELECT state, reason FROM runs WHERE id=?1",
            [i64::try_from(key.run.0).expect("small run id")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("durable provider failure");
    assert_eq!(outcome, ("failed".into(), "engine".into()));
    assert_eq!(
        store
            .drawing_scenes()
            .expect("retained failed prefix")
            .scenes
            .iter()
            .map(|held| held.scene)
            .collect::<Vec<_>>(),
        [scene]
    );
}

#[tokio::test]
async fn a_provider_step_is_laid_out_on_the_preparation_thread_before_its_write() {
    let scratch = Scratch::new("provider-prepare");
    let mut brain = brain(&scratch);
    brain.coordinator.defer_drawing_preparation();
    let (out, _controls) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("initial membership");
    let key = start(&mut brain, &mut link).await;
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::ToolCallStarted {
                call: CallId::new("provider-draw").expect("call id"),
                name: ToolName::Draw,
            },
        },
    );
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: delta(FIRST),
        },
    );
    let mut laid_out = false;
    let scene = loop {
        let input = reply(&mut brain).await;
        laid_out |= matches!(
            input,
            Input::Drawing(crate::drawing::Input::Prepared {
                outcome: crate::drawing::Prepared::Layout { .. },
                ..
            })
        );
        let landed = match &input {
            Input::DrawingWritten {
                reply: Ok(Reply::Step(scene)),
                ..
            } => Some(*scene),
            _ => None,
        };
        apply(&mut brain, &mut link, input);
        if let Some(scene) = landed {
            break scene;
        }
    };
    assert!(
        laid_out,
        "the layout came back from its thread before the write"
    );
    assert!(matches!(
        frames(&queue).as_slice(),
        [DataToShell::SceneBegin { scene: began, .. },
         DataToShell::SceneStepCommit { scene: committed, index: 0, .. }]
            if *began == scene && *committed == scene
    ));
}
