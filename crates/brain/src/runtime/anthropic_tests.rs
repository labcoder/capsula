use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kumochi_protocol::message::{Action, ErrorKey, KeyView, ScreenChoice};
use serde_json::{Value, json};

use super::*;
use crate::connections::account::Worker;
use crate::connections::anthropic::route::Transport;
use crate::connections::anthropic::tls::{Script, Scripted};
use crate::connections::clock::FixedClock;
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::runtime::account::Accounts;

const KEY: &str = "sk-ant-api-synthetic-runtime";
const SIG_1: &str = "SYNTHETIC-SIGNATURE-ONE";
const SIG_2: &str = "SYNTHETIC-SIGNATURE-TWO";
const CALL: &str = "toolu_reused";
const MODEL: &str = "claude-sonnet-5-5";

#[path = "anthropic_tests/pictured_stop.rs"]
mod pictured_stop;

fn http(media: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn sse(events: &[Value]) -> Vec<u8> {
    use std::fmt::Write as _;

    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").expect("writes to String");
    }
    http("text/event-stream", &body)
}

fn calls(signature: &str, query: &str) -> Vec<u8> {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"synthetic thinking"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":CALL,"name":"search_history","input":{"query":query}}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ])
}

fn answers(text: &str) -> Vec<u8> {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ])
}

fn replay_brain(scratch: &Scratch, store: FakeStore, peer: Scripted, count: u32) -> Brain {
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(peer, 1, count));
    let mut brain = test_support::brain_with(scratch, "", accounts);
    brain.compact_at = u64::MAX;
    brain
}

fn seed_key(scratch: &Scratch) -> FakeStore {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    Vault::with_service(store.clone(), "offline-key-runtime", &lock)
        .expect("fake vault")
        .save_anthropic_key(KEY.as_bytes())
        .expect("synthetic key");
    std::fs::write(
        scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
        format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
    ).expect("settings");
    store
}

