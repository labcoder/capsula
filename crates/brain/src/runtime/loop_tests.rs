//! The tool loop across the brain: a round's call is answered and stored, and the next round is
//! built from the store, with the call, its result and nothing else of round 1 in its input.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ActionState, ApprovalRev, ControlToShell, DataToBrain, DataToShell, DraftId, IssueCardsStatus,
    Phase, RunId, StateVersion, ThreadId,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::{PATIENCE, duplex, patient_brain_with};
use crate::context::InputItem;
use crate::coordinator::{Input, SendKey};
use crate::engines::{
    CallId, Engine, Finish, Opaque, Round, RunRequest, SendGate, ToolName, TurnEvent,
};
use crate::runtime::Ended;
use crate::tap::Tap;
use crate::test_support::Scratch;

const CALL: &str = r#"{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"}"#;
const ANSWER: &str = r#"{"type":"message","role":"assistant","content":"done"}"#;

/// Round 1 calls a tool Kumochi doesn't have; round 2 answers. Each start reports its input.
#[derive(Debug)]
struct Rounds {
    seen: mpsc::UnboundedSender<Vec<InputItem>>,
}

fn item(json: &str) -> Opaque {
    Opaque::new(json.as_bytes().to_vec()).expect("small")
}

impl Engine for Rounds {
    fn start(
        &self,
        request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        let SendKey::Build(key) = request.key else {
            panic!("only runs start here");
        };
        self.seen.send(request.request.input).ok();
        let call = CallId::new("c1").expect("short");
        let script = if key.round == 1 {
            vec![
                TurnEvent::ToolCallStarted {
                    call: call.clone(),
                    name: ToolName::Unknown,
                },
                TurnEvent::ToolCallReady {
                    call,
                    args: "{}".to_owned(),
                },
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(CALL),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        } else {
            vec![
                TurnEvent::TextDelta("done".to_owned()),
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(ANSWER),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        };
        Box::pin(async move {
            for event in script {
                if events.send(event).await.is_err() {
                    return;
                }
            }
        })
    }
}

#[tokio::test]
async fn a_called_round_is_stored_and_the_next_round_replays_it() {
    let scratch = Scratch::new("loop");
    let (seen, mut inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(&scratch, Box::new(Rounds { seen }));
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run,
                delta: "done".into()
            }
        );
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;

    let first = inputs.recv().await.expect("round 1");
    let second = inputs.recv().await.expect("round 2");
    assert_eq!(first.len(), 1, "round 1 sends only the turn");
    assert_eq!(second[..1], first[..], "the turn leads round 2 too");
    let [InputItem::Native(native), result] = &second[1..] else {
        panic!("round 2 replays the call and its result, not {second:?}");
    };
    let json = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).expect("json");
    assert_eq!(
        json(native.bytes()),
        json(CALL.as_bytes()),
        "the same JSON value"
    );
    assert_eq!(
        *result,
        InputItem::FunctionOutput {
            call: CallId::new("c1").expect("short"),
            output: r#"{"error":"unknown tool"}"#.to_owned(),
        }
    );
    assert_eq!(inputs.recv().await, None, "no third round");
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let kinds: Vec<String> = store
        .conn_for_tests()
        .prepare("SELECT kind FROM events WHERE run IS NOT NULL ORDER BY id")
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(
        kinds,
        [
            "user",
            "output_item",
            "tool_result",
            "assistant_text",
            "output_item"
        ]
    );
}

const DRAFT_ARGS: &str =
    r#"{"title":"  Local café  ","body":"first\r\n\tcafé\r\n","repo":"owner/repo"}"#;
const UNBOUND_DRAFT_ARGS: &str = r#"{"title":"  Local café  ","body":"first\r\n\tcafé\r\n"}"#;
const DRAFT_CALL: &str = r#"{"type":"function_call","call_id":"draft-1","name":"draft_issue","arguments":"{\"title\":\"  Local café  \",\"body\":\"first\\r\\n\\tcafé\\r\\n\",\"repo\":\"owner/repo\"}"}"#;

#[derive(Debug)]
struct DraftRounds {
    seen: mpsc::UnboundedSender<(ThreadId, Vec<InputItem>)>,
    without_repo: bool,
}

