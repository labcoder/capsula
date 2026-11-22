use super::*;
use crate::bridge::listener::{
    Binding, LaunchContext, McpCall, McpOrigin, McpReply, SessionEnd, session,
};
use crate::bridge::{CallKey, Input as BridgeInput, relay};
use crate::connections::github::device::DefaultRepo;
use crate::coordinator::{Effect, Input, draft_launch};
use crate::dispatch::{Committed, Then, ToolError};
use crate::engines::ToolName;
use crate::runtime::claude::Wake;
use kumochi_protocol::message::{DraftId, GitHubOp};
use tokio::sync::{mpsc, oneshot, watch};

const ARGS: &[u8] = r#"{"title":"  Local café  ","body":"first\r\n\tcafé\r\n"}"#.as_bytes();
const SAVED: &str = "Saved a local issue draft for user review. Nothing was filed on GitHub.";

async fn expect_reply(reply: oneshot::Receiver<McpReply>, expected: (&str, bool)) {
    let reply = timeout(PATIENCE, reply).await.unwrap().unwrap();
    assert_eq!((reply.text.as_str(), reply.error), expected);
}

fn raw_call(n: u32, args: &str) -> BridgeInput {
    BridgeInput::from_test_line(format!(
        r#"{{"jsonrpc":"2.0","id":{n},"method":"tools/call","params":{{"name":"draft_issue","arguments":{args}}}}}"#
    ).as_bytes())
}

async fn bridge_reply(out: &mut mpsc::Receiver<Vec<u8>>) -> serde_json::Value {
    serde_json::from_slice(&out.recv().await.unwrap()).unwrap()
}

fn reply_at(n: u32, (text, error): (&str, bool)) -> serde_json::Value {
    json!({"jsonrpc":"2.0","id":n,"result":{
        "isError":error,"content":[{"type":"text","text":text}]
    }})
}

fn draft_totals(scratch: &Scratch) -> (i64, i64, i64, i64) {
    rusqlite::Connection::open(scratch.db())
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM actions),
                (SELECT last FROM approval_mark),
                (SELECT COUNT(*) FROM events WHERE kind = 'draft'),
                (SELECT COUNT(*) FROM tool_calls)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn call_draft(brain: &mut Brain, origin: McpOrigin) -> oneshot::Receiver<McpReply> {
    let (reply, receiver) = oneshot::channel();
    brain
        .claude_done(
            Wake::Call(McpCall {
                origin,
                tool: ToolName::DraftIssue,
                args: Some(ARGS.to_vec()),
                reply,
            }),
            None,
        )
        .unwrap();
    receiver
}

async fn save_draft(brain: &mut Brain, origin: McpOrigin, file: &Scratch, id: u64) -> Action {
    let answer = call_draft(brain, origin);
    let ack = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        ack,
        Input::CallCommitted {
            outcome: Committed::Done,
            ..
        }
    ));
    assert_eq!(
        brain.handle(ack).unwrap(),
        [Effect::Control(ControlToShell::ThreadsChanged)]
    );
    expect_reply(answer, (SAVED, false)).await;
    assert_eq!(brain.filing.review_counts(), (0, 0));
    Store::open(&file.db())
        .unwrap()
        .action(DraftId(id))
        .unwrap()
        .unwrap()
}

fn committed_default(brain: &mut Brain, account: u64, repo: &str) {
    let pending = brain
        .config
        .stage_github_default(&DefaultRepo {
            account,
            repo: RepoName::new(repo).unwrap(),
        })
        .unwrap();
    pending.write().unwrap();
    brain.config.commit(pending);
    brain
        .account
        .github
        .restore_default(brain.config.github_default());
}