async fn play(
    scratch: &Scratch,
    store: FakeStore,
    turns: &[(u32, u64, &str)],
    responses: Vec<Vec<u8>>,
) -> Vec<String> {
    let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
    let count = u32::try_from(responses.len()).expect("small");
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: std::iter::once(http("application/json", &catalog.to_string()))
            .chain(responses)
            .collect(),
        ..Script::default()
    })));
    let mut brain = replay_brain(scratch, store, peer.clone(), count);
    let ((control, data), mut shell) = duplex();
    let recording = scratch
        .db()
        .with_file_name(format!("offline-key-{}.tap", turns[0].1));
    let file = std::fs::File::create_new(&recording).expect("test tap");
    let mut tap = Tap::to_file(file);
    let client = async {
        loop {
            if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
            {
                break;
            }
        }
        let DataToShell::Models { connection, models } = shell.data().await else {
            panic!("the verified catalog did not arrive");
        };
        assert_eq!(
            connection,
            kumochi_protocol::message::ConnectionKind::AnthropicKey
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug.as_str(), MODEL);
        for &(turn, id, text) in turns {
            let run = RunId(id);
            let message = DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                thread: Some(ThreadId(1)),
                text: text.into(),
            };
            let frame = frame::encode(&message, Pipe::Data).expect("encodes");
            shell.data_out.write_all(&frame).await.expect("submitted");
            loop {
                let (actual, phase) = shell.state().await;
                assert_eq!(actual, run);
                match phase {
                    Phase::Done => break,
                    Phase::Thinking | Phase::Answering => {}
                    _ => panic!("unexpected phase {phase:?}"),
                }
            }
            assert!(shell.read_to_end(run).await > 0);
        }
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("routed runtime finishes");
    assert_eq!(ended, Ended::Lost);
    assert!(brain.key_round.is_none());
    let last = RunId(turns[turns.len() - 1].1);
    assert_eq!(
        brain.key_allowance.spent(last),
        Some(1),
        "the last run's one send charged its allowance"
    );
    for marker in [KEY, SIG_1, SIG_2] {
        assert!(!format!("{brain:?}").contains(marker));
    }
    brain.close().await;
    drop(tap);
    let bytes = std::fs::read(recording).expect("tap");
    for marker in [KEY, SIG_1, SIG_2] {
        assert!(
            !bytes
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        );
    }
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, usize::try_from(count).expect("small") + 1);
    assert!(script.requests[0].starts_with(b"GET /v1/models?limit=65 "));
    script.requests[1..]
        .iter()
        .map(|request| {
            assert!(request.starts_with(b"POST /v1/messages "));
            let at = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .expect("head");
            std::str::from_utf8(&request[at + 4..])
                .expect("synthetic body")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn routed_key_rounds_continue_once_and_replay_only_under_the_verified_tag() {
    let scratch = Scratch::new("offline-key-round-owner");
    let store = seed_key(&scratch);
    let sent = Box::pin(play(
        &scratch,
        store.clone(),
        &[(1, 1, "start"), (2, 2, "again")],
        vec![
            calls(SIG_1, "synthetic-first"),
            calls(SIG_2, "synthetic-second"),
            answers("Done."),
            answers("Again."),
        ],
    ))
    .await;
    assert!(!sent[0].contains(SIG_1));
    assert!(sent[1].contains(SIG_1));
    assert!(!sent[1].contains(SIG_2));
    assert!(sent[2].contains(SIG_1) && sent[2].contains(SIG_2));
    assert!(!sent[3].contains(SIG_1) && !sent[3].contains(SIG_2));
    for body in [&sent[2], &sent[3]] {
        assert_eq!(body.matches("\"id\":\"toolu_reused\"").count(), 2);
        assert_eq!(body.matches("\"tool_use_id\":\"toolu_reused\"").count(), 2);
    }
    let db = Store::open(&scratch.db()).expect("persistent store");
    let stored: String = db
        .conn_for_tests()
        .query_row(
            "SELECT group_concat(COALESCE(text, '') || COALESCE(body, '')) FROM events",
            [],
            |row| row.get(0),
        )
        .expect("stored events");
    assert!(stored.contains(CALL) && stored.contains("Done."));
    for marker in [KEY, SIG_1, SIG_2] {
        assert!(!stored.contains(marker));
    }
    let calls: i64 = db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(DISTINCT call) FROM tool_calls WHERE run = 1 AND tool = 'search_history'",
            [],
            |row| row.get(0),
        )
        .expect("committed calls");
    assert_eq!(calls, 2);
    drop(db);

    let restarted = Box::pin(play(
        &scratch,
        store,
        &[(1, 3, "after restart")],
        vec![answers("Restarted.")],
    ))
    .await;
    assert!(restarted[0].contains("Done.") && restarted[0].contains("Again."));
    for omitted in [CALL, SIG_1, SIG_2] {
        assert!(!restarted[0].contains(omitted));
    }
}

const KEY_PICTURE: &[u8] = b"\xff\xd8\xffoffline-key-picture-sentinel-91\xff\xd9";
const PICTURE_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PictureEnd {
    Success,
    ProviderError,
    Stop,
    Quit,
}

async fn submit_pictured_hold(brain: &mut Brain) {
    use crate::context::{Origin, Screen};
    use crate::coordinator::{Effect, HoldKey, PictureRoutes};
    use kumochi_protocol::message::GestureId;

    brain
        .coordinator
        .set_pictures(PictureRoutes::new(true, false));
    timeout(PICTURE_WAIT, async {
        while brain.account.connections.anthropic() != KeyView::Ready {
            let event = brain.account.next().await;
            brain.account_input(event, None).expect("account event");
        }
    })
    .await
    .expect("the offline catalog is ready");
    let read = brain
        .coordinator
        .prepare_hold(
            HoldKey {
                epoch: 0,
                gesture: GestureId(1),
            },
            Some(ThreadId(1)),
            1,
            brain.snapshot(Origin::Kumo),
            Arc::default(),
        )
        .expect("admit the held turn");
    brain
        .apply(vec![Effect::Store(read)], None)
        .expect("read the hold");
    let Some(Input::HoldPrepared {
        result: Ok(hold), ..
    }) = timeout(PICTURE_WAIT, brain.inputs.recv())
        .await
        .expect("held snapshot")
    else {
        panic!("the store did not prepare the held turn");
    };
    let image = crate::screen::ScreenContext::fixture(KEY_PICTURE);
    let geometry = image.geometry();
    let effects = brain
        .coordinator
        .submit_hold(
            hold,
            "look at this".into(),
            Some(Screen::Picture(image.into_jpeg(), None, geometry)),
        )
        .expect("submit the pictured hold");
    brain.apply(effects, None).expect("start the hold");
}

fn assert_key_picture_not_retained(scratch: &Scratch, captured: &test_support::Captured) {
    assert!(
        !test_support::fixture_has_picture_marker(scratch.dir(), KEY_PICTURE),
        "the key route retained the picture in owned scratch"
    );
    assert!(
        !test_support::has_picture_marker(captured.text().as_bytes(), KEY_PICTURE),
        "the key route logged the picture"
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "one held request from send to durable history"
)]
async fn pictured_key_replay(ending: PictureEnd) {
    use crate::coordinator::StoreOp;
    use crate::runtime::{Quit, Seen};

    let captured = test_support::Captured::default();
    let _logs = captured.install();
    let scratch = Scratch::new("offline-key-picture-lifetime");
    let store = seed_key(&scratch);
    let catalog = json!({
        "data": [{"type": "model", "id": MODEL, "display_name": "Synthetic model",
                  "capabilities": {"image_input": {"supported": true}}}],
        "has_more": false
    });
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: [http("application/json", &catalog.to_string())].into(),
        ..Script::default()
    })));
    let mut brain = replay_brain(&scratch, store.clone(), peer.clone(), 1);
    let mut observed = brain.observe();
    submit_pictured_hold(&mut brain).await;
    let (reader, mut reply) = tokio::io::duplex(4096);
    peer.0.lock().expect("peer").reader = Some(Box::new(reader));
    let upload = async {
        let encoded = STANDARD.encode(KEY_PICTURE);
        loop {
            {
                let script = peer.0.lock().expect("peer");
                if let Some(post) = script.requests.get(1).filter(|post| post.ends_with(b"}")) {
                    let post = std::str::from_utf8(post).expect("UTF-8 Messages request");
                    let (_, body) = post.split_once("\r\n\r\n").expect("HTTP headers");
                    assert!(post.starts_with("POST /v1/messages "));
                    assert_eq!(body.matches("\"type\":\"image\"").count(), 1);
                    assert_eq!(body.matches(encoded.as_str()).count(), 1);
                    assert_eq!(
                        script.drops, 1,
                        "only the catalog is closed before termination"
                    );
                    return post.len();
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let sent = tokio::select! {
        () = brain.idle() => unreachable!("idle does not return"),
        sent = timeout(PICTURE_WAIT, upload) => sent.expect("pictured upload reached the peer"),
    };

    let closed = async {
        loop {
            if peer.0.lock().expect("peer").drops == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    if ending == PictureEnd::Quit {
        assert_eq!(
            brain.quit(tokio::time::Instant::now() + PATIENCE).await,
            Quit::Complete
        );
        timeout(PICTURE_WAIT, closed)
            .await
            .expect("Quit closed the pictured POST");
    } else {
        match ending {
            PictureEnd::Success => reply.write_all(&answers("Seen.")).await.expect("answer"),
            PictureEnd::ProviderError => reply
                .write_all(&sse(&[
                    json!({"type": "error", "error": {"type": "api_error"}}),
                ]))
                .await
                .expect("provider error"),
            PictureEnd::Stop => brain.offline(Input::Stop {
                run: RunId(1),
                revealed: Vec::new(),
            }),
            PictureEnd::Quit => unreachable!(),
        }
        let ended = async {
            while !matches!(
                observed.recv().await.expect("runtime observation"),
                Seen::StoreOpQueued(StoreOp::EndRun { run: RunId(1), .. })
            ) {}
        };
        tokio::select! {
            () = brain.idle() => unreachable!("idle does not return"),
            ended = timeout(PICTURE_WAIT, async { tokio::join!(ended, closed); }) =>
                ended.expect("run ended and closed the pictured POST"),
        }
        brain.close().await;
    }
    {
        let script = peer.0.lock().expect("peer");
        assert_eq!((script.starts, script.drops), (2, 2));
        assert_eq!(
            script.requests[1].len(),
            sent,
            "no later upload after termination"
        );
    }
    assert_eq!(
        reply
            .write_all(b"x")
            .await
            .expect_err("closed response pipe")
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );

    let db = Store::open(&scratch.db()).expect("reopen durable store");
    let (state, reason, screen): (String, Option<String>, String) = db
        .conn_for_tests()
        .query_row(
            "SELECT state, reason, screen FROM runs WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the pictured turn");
    let expected = match ending {
        PictureEnd::Success => ("done", None),
        PictureEnd::ProviderError => ("failed", Some("engine")),
        PictureEnd::Stop => ("interrupted", Some("stop")),
        PictureEnd::Quit => ("interrupted", Some("shell_lost")),
    };
    assert_eq!((state.as_str(), reason.as_deref()), expected);
    assert_eq!(screen, "screen", "history records only that it was sent");
    drop(db);
    let next = Box::pin(play(
        &scratch,
        store,
        &[(1, 2, "after restart")],
        vec![answers("Again.")],
    ))
    .await;
    assert_eq!(next.len(), 1);
    assert!(next[0].contains("after restart") && next[0].contains("look at this"));
    if ending == PictureEnd::Success {
        assert!(next[0].contains("Seen."));
    }
    assert!(
        !next[0].contains("\"type\":\"image\"")
            && !test_support::has_picture_marker(next[0].as_bytes(), KEY_PICTURE),
        "the reopened request contained the picture"
    );
    assert_key_picture_not_retained(&scratch, &captured);
}

#[tokio::test]
async fn pictured_key_success_reopens_with_only_text_and_a_screen_mark() {
    Box::pin(pictured_key_replay(PictureEnd::Success)).await;
}

#[tokio::test]
async fn pictured_key_provider_error_closes_without_retaining_the_picture() {
    Box::pin(pictured_key_replay(PictureEnd::ProviderError)).await;
}

#[tokio::test]
async fn pictured_key_stop_closes_the_post_without_retaining_the_picture() {
    Box::pin(pictured_key_replay(PictureEnd::Stop)).await;
}

#[tokio::test]
async fn pictured_key_quit_closes_the_post_without_retaining_the_picture() {
    Box::pin(pictured_key_replay(PictureEnd::Quit)).await;
}

/// A task's end takes the run's last attempt for its title; the title write hands over to the same
/// run's compaction, which is refused as spent: no connection, no summary, and the run stays Done.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real shell session through a title and its compaction"
)]
async fn a_title_takes_the_last_attempt_and_its_compaction_is_refused_unsent() {
    const ALLOWED: u32 = 17;
    let scratch = Scratch::new("offline-key-allowance-jobs");
    let (task, run) = {
        let mut store = Store::open(&scratch.db()).expect("store");
        store.recover().expect("recover");
        let (high_thread, high_run) = store.high_water().expect("ids");
        let task = kumochi_protocol::message::ThreadId(high_thread.0 + 1);
        for n in 1..=9 {
            let run = RunId(high_run.0 + n);
            let title = (n == 1).then_some("zqxoldask");
            store
                .begin_turn(task, title, run, &format!("zqxoldask {n}"))
                .expect("turn");
            store
                .end_run(task, run, &[], crate::store::RunEnd::Done)
                .expect("ended");
        }
        (task, RunId(high_run.0 + 10))
    };
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    Vault::with_service(store.clone(), "offline-key-runtime", &lock)
        .expect("fake vault")
        .save_anthropic_key(KEY.as_bytes())
        .expect("synthetic key");
    std::fs::write(
        scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
        format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
    ).expect("settings");
    let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: [
            http("application/json", &catalog.to_string()),
            answers("Done."),
            answers("Synthetic title"),
            answers("Synthetic summary."),
        ]
        .into(),
        ..Script::default()
    })));
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(peer.clone(), 1, 3));
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    brain.compact_at = 1;
    let earlier = brain.key_allowance.of(run).expect("the run's allowance");
    for _ in 0..ALLOWED - 2 {
        assert!(
            earlier.reserve(),
            "the run's earlier rounds and inline sends"
        );
    }
    drop(earlier);
    let mut seen = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        loop {
            if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
            {
                break;
            }
        }
        let DataToShell::Models { .. } = shell.data().await else {
            panic!("the verified catalog did not arrive");
        };
        let message = DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(task),
            text: "start".into(),
        };
        let frame = frame::encode(&message, Pipe::Data).expect("encodes");
        shell.data_out.write_all(&frame).await.expect("submitted");
        loop {
            let (actual, phase) = shell.state().await;
            assert_eq!(actual, run);
            match phase {
                Phase::Done => break,
                Phase::Thinking | Phase::Answering => {}
                _ => panic!("unexpected phase {phase:?}"),
            }
        }
        assert!(shell.read_to_end(run).await > 0);
        let mut titled = false;
        timeout(PATIENCE, async {
            loop {
                match seen.recv().await.expect("the brain reports") {
                    crate::runtime::Seen::StoreOpQueued(
                        crate::coordinator::StoreOp::SetTitle { .. },
                    ) => titled = true,
                    crate::runtime::Seen::StoreOpQueued(crate::coordinator::StoreOp::ReadJob(
                        crate::coordinator::ReadJob {
                            require_recovery: None,
                            kind: crate::coordinator::JobKind::Compact,
                            ..
                        },
                    )) if titled => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("the title write hands over to compaction");
        let summary = timeout(Duration::from_secs(3), async {
            loop {
                if let crate::runtime::Seen::StoreOpQueued(
                    crate::coordinator::StoreOp::StoreSummary { .. },
                ) = seen.recv().await.expect("the brain reports")
                {
                    return;
                }
            }
        })
        .await;
        assert!(summary.is_err(), "the refused compaction writes no summary");
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("routed runtime finishes");
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.key_allowance.spent(run), Some(ALLOWED));
    brain.close().await;
    let script = peer.0.lock().expect("peer");
    assert_eq!(script.starts, 3, "catalog, the run's round and its title");
    assert_eq!(script.responses.len(), 1, "the compaction never connected");
    drop(script);
    let db = Store::open(&scratch.db()).expect("persistent store");
    let count = |sql: &str| -> i64 {
        db.conn_for_tests()
            .query_row(sql, [], |row| row.get(0))
            .expect("count")
    };
    assert_eq!(count("SELECT COUNT(*) FROM summaries"), 0);
    assert_eq!(
        count("SELECT COUNT(*) FROM threads WHERE title = 'Synthetic title'"),
        1
    );
    assert_eq!(
        count(&format!(
            "SELECT COUNT(*) FROM runs WHERE id = {} AND state = 'done'",
            run.0
        )),
        1
    );
}

/// A 401 on a Messages post fails its run with the rejected-key notice and leaves the key
/// Unconfirmed, so Check again comes back and the next ask is refused here as unavailable instead
/// of sending another refused request. A 403 only fails its run, with the same notice: it can
/// concern one model, so the key stays Ready and the next ask sends.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real shell session per status through a refused ask and the next one"
)]
async fn a_refused_post_leaves_the_key_unconfirmed_and_the_next_ask_unsent() {
    for (status, kind, view, starts, next) in [
        (
            401,
            "authentication_error",
            KeyView::Unconfirmed,
            2,
            ErrorKey::ModelUnavailable,
        ),
        (
            403,
            "permission_error",
            KeyView::Ready,
            3,
            ErrorKey::AnthropicKeyRejected,
        ),
    ] {
        let scratch = Scratch::new(&format!("offline-key-refused-post-{status}"));
        let store = FakeStore::default();
        let lock = scratch.db().with_file_name("offline-key-runtime.lock");
        Vault::with_service(store.clone(), "offline-key-runtime", &lock)
            .expect("fake vault")
            .save_anthropic_key(KEY.as_bytes())
            .expect("synthetic key");
        std::fs::write(
        scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
        format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
    ).expect("settings");
        let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
        let body = json!({"type":"error","error":{"type":kind,"message":"synthetic"}}).to_string();
        let refused = format!(
        "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();
        let peer = Scripted(Arc::new(Mutex::new(Script {
            responses: [
                http("application/json", &catalog.to_string()),
                refused.clone(),
                refused,
            ]
            .into(),
            ..Script::default()
        })));
        let worker = Worker::new(
            Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
            Arc::new(FixedClock(0)),
        );
        let mut accounts =
            Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
        accounts
            .connections
            .use_test_transport(Transport::offline(peer.clone(), 1, 2));
        let mut brain = test_support::brain_with(&scratch, "", accounts);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            loop {
                if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
                {
                    break;
                }
            }
            let DataToShell::Models { .. } = shell.data().await else {
                panic!("the verified catalog did not arrive");
            };
            let mut ended = Vec::new();
            for turn in 1..=2 {
                let message = DataToBrain::TextSubmit {
                    turn,
                    screen: ScreenChoice::None,
                    thread: None,
                    text: "ask".into(),
                };
                let frame = frame::encode(&message, Pipe::Data).expect("encodes");
                shell.data_out.write_all(&frame).await.expect("submitted");
                loop {
                    let (_, phase, error) = shell.notice().await;
                    if !matches!(phase, Phase::Thinking | Phase::Answering) {
                        let notice = error.map(|error| (error.key(), error.actions().to_vec()));
                        ended.push((phase, notice));
                        break;
                    }
                }
            }
            drop(shell);
            ended
        };
        let (ended, outcomes) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .expect("routed runtime finishes");
        assert_eq!(ended, Ended::Lost);
        let open = vec![Action::OpenSettings];
        assert_eq!(
            outcomes,
            [
                (
                    Phase::Failed,
                    Some((ErrorKey::AnthropicKeyRejected, open.clone()))
                ),
                (Phase::Failed, Some((next, open))),
            ],
            "{status}: the refused ask, then the next one"
        );
        assert_eq!(brain.account.connections.anthropic(), view, "{status}");
        brain.close().await;
        let script = peer.0.lock().expect("peer");
        assert_eq!(
            script.starts, starts,
            "{status}: the catalog read and each post sent"
        );
    }
}