impl Engine for DraftRounds {
    fn start(
        &self,
        request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        let SendKey::Build(key) = request.key else {
            panic!("only runs start here");
        };
        self.seen.send((request.thread, request.request.input)).ok();
        let call = CallId::new("draft-1").expect("short ASCII");
        let args = if self.without_repo {
            UNBOUND_DRAFT_ARGS
        } else {
            DRAFT_ARGS
        };
        let script = if key.round == 1 {
            vec![
                TurnEvent::ToolCallStarted {
                    call: call.clone(),
                    name: ToolName::DraftIssue,
                },
                TurnEvent::ToolCallReady {
                    call,
                    args: args.to_owned(),
                },
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(
                        &serde_json::json!({
                            "type":"function_call", "call_id":"draft-1", "name":"draft_issue",
                            "arguments":args,
                        })
                        .to_string(),
                    ),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        } else {
            vec![
                TurnEvent::TextDelta("done".to_owned()),
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(ANSWER),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        };
        Box::pin(async move {
            for event in script {
                if events.send(event).await.is_err() {
                    return;
                }
            }
        })
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the model round, SQL commit, correlated wire read and replay in one regression"
)]
async fn a_model_draft_commits_and_replays_with_a_correlated_card_without_http() {
    let scratch = Scratch::new("loop-draft");
    let (seen, mut inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(
        &scratch,
        Box::new(DraftRounds {
            seen,
            without_repo: false,
        }),
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let http_attempts = Arc::clone(&attempts);
    brain.account.net.http = Arc::new(move || {
        http_attempts.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::http_client::HttpError::Configuration)
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("draft this locally").await;
        assert_eq!(shell.state().await, (RunId(1), Phase::Thinking));
        assert_eq!(shell.thread, Some(ThreadId(2)));
        assert_eq!(shell.control().await, Some(ControlToShell::ThreadsChanged));
        assert_eq!(shell.state().await, (RunId(1), Phase::Answering));
        assert_eq!(shell.state().await, (RunId(1), Phase::Done));
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run: RunId(1),
                delta: "done".into(),
            }
        );
        assert_eq!(shell.data().await, DataToShell::RunEnd { run: RunId(1) });
        let request = DataToBrain::IssueCardsRequest {
            request: 73,
            drafts: kumochi_scene::text::Bounded::new(vec![DraftId(1)]).expect("one draft"),
        };
        shell
            .data_out
            .write_all(&frame::encode(&request, Pipe::Data).expect("frame"))
            .await
            .expect("sent");
        let DataToShell::IssueCardRead { request, card } = shell.data().await else {
            panic!("a correlated authoritative card");
        };
        assert_eq!(request, 73);
        assert_eq!(card.draft, DraftId(1));
        assert_eq!(card.thread, ThreadId(2));
        assert_eq!(card.title.as_str(), "  Local café  ");
        assert_eq!(card.body.as_str(), "first\n\tcafé\n");
        assert_eq!(
            card.repo
                .as_ref()
                .map(kumochi_protocol::message::RepoName::as_str),
            Some("owner/repo")
        );
        assert_eq!(card.account, None);
        assert_eq!(card.state, ActionState::Draft);
        assert_eq!(card.version, StateVersion(1));
        assert_eq!(card.approval, Some(ApprovalRev(1)));
        assert_eq!(
            shell.data().await,
            DataToShell::IssueCardsDone {
                request: 73,
                status: IssueCardsStatus::Complete,
            }
        );
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("the draft round and card read finish");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);

    let (thread, first) = inputs.recv().await.expect("round 1");
    let (next_thread, second) = inputs.recv().await.expect("round 2");
    assert_eq!(thread, ThreadId(2));
    assert_eq!(next_thread, ThreadId(2));
    assert_eq!(first.len(), 1);
    assert_eq!(second[..1], first[..]);
    let [InputItem::Native(native), result] = &second[1..] else {
        panic!("round 2 replays only the draft call and its result after the turn");
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(native.bytes()).expect("JSON"),
        serde_json::from_str::<serde_json::Value>(DRAFT_CALL).expect("JSON")
    );
    assert_eq!(
        *result,
        InputItem::FunctionOutput {
            call: CallId::new("draft-1").expect("short ASCII"),
            output: "Saved a local issue draft for user review. Nothing was filed on GitHub."
                .to_owned(),
        }
    );
    assert_eq!(inputs.recv().await, None, "no third round");
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let stored: String = store
        .conn_for_tests()
        .query_row(
            "SELECT json_array(
                json_array((SELECT count(*) FROM actions),
                    (SELECT count(*) FROM events WHERE kind IN ('draft', 'action')),
                    (SELECT count(*) FROM tool_calls),
                    (SELECT count(*) FROM events WHERE kind = 'tool_result'),
                    (SELECT last FROM approval_mark WHERE id = 1)),
                json_array(a.id, a.thread, a.version, json(a.stage), a.repo, a.account_id,
                    a.account_login, a.title, a.body, a.approval, a.consumed, a.uncertain),
                json_array(e.thread, e.run, e.kind, e.text, json(e.body)),
                json_array(c.run, c.thread, c.route, c.call, c.tool, json(c.result)),
                json_array(r.thread, r.run, r.kind, r.text, json(r.body)))
             FROM actions a JOIN events e ON e.kind = 'draft'
             JOIN tool_calls c ON c.tool = 'draft_issue'
             JOIN events r ON r.kind = 'tool_result'",
            [],
            |row| row.get(0),
        )
        .expect("one committed draft and result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).expect("JSON"),
        serde_json::json!([
            [1, 1, 1, 1, 1],
            [1, 2, 1, {"kind":"draft"}, "owner/repo", null, null,
                "  Local café  ", "first\n\tcafé\n", 1, null, 0],
            [2, null, "draft", "  Local café  ", {"draft":1}],
            [1, 2, "model", "draft-1", "draft_issue",
                {"text":"Saved a local issue draft for user review. Nothing was filed on GitHub."}],
            [2, 1, "tool_result", null, {"call":"draft-1", "output":
                {"text":"Saved a local issue draft for user review. Nothing was filed on GitHub."}}]
        ])
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps real SQL, Stop, delayed acknowledgement and owner cleanup in one regression"
)]
async fn a_sql_committed_draft_notifies_history_after_stop_without_restarting_the_model() {
    let scratch = Scratch::new("loop-draft-stop");
    let (seen, mut inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(
        &scratch,
        Box::new(DraftRounds {
            seen,
            without_repo: false,
        }),
    );
    let (forward, routed) = mpsc::unbounded_channel();
    let mut actual = std::mem::replace(&mut brain.inputs, routed);
    let (committed, acknowledgement) = oneshot::channel();
    let forwarded = forward.clone();
    let relay = async move {
        let mut committed = Some(committed);
        while let Some(input) = actual.recv().await {
            if matches!(&input, Input::CallCommitted { .. }) {
                committed
                    .take()
                    .expect("one draft call")
                    .send(input)
                    .expect("the client awaits the real SQL acknowledgement");
            } else {
                forwarded.send(input).expect("the runtime remains open");
            }
        }
    };
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("draft before stop").await;
        assert_eq!(shell.state().await, (RunId(1), Phase::Thinking));
        let acknowledgement = acknowledgement.await.expect("SQL committed");
        let Input::CallCommitted { key, outcome, then } = &acknowledgement else {
            panic!("the store's actual call acknowledgement");
        };
        assert_eq!((key.run, &*key.call), (RunId(1), "draft-1"));
        assert_eq!(*outcome, crate::dispatch::Committed::Done);
        assert_eq!(*then, Some(crate::dispatch::Then::IssueDrafted));
        let reader = rusqlite::Connection::open_with_flags(
            scratch.db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("independent reader");
        let durable: (i64, i64, i64, String) = reader
            .query_row(
                "SELECT (SELECT count(*) FROM actions),
                (SELECT count(*) FROM events WHERE kind = 'draft'),
                (SELECT count(*) FROM tool_calls), state FROM runs WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("committed data before acknowledgement delivery");
        assert_eq!(durable, (1, 1, 1, "active".to_owned()));
        drop(reader);
        shell.stop(RunId(1)).await;
        assert_eq!(shell.state().await, (RunId(1), Phase::Interrupted));
        forward
            .send(acknowledgement)
            .expect("deliver after cancellation");
        assert_eq!(shell.control().await, Some(ControlToShell::ThreadsChanged));
        drop(shell);
    };
    let lifecycle = Box::pin(async {
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        assert_eq!(brain.coordinator.current_run(), None);
        brain.close().await;
    });
    timeout(PATIENCE, async { tokio::join!(lifecycle, relay) })
        .await
        .expect("stop, notification and all runtime/store owners finish");
    let (thread, first) = inputs.recv().await.expect("the draft round");
    assert_eq!(thread, ThreadId(2));
    assert_eq!(first.len(), 1);
    assert_eq!(
        inputs.recv().await,
        None,
        "the stopped model never starts round two"
    );
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let durable: (i64, i64, i64, i64, String) = store
        .conn_for_tests()
        .query_row(
            "SELECT (SELECT count(*) FROM actions),
            (SELECT count(*) FROM events WHERE kind = 'draft'),
            (SELECT count(*) FROM tool_calls),
            (SELECT count(*) FROM events WHERE kind = 'tool_result'),
            state FROM runs WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("durable canceled run and local draft");
    assert_eq!(durable, (1, 1, 1, 0, "interrupted".to_owned()));
    assert_eq!(
        super::outcomes(&scratch),
        [super::outcome("interrupted", "stop")]
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps committed config, model invocation, correlated card and SQL assertions together"
)]
async fn a_model_draft_binds_the_committed_default_not_the_account_views_default() {
    use crate::connections::github::device::DefaultRepo;
    use kumochi_protocol::message::{GitHubView, RepoName};
    let scratch = Scratch::new("loop-draft-default");
    let (seen, _inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(
        &scratch,
        Box::new(DraftRounds {
            seen,
            without_repo: true,
        }),
    );
    brain.account.github = crate::runtime::account::github::draft_tests::connected_owner();
    brain.config =
        crate::config::Config::load(Some(scratch.db().with_file_name("settings")), ThreadId(1));
    let pending = brain
        .config
        .stage_github_default(&DefaultRepo {
            account: 7,
            repo: RepoName::new("owner/other").expect("listed"),
        })
        .expect("stage committed default");
    pending.write().expect("synthetic non-secret config");
    brain.config.commit(pending);
    brain.account.github.restore_default(Some(DefaultRepo {
        account: 7,
        repo: RepoName::new("owner/repo").expect("listed"),
    }));
    let attempts = Arc::new(AtomicUsize::new(0));
    let http_attempts = Arc::clone(&attempts);
    brain.account.net.http = Arc::new(move || {
        http_attempts.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::http_client::HttpError::Configuration)
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let Some(ControlToShell::GitHub(GitHubView::Connected { default, .. })) =
            shell.control().await
        else {
            panic!("connected account view")
        };
        assert_eq!(default.as_ref().map(RepoName::as_str), Some("owner/repo"));
        assert_eq!(shell.scenes().await, []);
        shell.submit("use my committed default").await;
        assert_eq!(shell.state().await, (RunId(1), Phase::Thinking));
        assert_eq!(shell.control().await, Some(ControlToShell::ThreadsChanged));
        assert_eq!(shell.state().await, (RunId(1), Phase::Answering));
        assert_eq!(shell.state().await, (RunId(1), Phase::Done));
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run: RunId(1),
                delta: "done".into()
            }
        );
        assert_eq!(shell.data().await, DataToShell::RunEnd { run: RunId(1) });
        shell
            .data_out
            .write_all(
                &frame::encode(
                    &DataToBrain::IssueCardsRequest {
                        request: 91,
                        drafts: kumochi_scene::text::Bounded::new(vec![DraftId(1)])
                            .expect("one draft"),
                    },
                    Pipe::Data,
                )
                .expect("frame"),
            )
            .await
            .expect("sent");
        let DataToShell::IssueCardRead { request, card } = shell.data().await else {
            panic!("the bound draft's authoritative card");
        };
        assert_eq!(request, 91);
        assert_eq!((card.draft, card.thread), (DraftId(1), ThreadId(2)));
        assert_eq!(
            card.repo.as_ref().map(RepoName::as_str),
            Some("owner/other")
        );
        assert_eq!(
            card.account
                .as_ref()
                .map(kumochi_protocol::message::GitHubLogin::as_str),
            Some("owner-sentinel")
        );
        assert_eq!(card.title.as_str(), "  Local café  ");
        assert_eq!(card.body.as_str(), "first\n\tcafé\n");
        assert_eq!(card.state, ActionState::Draft);
        assert_eq!(
            (card.version, card.approval),
            (StateVersion(1), Some(ApprovalRev(1)))
        );
        assert_eq!(
            shell.data().await,
            DataToShell::IssueCardsDone {
                request: 91,
                status: IssueCardsStatus::Complete,
            }
        );
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("model, draft and correlated card finish");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let bound: (String, String, String, i64) = store
        .conn_for_tests()
        .query_row(
            "SELECT repo, account_id, body, (SELECT count(*) FROM actions) FROM actions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("one bound draft");
    assert_eq!(
        bound,
        (
            "owner/other".to_owned(),
            "7".to_owned(),
            "first\n\tcafé\n".to_owned(),
            1
        )
    );
}

#[tokio::test]
async fn a_known_unavailable_proposal_reaches_the_model_as_refusal_without_a_draft() {
    use crate::connections::github::device::{Input as GitHubInput, Repositories};
    use kumochi_protocol::message::{GitHubView, RepoName};
    let scratch = Scratch::new("loop-draft-refused");
    let (seen, mut inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(
        &scratch,
        Box::new(DraftRounds {
            seen,
            without_repo: false,
        }),
    );
    brain.account.github = crate::runtime::account::github::draft_tests::connected_owner();
    let generation = brain.account.github.machine().stamp().generation;
    crate::runtime::account::github_tests::drive(
        &mut brain.account,
        GitHubInput::Repos {
            generation,
            result: Ok(Repositories {
                names: kumochi_scene::text::Bounded::new(vec![
                    RepoName::new("owner/other").expect("repo"),
                ])
                .expect("one listed repo"),
            }),
        },
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let http_attempts = Arc::clone(&attempts);
    brain.account.net.http = Arc::new(move || {
        http_attempts.fetch_add(1, Ordering::SeqCst);
        Err(crate::connections::http_client::HttpError::Configuration)
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let Some(ControlToShell::GitHub(GitHubView::Connected { repos, .. })) =
            shell.control().await
        else {
            panic!("complete connected account view")
        };
        assert_eq!(
            repos.iter().map(RepoName::as_str).collect::<Vec<_>>(),
            ["owner/other"]
        );
        assert_eq!(shell.scenes().await, []);
        shell.submit("do not retarget my repository").await;
        assert_eq!(shell.state().await, (RunId(1), Phase::Thinking));
        assert_eq!(shell.state().await, (RunId(1), Phase::Answering));
        assert_eq!(shell.state().await, (RunId(1), Phase::Done));
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run: RunId(1),
                delta: "done".into()
            }
        );
        assert_eq!(shell.data().await, DataToShell::RunEnd { run: RunId(1) });
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("the refused call finishes normally");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    let first = inputs.recv().await.expect("first round");
    let (thread, second) = inputs.recv().await.expect("second round");
    assert_eq!(first.0, ThreadId(2));
    assert_eq!(thread, ThreadId(2));
    assert_eq!(second[..1], first.1[..]);
    assert_eq!(second.len(), 3);
    assert_eq!(
        second[2],
        InputItem::FunctionOutput {
            call: CallId::new("draft-1").expect("call"),
            output: r#"{"error":"not available here"}"#.to_owned(),
        }
    );
    assert_eq!(inputs.recv().await, None);
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let counts: (i64, i64, i64, i64) = store
        .conn_for_tests()
        .query_row(
            "SELECT (SELECT count(*) FROM actions),
            (SELECT count(*) FROM events WHERE kind IN ('draft', 'action')),
            (SELECT count(*) FROM tool_calls),
            (SELECT count(*) FROM events WHERE kind = 'tool_result')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("no local draft or successful call ledger");
    assert_eq!(counts, (0, 0, 0, 1));
}
