use std::sync::{Arc, Mutex};

use kumochi_protocol::message::{KeyView, ScreenChoice};
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

#[expect(
    clippy::too_many_lines,
    reason = "one real shell session from catalog to transcript"
)]
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
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(peer.clone(), 1, count));
    let mut brain = test_support::brain_with(scratch, "", accounts);
    brain.compact_at = u64::MAX;
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

/// A Messages post refused for its key leaves the key Unconfirmed, so Check again comes back and
/// the next ask is refused here instead of sending another refused request.
#[tokio::test]
async fn a_refused_post_leaves_the_key_unconfirmed_and_the_next_ask_unsent() {
    let scratch = Scratch::new("offline-key-refused-post");
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
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
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
                let (_, phase) = shell.state().await;
                if !matches!(phase, Phase::Thinking | Phase::Answering) {
                    ended.push(phase);
                    break;
                }
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
    assert_eq!(phases, [Phase::Failed, Phase::Failed]);
    assert_eq!(brain.account.connections.anthropic(), KeyView::Unconfirmed);
    brain.close().await;
    let script = peer.0.lock().expect("peer");
    assert_eq!(
        script.starts, 2,
        "the catalog read and the one refused post"
    );
}

/// A title job's 401 retires the key as a run's does: the row turns Unconfirmed with no run left
/// to report it, and the next ask is refused here without connecting.
#[tokio::test]
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
