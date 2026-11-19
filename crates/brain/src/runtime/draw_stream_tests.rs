use std::cell::RefCell;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ConnectionKind, ControlToShell, DataToShell, Effort, ModelChoice, ModelInfo, ModelName,
    ModelSlug, OutputGeneration, PageEvent, PageEventKind, PageOf, SceneId, SettingsOp, ThreadId,
};
use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::data::DataQueue;
use super::{Brain, Link, Seen, window};
use crate::connections::model::sse::{Progress, Reader};
use crate::context::{ConnectionTag, InputItem, Role};
use crate::coordinator::{Availability, BuildKey, Input, Ready, StoreOp};
use crate::drawing::{Reply, Write};
use crate::engines::{CallId, EngineFailure, Finish, Opaque, ToolName, TurnEvent};
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
                supported_efforts: kumochi_protocol::message::EffortSet::new([
                    kumochi_protocol::message::Effort::Low,
                ]),
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
    all_frames(queue)
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                DataToShell::SceneBegin { .. }
                    | DataToShell::SceneStepCommit { .. }
                    | DataToShell::SceneEnd { .. }
            )
        })
        .collect()
}

fn all_frames(queue: &RefCell<DataQueue>) -> Vec<DataToShell> {
    let mut queue = queue.borrow_mut();
    let mut messages = Vec::new();
    while let Some(bytes) = queue.pop(std::time::Instant::now()) {
        let message = frame::read::<DataToShell>(&mut bytes.as_slice(), Pipe::Data)
            .expect("data frame")
            .expect("whole data frame");
        queue.wrote();
        messages.push(message);
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

async fn staged_say(brain: &mut Brain, narration: [&str; 2], expected: &[&str], segments: &[u32]) {
    let (out, mut controls) = mpsc::channel(64);
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let queue = RefCell::new(DataQueue::new(window()));
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("initial membership");
    let key = start(brain, &mut link).await;
    let thread = loop {
        let bytes = controls.try_recv().expect("the staged run is named");
        if let Some(ControlToShell::RunState { run, thread, .. }) =
            frame::read(&mut bytes.as_slice(), Pipe::Control).expect("control frame")
            && run == key.run
        {
            break thread;
        }
    };
    apply(
        brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::ToolCallStarted {
                call: CallId::new("provider-draw").expect("call id"),
                name: ToolName::Draw,
            },
        },
    );
    let mut prefix = FIRST.split_once(",{\"step\"").expect("first record").0;
    for (index, say) in narration.into_iter().chain([" \t "]).enumerate() {
        let record = json!({"step":{
            "id": format!("s{index}"), "say": say,
            "put": [{"shape":{"id":format!("r{index}"),"kind":"rect","box":[index * 20,0,10,10]}}]
        }});
        let args = format!("{prefix},{record}");
        prefix = "";
        apply(
            brain,
            &mut link,
            Input::Engine {
                key,
                event: delta(&args),
            },
        );
        step_landed(brain, &mut link).await;
    }
    let messages = all_frames(&queue);
    let paragraphs = expected
        .iter()
        .map(|text| format!("\n\n{text}\n\n"))
        .collect::<Vec<_>>();
    let live: Vec<_> = messages
        .iter()
        .filter_map(|frame| match frame {
            DataToShell::AssistantText { run, delta } if *run == key.run => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let assigned: Vec<_> = messages
        .iter()
        .filter_map(|frame| match frame {
            DataToShell::SegmentQueued {
                run,
                step: Some(step),
                ..
            } if *run == key.run => Some(step.index),
            _ => None,
        })
        .collect();
    assert_eq!(assigned, segments);
    assert_eq!(live, paragraphs, "Chat lines from the live steps");

    brain.offline(Input::ShellLost);
    settled(brain, &mut link).await;
    let queue = RefCell::new(DataQueue::new(window()));
    brain.coordinator.begin_session();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("reconnect membership");
    brain.queue_recovery(&mut link).expect("snapshot");
    let snapshots = all_frames(&queue);
    assert!(matches!(
        snapshots.as_slice(),
        [DataToShell::SceneSnapshotV1 { .. }]
    ));
    let lines = read_chat_page(brain, &mut link, thread, 1).await;
    assert_eq!(
        lines
            .iter()
            .map(|event| event.text.as_str())
            .collect::<Vec<_>>(),
        paragraphs,
    );
    assert!(lines.iter().all(|event| event.run == Some(key.run)));
    assert_eq!(read_chat_page(brain, &mut link, thread, 2).await, lines);
}

async fn read_chat_page(
    brain: &mut Brain,
    link: &mut Link<'_, '_>,
    thread: ThreadId,
    request: u32,
) -> Vec<PageEvent> {
    let key = link
        .queue
        .borrow_mut()
        .admit_page(1, request)
        .expect("admit");
    apply(
        brain,
        link,
        Input::ReadPage {
            key,
            page: PageOf::Events {
                thread,
                before: None,
            },
        },
    );
    let input = reply(brain).await;
    apply(brain, link, input);
    let mut page = all_frames(link.queue);
    assert_eq!(page.len(), 1);
    let DataToShell::Page {
        request: got,
        thread: of,
        events,
        before: None,
    } = page.pop().expect("page")
    else {
        panic!("expected one complete history page");
    };
    assert_eq!((got, of), (request, thread));
    events
        .into_iter()
        .rev()
        .filter(|event| event.kind == PageEventKind::AssistantText)
        .collect()
}

fn submit_unspoken_step(brain: &mut Brain, link: &mut Link<'_, '_>, key: BuildKey) -> String {
    apply(
        brain,
        link,
        Input::Engine {
            key,
            event: TurnEvent::ToolCallStarted {
                call: CallId::new("provider-draw").expect("call id"),
                name: ToolName::Draw,
            },
        },
    );
    let record = FIRST.replace(
        "\"id\":\"s1\",",
        "\"id\":\"s1\",\"say\":\"One Chat line.\",",
    );
    apply(
        brain,
        link,
        Input::Engine {
            key,
            event: delta(&record),
        },
    );
    record
}

async fn first_unspoken_write(brain: &mut Brain, link: &mut Link<'_, '_>) -> Input {
    let key = start(brain, link).await;
    submit_unspoken_step(brain, link, key);
    loop {
        let input = reply(brain).await;
        if matches!(
            input,
            Input::DrawingWritten {
                reply: Ok(Reply::Step(_)) | Err(()),
                ..
            }
        ) {
            return input;
        }
        apply(brain, link, input);
    }
}

#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "the offline child is spawned under platform::spawn_lock"
)]
async fn unspoken_line_and_step_survive_a_crash_before_the_write_receipt_is_applied() {
    const CHILD: &str = "KUMOCHI_TEST_UNSPOKEN_CRASH_DIR";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        let scratch = Scratch::under(&root, "child");
        let mut brain = brain(&scratch);
        let (out, _controls) = mpsc::channel(64);
        let ready = Notify::new();
        let mut tap = Tap::from_env();
        let queue = RefCell::new(DataQueue::new(window()));
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        link.recovery = brain.describe(&mut link).expect("initial membership");
        assert!(matches!(
            first_unspoken_write(&mut brain, &mut link).await,
            Input::DrawingWritten {
                reply: Ok(Reply::Step(_)),
                ..
            }
        ));
        std::fs::write(
            root.join("writing-marker"),
            scratch.db().as_os_str().to_str().expect("test path"),
        )
        .expect("signal the committed step");
        std::fs::rename(root.join("writing-marker"), root.join("committed"))
            .expect("publish marker");
        tokio::time::sleep(WAIT * 2).await;
        return;
    }
    let scratch = Scratch::new("unspoken-crash-parent");
    std::fs::create_dir_all(scratch.dir()).expect("private parent scratch");
    let mut child = {
        let _spawning = crate::platform::spawn_lock();
        std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "runtime::draw_stream_tests::unspoken_line_and_step_survive_a_crash_before_the_write_receipt_is_applied"])
            .env(CHILD, scratch.dir())
            .spawn()
            .expect("child fixture")
    };
    let marker = scratch.dir().join("committed");
    let deadline = std::time::Instant::now() + WAIT;
    let ready = loop {
        if marker.exists() {
            break true;
        }
        if child.try_wait().expect("child status").is_some()
            || std::time::Instant::now() >= deadline
        {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if child.try_wait().expect("child status").is_none() {
        child.kill().expect("kill the owned fixture");
    }
    assert!(!child.wait().expect("reap the owned fixture").success());
    assert!(ready, "the child reached the commit before it was killed");
    let db = std::fs::read_to_string(marker).expect("committed store path");
    let mut store = Store::open(std::path::Path::new(&db)).expect("reopen after crash");
    store.recover().expect("crash recovery");
    assert_eq!(
        drawing_rows(store.conn_for_tests()),
        ["scene_begin", "scene_step:s1"]
    );
    let DataToShell::Page { events, .. } = store
        .pages()
        .read_page(
            1,
            PageOf::Events {
                thread: ThreadId(2),
                before: None,
            },
        )
        .expect("recovered Chat page")
    else {
        panic!("expected a page")
    };
    assert_eq!(
        events
            .into_iter()
            .filter(|event| event.kind == PageEventKind::AssistantText)
            .map(|event| event.text)
            .collect::<Vec<_>>(),
        ["\n\nOne Chat line.\n\n"]
    );
}

#[tokio::test]
async fn unspoken_line_write_failure_rolls_back_the_drawing_step() {
    let scratch = Scratch::new("unspoken-atomic-failure");
    let mut brain = brain(&scratch);
    let conn = rusqlite::Connection::open(scratch.db()).expect("fixture database");
    conn.execute_batch(
        "CREATE TRIGGER fail_chat BEFORE INSERT ON events
         WHEN new.kind = 'assistant_text' AND trim(new.text, char(10)) = 'One Chat line.'
         BEGIN SELECT RAISE(ABORT, 'fixture narration failure'); END;",
    )
    .expect("install fault");
    let (out, _controls) = mpsc::channel(64);
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let queue = RefCell::new(DataQueue::new(window()));
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("initial membership");
    let input = first_unspoken_write(&mut brain, &mut link).await;
    let failed = matches!(input, Input::DrawingWritten { reply: Err(()), .. });
    apply(&mut brain, &mut link, input);
    brain.offline(Input::ShellLost);
    brain.close().await;
    assert!(
        failed,
        "the step itself must report the failed narration write"
    );
    assert_eq!(drawing_rows(&conn).len(), 0);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM events WHERE kind = 'assistant_text'",
            [],
            |row| row.get::<_, u32>(0)
        )
        .expect("count"),
        0
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real write race covers live Chat, history and the next request"
)]
async fn unspoken_line_preserves_reply_order_while_the_drawing_write_is_held() {
    let scratch = Scratch::new("unspoken-order");
    let mut brain = brain(&scratch);
    let mut observed = brain.observe();
    let (out, _controls) = mpsc::channel(64);
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let queue = RefCell::new(DataQueue::new(window()));
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    link.recovery = brain.describe(&mut link).expect("initial membership");
    let key = start(&mut brain, &mut link).await;
    let prefix = "Before the drawing.";
    let suffix = "After the drawing.";
    let expected = "Before the drawing.\n\nOne Chat line.\n\nAfter the drawing.";
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::TextDelta(prefix.into()),
        },
    );
    let release = brain.hold_store(|op| {
        matches!(
            op,
            StoreOp::Drawing {
                write: Write::Step { revision: 1, .. },
                ..
            }
        )
    });
    let record = submit_unspoken_step(&mut brain, &mut link, key);
    let input = reply(&mut brain).await;
    assert!(matches!(
        input,
        Input::DrawingWritten {
            reply: Ok(Reply::Lookup(None)),
            ..
        }
    ));
    apply(&mut brain, &mut link, input);
    let serial = timeout(WAIT, async {
        loop {
            if let Seen::StoreOpStarted(StoreOp::Drawing {
                serial,
                write: Write::Step { revision: 1, .. },
            }) = observed.recv().await.expect("store observation")
            {
                break serial;
            }
        }
    })
    .await
    .expect("first step at the real write gate");
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::TextDelta(suffix.into()),
        },
    );
    let early = all_frames(&queue);
    release.send(()).expect("release the step write");
    let scene = step_landed(&mut brain, &mut link).await;
    assert!(
        brain
            .handle(Input::DrawingWritten {
                serial,
                reply: Ok(Reply::Step(scene))
            })
            .is_none()
    );
    apply(
        &mut brain,
        &mut link,
        Input::Drawing(crate::drawing::Input::StepReady {
            run: key.run,
            scene,
            index: 0,
        }),
    );
    let end = r#",{"end":{}}]}"#;
    let args = format!("{record}{end}");
    let provider_message = |text| json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]});
    let call = CallId::new("provider-draw").expect("call id");
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: delta(end),
        },
    );
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::ToolCallReady {
                call,
                args: args.clone(),
            },
        },
    );
    for (index, item) in [
        provider_message(prefix),
        json!({"type":"function_call","id":"item-draw","call_id":"provider-draw","name":"draw","arguments":args}),
        json!({"type":"message","role":"assistant","content":[
            {"type":"output_text","text":suffix},
            {"type":"refusal","refusal":"fixture non-text output"}
        ]}),
    ].into_iter().enumerate() {
        let item = Opaque::new(serde_json::to_vec(&item).expect("fixture JSON")).expect("fixture item");
        let index = u32::try_from(index).expect("fixture index");
        apply(&mut brain, &mut link, Input::Engine { key, event: TurnEvent::OutputItem { index, item } });
    }
    apply(
        &mut brain,
        &mut link,
        Input::Engine {
            key,
            event: TurnEvent::Finished(Finish::Completed),
        },
    );
    let next = timeout(WAIT, async {
        loop {
            let input = reply(&mut brain).await;
            if let Input::ContextBuilt(built) = input {
                break built.result.expect("next model context").request.input;
            }
            apply(&mut brain, &mut link, input);
        }
    })
    .await
    .expect("next request");
    let live = early
        .into_iter()
        .chain(all_frames(&queue))
        .filter_map(|message| match message {
            DataToShell::AssistantText { run, delta } if run == key.run => {
                Some(delta.into_string())
            }
            _ => None,
        })
        .collect::<String>();
    let history = read_chat_page(&mut brain, &mut link, ThreadId(2), 1).await;
    let stored: String = history.iter().map(|event| event.text.as_str()).collect();
    let mut replay = String::new();
    let mut native_calls = 0;
    let mut native_results = 0;
    let mut native_refusals = 0;
    for item in next {
        match item {
            InputItem::Message {
                role: Role::Assistant,
                text,
            } => replay.push_str(&text),
            InputItem::Native(item) => {
                let value: serde_json::Value =
                    serde_json::from_slice(item.bytes()).expect("native input");
                if value["type"] == "message" && value["role"] == "assistant" {
                    for part in value["content"].as_array().expect("message parts") {
                        if part["type"] == "output_text" {
                            replay.push_str(part["text"].as_str().expect("provider text"));
                        } else if part["refusal"] == "fixture non-text output" {
                            native_refusals += 1;
                        }
                    }
                } else if value["call_id"] == "provider-draw" {
                    native_calls += 1;
                }
            }
            InputItem::FunctionOutput { call, .. } if call.as_str() == "provider-draw" => {
                native_results += 1;
            }
            _ => {}
        }
    }
    brain.offline(Input::ShellLost);
    brain.close().await;
    assert_eq!(
        (live.as_str(), stored.as_str(), replay.as_str()),
        (expected, expected, expected)
    );
    assert_eq!((native_calls, native_results), (1, 1));
    assert_eq!(native_refusals, 1);
}

#[tokio::test]
async fn unspoken_text_drawing_lines_appear_once_in_chat_and_history() {
    let scratch = Scratch::new("drawing-text-say");
    let mut brain = brain(&scratch);
    let lines = ["Name the input.", "Mark the output."];
    staged_say(&mut brain, lines, &lines, &[]).await;
    brain.close().await;
}

#[tokio::test]
async fn unspoken_lines_in_a_spoken_drawing_exclude_assigned_and_blank_say() {
    let scratch = Scratch::new("drawing-spoken-say");
    let mut brain = brain(&scratch);
    brain.offline(Input::SpokenReplies { loaded: true });
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
    let unassigned = "界".repeat(350);
    staged_say(
        &mut brain,
        ["Name the input.", &unassigned],
        &[&unassigned],
        &[0],
    )
    .await;
    brain.close().await;
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