/// A title job's 401 retires the key as a run's does: the row turns Unconfirmed with no run left
/// to report it, and the next ask is refused here without connecting.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real shell session through a refused title and the next ask"
)]
async fn a_title_jobs_refused_post_leaves_the_next_ask_unsent() {
    let scratch = Scratch::new("offline-key-refused-title");
    let task = {
        let mut store = Store::open(&scratch.db()).expect("store");
        store.recover().expect("recover");
        let (high_thread, high_run) = store.high_water().expect("ids");
        let task = kumochi_protocol::message::ThreadId(high_thread.0 + 1);
        let run = RunId(high_run.0 + 1);
        store
            .begin_turn(task, Some("zqxoldask"), run, "zqxoldask")
            .expect("turn");
        store
            .end_run(task, run, &[], crate::store::RunEnd::Done)
            .expect("ended");
        task
    };
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    Vault::with_service(store.clone(), "offline-key-runtime", &lock)
        .expect("fake vault")
        .save_anthropic_key(KEY.as_bytes())
        .expect("synthetic key");
    std::fs::write(
        scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
        format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
    ).expect("settings");
    let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
    let body =
        json!({"type":"error","error":{"type":"authentication_error","message":"synthetic"}})
            .to_string();
    let refused = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: [
            http("application/json", &catalog.to_string()),
            answers("Done."),
            refused,
        ]
        .into(),
        ..Script::default()
    })));
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(peer.clone(), 1, 3));
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    brain.compact_at = u64::MAX;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        loop {
            if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
            {
                break;
            }
        }
        let DataToShell::Models { .. } = shell.data().await else {
            panic!("the verified catalog did not arrive");
        };
        let mut ended = Vec::new();
        for turn in 1..=2 {
            let message = DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                thread: Some(task),
                text: "ask".into(),
            };
            let frame = frame::encode(&message, Pipe::Data).expect("encodes");
            shell.data_out.write_all(&frame).await.expect("submitted");
            let run = loop {
                let (run, phase) = shell.state().await;
                if !matches!(phase, Phase::Thinking | Phase::Answering) {
                    ended.push(phase);
                    break run;
                }
            };
            if turn == 1 {
                assert!(shell.read_to_end(run).await > 0);
                timeout(Duration::from_secs(10), async {
                    while !matches!(shell.next_control().await.expect("control"),
                        Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Unconfirmed)
                    {}
                })
                .await
                .expect("the title job's refusal reaches the row");
            }
        }
        drop(shell);
        ended
    };
    let (ended, phases) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("routed runtime finishes");
    assert_eq!(ended, Ended::Lost);
    assert_eq!(phases, [Phase::Done, Phase::Failed]);
    assert_eq!(brain.account.connections.anthropic(), KeyView::Unconfirmed);
    brain.close().await;
    let script = peer.0.lock().expect("peer");
    assert_eq!(
        script.starts, 3,
        "the catalog read, the answer and the refused title"
    );
}