fn assert_no_filing_http(server: &FilingServer) {
    assert_eq!(
        (
            server.factories.load(Ordering::SeqCst),
            server.gets.load(Ordering::SeqCst),
            server.posts.load(Ordering::SeqCst)
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn mcp_draft_defaults_need_the_same_verified_account_and_complete_list() {
    use crate::connections::github::api::ApiError;
    for (case, default, repo, account) in [
        (
            "listed",
            Some((7, "owner/repo")),
            Some("owner/repo"),
            Some(7),
        ),
        ("absent", None, None, Some(7)),
        ("other account", Some((8, "owner/repo")), None, Some(7)),
        ("removed from list", Some((7, "owner/repo")), None, Some(7)),
        ("incomplete", Some((7, "owner/repo")), None, None),
        ("signed out", Some((7, "owner/repo")), None, None),
        ("expired", Some((7, "owner/repo")), None, None),
    ] {
        let scratch = Scratch::new("mcp-draft-default");
        let server = Arc::new(FilingServer::default());
        let (mut brain, before) = filing_owner(&scratch, &server).await;
        if let Some((id, destination)) = default {
            committed_default(&mut brain, id, destination);
        }
        let generation = brain.account.github.machine().stamp().generation;
        match case {
            "removed from list" => {
                let _updates = brain.account.github.machine_mut().on(
                    GitHubInput::Repos {
                        generation,
                        result: Ok(Repositories {
                            names: Bounded::new(vec![RepoName::new("owner/other").unwrap()])
                                .unwrap(),
                            ids: Vec::new(),
                        }),
                    },
                    tokio::time::Instant::now(),
                );
            }
            "incomplete" => {
                let _updates = brain.account.github.machine_mut().on(
                    GitHubInput::Repos {
                        generation,
                        result: Err(ApiError::Http(HttpError::Connect)),
                    },
                    tokio::time::Instant::now(),
                );
            }
            "signed out" => {
                let _updates = brain.account.github.machine_mut().on(
                    GitHubInput::Op(GitHubOp::SignOut),
                    tokio::time::Instant::now(),
                );
            }
            "expired" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_hours(24)).await;
                assert_eq!(brain.account.github.machine().user(), None);
                tokio::time::resume();
            }
            _ => {}
        }
        let (core, build, launch, _, thread, _) = draft_launch(&["draft_issue"]);
        Store::open(&scratch.db())
            .unwrap()
            .begin_turn(thread, Some("MCP"), build.run, "ask")
            .unwrap();
        *brain.coordinator = core;
        let origin = McpOrigin {
            build,
            launch,
            key: CallKey("m:n:1".to_owned()),
        };
        let action = save_draft(&mut brain, origin, &scratch, before.draft.0 + 1).await;
        assert_eq!(action.repo.as_ref().map(RepoName::as_str), repo, "{case}");
        assert_eq!(
            action.account.as_ref().map(|user| user.id),
            account,
            "{case}"
        );
        assert_eq!(
            (action.stage, action.version, action.consumed),
            (Stage::Draft, StateVersion(1), None)
        );
        assert_no_filing_http(&server);
        brain.close().await;
    }
}

#[tokio::test]
async fn mcp_draft_binds_a_new_default_only_after_the_settings_commit() {
    use crate::runtime::settings::{self, Gate};
    let scratch = Scratch::new("mcp-draft-default-commit");
    let server = Arc::new(FilingServer::default());
    let (mut brain, before) = filing_owner(&scratch, &server).await;
    committed_default(&mut brain, 7, "owner/repo");
    let (core, build, launch, _, thread, _) = draft_launch(&["draft_issue"]);
    Store::open(&scratch.db())
        .unwrap()
        .begin_turn(thread, Some("MCP"), build.run, "ask")
        .unwrap();
    *brain.coordinator = core;
    let origin = |n| McpOrigin {
        build,
        launch,
        key: CallKey(format!("m:n:{n}")),
    };
    let held = brain.hold_config_write();
    let next = RepoName::new("owner/other").unwrap();
    brain
        .github_op(GitHubOp::SetDefaultRepo(next.clone()), None)
        .unwrap();
    assert!(brain.config_write.is_some());
    let (done, written) = oneshot::channel();
    held.send(Gate::WriteAndSignal(done)).unwrap();
    assert!(timeout(PATIENCE, written).await.unwrap().unwrap());
    assert_eq!(scratch.config().github_default().unwrap().repo, next);
    assert_eq!(
        brain.config.github_default().unwrap().repo.as_str(),
        "owner/repo"
    );
    let a = save_draft(&mut brain, origin(1), &scratch, before.draft.0 + 1).await;
    assert_eq!(a.repo.as_ref().map(RepoName::as_str), Some("owner/repo"));
    let done = timeout(PATIENCE, settings::written(&mut brain.config_write))
        .await
        .unwrap();
    brain.settings_done(done, None).unwrap();
    expect_reply(call_draft(&mut brain, origin(1)), (SAVED, false)).await;
    assert_eq!(
        Store::open(&scratch.db()).unwrap().action(a.draft).unwrap(),
        Some(a.clone())
    );
    let b = save_draft(&mut brain, origin(2), &scratch, a.draft.0 + 1).await;
    assert_eq!(b.repo, Some(next));
    assert_eq!(b.account.as_ref().map(|user| user.id), Some(7));
    assert_eq!(draft_totals(&scratch), (3, 3, 3, 2));
    assert_no_filing_http(&server);
    brain.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one framed bridge fixture carries both offer widths and the ACK boundary"
)]
async fn mcp_draft_raw_stdio_uses_only_the_bound_inventory_and_waits_for_ack() {
    let valid = r#"{"title":"Local","body":"body"}"#;
    for names in [&["draw"][..], &["draw", "draft_issue"][..]] {
        let scratch = Scratch::new("mcp-draft-stdio");
        let mut brain = crate::test_support::brain(&scratch, "");
        let (core, build, launch, gate, thread, offer) = draft_launch(names);
        Store::open(&scratch.db())
            .unwrap()
            .begin_turn(thread, Some("MCP"), build.run, "ask")
            .unwrap();
        *brain.coordinator = core;
        let job = crate::platform::Job::new().unwrap();
        let exe = std::env::current_exe().unwrap();
        let (calls, mut mailbox) = mpsc::channel(1);
        let until = tokio::time::Instant::now() + PATIENCE;
        let context = LaunchContext {
            job: &job,
            brain_exe: &exe,
            deadline: until,
            tools: offer.schemas(),
            gate,
            binding: Binding::default(),
            calls: Some(calls),
        };
        context.binding.bind(Some((build, until))).unwrap();
        let (listener, bridge) = tokio::io::duplex(1 << 20);
        let (lines, input) = mpsc::channel(2);
        let (_stdin, stdin_end) = watch::channel(false);
        let (out, mut written) = mpsc::channel(2);
        let steps = async {
            lines
                .send(BridgeInput::from_test_line(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .await
                .unwrap();
            let list = bridge_reply(&mut written).await;
            assert_eq!(list["id"], 1);
            let tools = list["result"]["tools"].as_array().unwrap();
            assert_eq!(
                tools
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                names
            );
            assert_eq!(
                tools[0]["inputSchema"]["$defs"]["Surface"],
                json!({"type":"string","const":"overlay"})
            );
            if let [_, draft] = tools.as_slice() {
                crate::test_support::assert_mcp_draft_schema(
                    &draft["description"],
                    &draft["inputSchema"],
                );
                for (n, arguments) in [
                    (2, r#"{"title":"A","title":"B","body":"body"}"#),
                    (2, valid),
                    (3, r#"{"title":"A","body":"first","body":"second"}"#),
                    (4, r#"{"title":"A","body":"body","repo":null}"#),
                ] {
                    lines.send(raw_call(n, arguments)).await.unwrap();
                    let call = mailbox.recv().await.unwrap();
                    assert_eq!(call.args.as_deref(), Some(arguments.as_bytes()));
                    brain.claude_done(Wake::Call(call), None).unwrap();
                    assert_eq!(
                        bridge_reply(&mut written).await,
                        reply_at(n, (ToolError::Invalid.text(), true))
                    );
                    assert_eq!(draft_totals(&scratch), (0, 0, 0, 0));
                }
                let release = brain.hold_store(|op| matches!(op, StoreOp::CommitCall(_)));
                lines.send(raw_call(5, valid)).await.unwrap();
                let call = mailbox.recv().await.unwrap();
                assert_eq!(call.origin.key, CallKey("m:n:5".to_owned()));
                brain.claude_done(Wake::Call(call), None).unwrap();
                assert!(matches!(
                    written.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(draft_totals(&scratch), (0, 0, 0, 0));
                release.send(()).unwrap();
                let ack = brain.inputs.recv().await.unwrap();
                assert!(matches!(
                    ack,
                    Input::CallCommitted {
                        outcome: Committed::Done,
                        ..
                    }
                ));
                assert_eq!(draft_totals(&scratch), (1, 1, 1, 1));
                let row = Store::open(&scratch.db())
                    .unwrap()
                    .action(DraftId(1))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    (row.thread, row.title.as_str(), row.body.as_str()),
                    (thread, "Local", "body")
                );
                assert_eq!(
                    (row.repo, row.account, row.stage),
                    (None, None, Stage::Draft)
                );
                assert!(matches!(
                    written.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(
                    brain.handle(ack).unwrap(),
                    [Effect::Control(ControlToShell::ThreadsChanged)]
                );
                assert_eq!(
                    bridge_reply(&mut written).await,
                    reply_at(5, (SAVED, false))
                );
                lines.send(raw_call(5, valid)).await.unwrap();
                brain
                    .claude_done(Wake::Call(mailbox.recv().await.unwrap()), None)
                    .unwrap();
                assert_eq!(
                    bridge_reply(&mut written).await,
                    reply_at(5, (SAVED, false))
                );
                assert_eq!(draft_totals(&scratch), (1, 1, 1, 1));
            } else {
                lines.send(raw_call(2, valid)).await.unwrap();
                assert_eq!(
                    bridge_reply(&mut written).await,
                    json!({
                        "jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params"}
                    })
                );
                assert!(matches!(
                    mailbox.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(draft_totals(&scratch), (0, 0, 0, 0));
            }
            lines.send(BridgeInput::End).await.unwrap();
        };
        let (end, relayed, ()) = timeout(PATIENCE, async {
            tokio::join!(
                session(listener, launch, &context),
                relay(bridge, input, stdin_end, out),
                steps,
            )
        })
        .await
        .unwrap();
        assert_eq!(end, SessionEnd::Closed);
        assert!(relayed.is_ok(), "{relayed:?}");
        brain.close().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one transaction and review session witness the full proposal-to-consent boundary"
)]
async fn mcp_draft_ack_is_durable_but_only_reviewed_consent_can_file() {
    let scratch = Scratch::new("mcp-draft-consent");
    let server = Arc::new(FilingServer::default());
    let (mut brain, reviewed) = reviewed_filing_owner(&scratch, &server).await;
    let (core, build, launch, gate, thread, _) = draft_launch(&["draft_issue"]);
    let mut store = Store::open(&scratch.db()).unwrap();
    store
        .begin_turn(thread, Some("MCP task"), build.run, "ask")
        .unwrap();
    drop(store);
    *brain.coordinator = core;
    let default = brain
        .config
        .stage_github_default(&DefaultRepo {
            account: 7,
            repo: RepoName::new("owner/repo").unwrap(),
        })
        .unwrap();
    default.write().unwrap();
    brain.config.commit(default);

    let session = brain.issue_session().unwrap();
    let send = |brain: &mut Brain, args: &[u8]| {
        let (reply, receiver) = oneshot::channel();
        brain
            .claude_done(
                Wake::Call(McpCall {
                    origin: McpOrigin {
                        build,
                        launch,
                        key: CallKey("m:n:1".to_owned()),
                    },
                    tool: ToolName::DraftIssue,
                    args: Some(args.to_vec()),
                    reply,
                }),
                None,
            )
            .unwrap();
        receiver
    };
    let draft = DraftId(reviewed.draft.0 + 1);
    let read = |draft| Store::open(&scratch.db()).unwrap().action(draft).unwrap();
    let release = brain.hold_store(|op| matches!(op, StoreOp::CommitCall(_)));
    let mut first = send(&mut brain, ARGS);
    let mut retry = send(&mut brain, ARGS);
    for answer in [&mut first, &mut retry] {
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
    assert_eq!(read(draft), None);
    release.send(()).unwrap();
    let ack = timeout(PATIENCE, brain.inputs.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        &ack,
        Input::CallCommitted {
            outcome: Committed::Done,
            then: Some(Then::IssueDrafted),
            ..
        }
    ));
    for answer in [&mut first, &mut retry] {
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
    let action = read(draft).unwrap();
    assert_eq!(action.thread, thread);
    assert_eq!(action.title.as_str(), "  Local café  ");
    assert_eq!(action.body.as_str(), "first\n\tcafé\n");
    assert_eq!(
        action.repo.as_ref().map(RepoName::as_str),
        Some("owner/repo")
    );
    assert_eq!(action.account.as_ref().map(|user| user.id), Some(7));
    assert_eq!(action.stage, Stage::Draft);
    assert_eq!(action.version, StateVersion(1));
    assert_eq!(action.approval, Some(ApprovalRev(3)));
    assert_eq!(action.consumed, None);
    assert_eq!(
        brain.handle(ack).unwrap(),
        [Effect::Control(ControlToShell::ThreadsChanged)],
    );
    expect_reply(first, (SAVED, false)).await;
    expect_reply(retry, (SAVED, false)).await;
    expect_reply(send(&mut brain, ARGS), (SAVED, false)).await;

    let consent = |action: &Action| IssueOp::Consent {
        draft: action.draft,
        hash: action.hash(),
        approval: action.approval.unwrap(),
        decision: Decision::File,
    };
    assert_eq!(
        brain
            .issue_op(session, 1, &consent(&action), Some(brain.epoch))
            .unwrap(),
        [Effect::Control(ControlToShell::IssueUnavailable {
            kind: IssueUnavailableKind::Action,
            request: 1,
            draft,
        })],
    );
    assert_eq!(brain.filing.review_counts(), (1, 0));
    assert_eq!(brain.filing.deadline_for_test(), None);
    assert_eq!(read(reviewed.draft), Some(reviewed.clone()));
    assert_eq!(server.factories.load(Ordering::SeqCst), 0);
    assert_eq!(server.gets.load(Ordering::SeqCst), 0);
    assert_eq!(server.posts.load(Ordering::SeqCst), 0);
    let counts: (i64, i64, i64, i64, String) = rusqlite::Connection::open(scratch.db())
        .unwrap().query_row(
            "SELECT (SELECT COUNT(*) FROM actions),
                    (SELECT last FROM approval_mark),
                    (SELECT COUNT(*) FROM events WHERE kind = 'draft'),
                    (SELECT COUNT(*) FROM tool_calls),
                    (SELECT result FROM tool_calls WHERE tool = 'draft_issue' AND route LIKE 'mcp:%')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).unwrap();
    assert_eq!(counts, (2, 3, 2, 1, format!(r#"{{"text":"{SAVED}"}}"#)));

    let effects = brain
        .issue_op(session, 2, &consent(&reviewed), Some(brain.epoch))
        .unwrap();
    brain.apply(effects, None).unwrap();
    storage::settle(&mut brain).await;
    let filed = Action {
        version: StateVersion(5),
        stage: Stage::Filed {
            number: NonZeroU64::new(17).unwrap(),
            confirmed: false,
            maybe_duplicate: false,
        },
        approval: None,
        consumed: reviewed.approval,
        ..reviewed
    };
    expect_reply(send(&mut brain, ARGS), (SAVED, false)).await;
    let changed = br#"{"title":"Changed","body":"b"}"#;
    expect_reply(send(&mut brain, changed), (ToolError::Reused.text(), true)).await;
    gate.fail();
    expect_reply(send(&mut brain, ARGS), (ToolError::Ended.text(), true)).await;
    assert_eq!(read(filed.draft), Some(filed));
    assert_eq!(read(draft), Some(action));
    assert_eq!(brain.filing.review_counts(), (0, 0));
    assert_eq!(server.factories.load(Ordering::SeqCst), 1);
    assert_eq!(server.gets.load(Ordering::SeqCst), 4);
    assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    brain.close().await;
}
