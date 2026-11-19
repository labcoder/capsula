use super::*;
use crate::bridge::CallKey;
use crate::bridge::listener::{McpCall, McpOrigin, McpReply};
use crate::connections::github::device::DefaultRepo;
use crate::coordinator::{Effect, Input, draft_launch};
use crate::dispatch::{Committed, Then, ToolError};
use crate::engines::ToolName;
use crate::runtime::claude::Wake;
use kumochi_protocol::message::DraftId;
use tokio::sync::oneshot;

const ARGS: &[u8] = r#"{"title":"  Local café  ","body":"first\r\n\tcafé\r\n"}"#.as_bytes();
const SAVED: &str = "Saved a local issue draft for user review. Nothing was filed on GitHub.";

async fn expect_reply(reply: oneshot::Receiver<McpReply>, expected: (&str, bool)) {
    let reply = timeout(PATIENCE, reply).await.unwrap().unwrap();
    assert_eq!((reply.text.as_str(), reply.error), expected);
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
    let (core, build, launch, gate, thread) = draft_launch();
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