/// A title rereads the saved default when its task ends. A model the key's catalog doesn't offer
/// gets no request, even when the run before it was answered on an offered one.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real shell session through a default change and its title"
)]
async fn a_title_never_posts_to_a_saved_model_the_catalog_does_not_offer() {
    let scratch = Scratch::new("offline-key-unoffered-title");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    Vault::with_service(store.clone(), "offline-key-runtime", &lock)
        .expect("fake vault")
        .save_anthropic_key(KEY.as_bytes())
        .expect("synthetic key");
    std::fs::write(
        scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
        format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
    ).expect("settings");
    let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
    let gate = Arc::new(tokio::sync::Notify::new());
    let peer = Scripted(Arc::new(Mutex::new(Script {
        responses: [
            http("application/json", &catalog.to_string()),
            answers("Done."),
            answers("Synthetic title"),
        ]
        .into(),
        connect_gate: Some(Arc::clone(&gate)),
        ..Script::default()
    })));
    gate.notify_one();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(peer.clone(), 1, 2));
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    brain.compact_at = u64::MAX;
    let mut seen = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let starts = || peer.0.lock().expect("peer").starts;
    let unoffered = kumochi_protocol::message::ModelChoice {
        model: kumochi_protocol::message::ModelSlug::new("claude-opus-5-5").expect("slug"),
        effort: kumochi_protocol::message::Effort::Medium,
        connection: kumochi_protocol::message::ConnectionKind::AnthropicKey,
    };
    let client = async {
        loop {
            if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
            {
                break;
            }
        }
        let DataToShell::Models { .. } = shell.data().await else {
            panic!("the verified catalog did not arrive");
        };
        let message = DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: None,
            text: "start".into(),
        };
        let frame = frame::encode(&message, Pipe::Data).expect("encodes");
        shell.data_out.write_all(&frame).await.expect("submitted");
        timeout(PATIENCE, async {
            while starts() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the run's round connects");
        shell
            .send(&ControlToBrain::Settings(
                kumochi_protocol::message::SettingsOp::SetDefaultModel(unoffered.clone()),
            ))
            .await;
        loop {
            if let Some(ControlToShell::Settings { view, answer: true }) =
                shell.next_control().await.expect("control")
            {
                assert_eq!(view.default_model.as_ref(), Some(&unoffered));
                break;
            }
        }
        gate.notify_one();
        let run = loop {
            let (run, phase) = shell.state().await;
            match phase {
                Phase::Done => break run,
                Phase::Thinking | Phase::Answering => {}
                _ => panic!("unexpected phase {phase:?}"),
            }
        };
        assert!(shell.read_to_end(run).await > 0);
        timeout(PATIENCE, async {
            loop {
                if let crate::runtime::Seen::StoreOpQueued(crate::coordinator::StoreOp::ReadJob(
                    crate::coordinator::ReadJob {
                        require_recovery: None,
                        kind: crate::coordinator::JobKind::Title,
                        ..
                    },
                )) = seen.recv().await.expect("the brain reports")
                {
                    return;
                }
            }
        })
        .await
        .expect("the task's end reads its title job");
        gate.notify_one();
        let titled = timeout(Duration::from_secs(3), async {
            loop {
                if let crate::runtime::Seen::StoreOpQueued(
                    crate::coordinator::StoreOp::SetTitle { .. },
                ) = seen.recv().await.expect("the brain reports")
                {
                    return;
                }
            }
        })
        .await;
        assert!(titled.is_err(), "no title comes from an unoffered model");
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("routed runtime finishes");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let script = peer.0.lock().expect("peer");
    assert_eq!(
        script.starts, 2,
        "the catalog and the run's round, and no title"
    );
    assert_eq!(script.responses.len(), 1, "the title never connected");
}

/// A Check's catalog read and a Messages post can both be refused on the same key. Whichever
/// refusal the account handles first retires the key, the run that was sending on it ends with
/// the rejected-key notice, and the next ask is refused here as unavailable without a post.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one real shell session per order through a held post, a Check and the next ask"
)]
async fn a_refused_check_racing_a_refused_post_keeps_the_rejected_key_notice() {
    let refused = || {
        let body =
            json!({"type":"error","error":{"type":"authentication_error","message":"synthetic"}})
                .to_string();
        format!(
            "HTTP/1.1 401 Refused\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    for catalog_first in [true, false] {
        let scratch = Scratch::new(&format!("offline-key-refused-race-{catalog_first}"));
        let store = FakeStore::default();
        let lock = scratch.db().with_file_name("offline-key-runtime.lock");
        Vault::with_service(store.clone(), "offline-key-runtime", &lock)
            .expect("fake vault")
            .save_anthropic_key(KEY.as_bytes())
            .expect("synthetic key");
        std::fs::write(
            scratch.db().with_file_name(kumochi_protocol::constants::CONFIG_FILE),
            format!("default_model = {{ connection = \"AnthropicKey\", model = \"{MODEL}\", effort = \"Medium\" }}\n"),
        ).expect("settings");
        let catalog = json!({"data":[{"type":"model","id":MODEL,"display_name":"Synthetic model"}],"has_more":false});
        let peer = Scripted(Arc::new(Mutex::new(Script {
            responses: [http("application/json", &catalog.to_string())].into(),
            ..Script::default()
        })));
        let worker = Worker::new(
            Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
            Arc::new(FixedClock(0)),
        );
        let mut accounts =
            Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
        accounts
            .connections
            .use_test_transport(Transport::offline(peer.clone(), 2, 2));
        let mut brain = test_support::brain_with(&scratch, "", accounts);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let connected = |count: usize| {
            let peer = peer.clone();
            async move {
                while peer.0.lock().expect("peer").starts < count {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        };
        let ask = |turn| {
            let message = DataToBrain::TextSubmit {
                turn,
                screen: ScreenChoice::None,
                thread: None,
                text: "ask".into(),
            };
            frame::encode(&message, Pipe::Data).expect("encodes")
        };
        let client = async {
            loop {
                if matches!(shell.next_control().await.expect("control"),
                Some(ControlToShell::Connections(view)) if view.anthropic == KeyView::Ready)
                {
                    break;
                }
            }
            let DataToShell::Models { .. } = shell.data().await else {
                panic!("the verified catalog did not arrive");
            };
            let (post, mut post_reply) = tokio::io::duplex(4096);
            peer.0.lock().expect("peer").reader = Some(Box::new(post));
            shell.data_out.write_all(&ask(1)).await.expect("submitted");
            connected(2).await;
            let (read, catalog_reply) = tokio::io::duplex(4096);
            {
                let mut script = peer.0.lock().expect("peer");
                if catalog_first {
                    script.responses.push_back(refused());
                } else {
                    script.reader = Some(Box::new(read));
                }
            }
            shell
                .send(&ControlToBrain::Connection(
                    kumochi_protocol::message::ConnectionOp::CheckAnthropicKey,
                ))
                .await;
            connected(3).await;
            if !catalog_first {
                post_reply
                    .write_all(&refused())
                    .await
                    .expect("the post's refusal");
            }
            let mut ended = Vec::new();
            for turn in 1..=2 {
                if turn == 2 {
                    shell.data_out.write_all(&ask(2)).await.expect("submitted");
                }
                loop {
                    let (_, phase, error) = shell.notice().await;
                    if !matches!(phase, Phase::Thinking | Phase::Answering) {
                        let notice = error.map(|error| (error.key(), error.actions().to_vec()));
                        ended.push((phase, notice));
                        break;
                    }
                }
            }
            drop((post_reply, catalog_reply, shell));
            ended
        };
        let (ended, outcomes) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .expect("routed runtime finishes");
        assert_eq!(ended, Ended::Lost);
        let open = vec![Action::OpenSettings];
        assert_eq!(
            outcomes,
            [
                (
                    Phase::Failed,
                    Some((ErrorKey::AnthropicKeyRejected, open.clone()))
                ),
                (Phase::Failed, Some((ErrorKey::ModelUnavailable, open))),
            ],
            "catalog refusal handled first: {catalog_first}"
        );
        brain.close().await;
        let script = peer.0.lock().expect("peer");
        let posts = script
            .requests
            .iter()
            .filter(|request| request.starts_with(b"POST "))
            .count();
        assert_eq!(
            (script.starts, posts),
            (3, 1),
            "catalog refusal handled first: {catalog_first}: two catalog reads and the one post"
        );
    }
}
