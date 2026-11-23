//! Claude Code's MCP calls through the brain: the offer a build's launch may carry, the core's
//! authorization of a live launch, and one reply per call, on its write's acknowledgement.

use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::ThreadId;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use super::Held;
use crate::bridge::listener::{
    Binding, LaunchContext, McpCall, McpOrigin, McpReply, SessionEnd, session,
};
use crate::bridge::{CallKey, LaunchId, Method, ToBrain, ToBridge};
use crate::context::{Built, ModelRequest, Origin, ToolSchema};
use crate::coordinator::{BuildKey, ContextBuilt, Effect, Input, SendKey, StoreOp};
use crate::dispatch::{Caller, Committed, Route, ToolError};
use crate::engines::ToolName;
use crate::engines::claude_code::lane::tests::V;
use crate::engines::claude_code::launch::{Bind, Offer};
use crate::engines::claude_code::supervisor::{self, Launch, Question, Start};
use crate::runtime::Brain;
use crate::test_support::Scratch;

/// The thread the test brain's turn runs in.
const THREAD: ThreadId = ThreadId(2);

#[test]
fn a_cloned_offer_reuses_the_canonical_schema_set_without_copying() {
    let request = [ToolSchema {
        name: "draw",
        description: "untrusted request description",
        parameters: json!({"type":"untrusted"}),
    }];
    let offer = Offer::authorized(Origin::Kumo, &request);
    let copy = offer.clone();
    let [schema] = offer.schemas() else {
        panic!("exactly one canonical schema");
    };
    assert_eq!(schema.name, "draw");
    assert_eq!(
        schema.parameters["$defs"]["Surface"],
        json!({"type":"string","const":"overlay"})
    );
    let description_digest: [u8; 32] =
        Sha256::digest(schema.description.replace("\r\n", "\n")).into();
    assert_eq!(
        description_digest,
        [
            0xf8, 0xa0, 0x2e, 0x38, 0xb1, 0x7b, 0x03, 0x1d, 0x6c, 0x77, 0x07, 0x50, 0x43, 0xe6,
            0x37, 0x06, 0xf4, 0xbe, 0xcc, 0x63, 0x24, 0xa6, 0x26, 0x9d, 0x36, 0xd3, 0xc5, 0xbb,
            0x59, 0x7b, 0x11, 0x52,
        ],
        "canonical overlay description, with LF line endings"
    );
    assert!(std::ptr::eq(offer.schemas(), copy.schemas()));
}

#[test]
fn a_launch_offers_only_what_its_build_offers_and_mcp_permits() {
    let kumo = crate::context::tools(Origin::Kumo);
    for (without, expected) in [
        (&[][..], &["draw", "draft_issue"][..]),
        (&["draw"][..], &["draft_issue"][..]),
        (&["draft_issue"][..], &["draw"][..]),
        (&["draw", "draft_issue"][..], &[][..]),
    ] {
        let built: Vec<_> = kumo
            .iter()
            .filter(|tool| !without.contains(&tool.name))
            .cloned()
            .collect();
        let offer = Offer::authorized(Origin::Kumo, &built);
        assert_eq!(
            offer
                .schemas()
                .iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(offer == Offer::Nothing, expected.is_empty());
    }
}

/// Starts a turn and admits its build, offering every tool; the build's key.
fn running(brain: &mut Brain) -> BuildKey {
    brain
        .coordinator
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale");
    let run = brain.coordinator.current_run().expect("a current run");
    let effects = brain
        .coordinator
        .handle(Input::TurnStored { run })
        .expect("current");
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("a build");
    brain
        .coordinator
        .handle(Input::ContextBuilt(ContextBuilt {
            key,
            snapshot: None,
            result: Ok(Built {
                request: ModelRequest {
                    choice: None,
                    instructions: "built".to_owned(),
                    input: Vec::new(),
                    tools: crate::dispatch::schemas(Origin::Kumo, Route::Model),
                },
                sources: Vec::new(),
            }),
        }))
        .expect("admitted");
    key
}

/// A launch the lane holds, live while both halves are, its audit passed.
fn live(brain: &mut Brain) -> (Start, Launch) {
    let (start, launch) = seeded(brain);
    launch.gate().pass();
    (start, launch)
}

/// A launch the lane holds as a fresh one is: its post-seed audit still to pass.
fn seeded(brain: &mut Brain) -> (Start, Launch) {
    let (start, launch) = supervisor::admit(Question {
        thread: THREAD,
        model: "claude-test".to_owned(),
        effort: None,
        tools: std::sync::Arc::default(),
        lifetime: Duration::from_secs(60),
        calls: None,
    })
    .expect("a launch id");
    brain.claude.launches.push(Held {
        thread: THREAD,
        id: launch.id(),
        control: start.control(),
        generation: brain.claude.generation,
        version: V,
        due: false,
        replaced: false,
        bound: false,
    });
    (start, launch)
}

fn bind(brain: &mut Brain, build: BuildKey, launch: LaunchId) -> bool {
    bind_until(
        brain,
        build,
        launch,
        Instant::now() + Duration::from_secs(60),
    )
}

/// Binds as the launcher would: offering the probe, for the turn's thread, until `until`.
fn bind_until(brain: &mut Brain, build: BuildKey, launch: LaunchId, until: Instant) -> bool {
    let (reply, mut bound) = oneshot::channel();
    brain.mcp_bind(Bind {
        build,
        launch,
        offer: Offer::for_tests(Origin::Task(THREAD), vec![ToolName::Probe]),
        until,
        reply,
    });
    bound.try_recv().expect("answered at once").is_some()
}

fn call(build: BuildKey, launch: LaunchId, n: u32) -> (McpCall, oneshot::Receiver<McpReply>) {
    let (reply, answer) = oneshot::channel();
    let call = McpCall {
        origin: McpOrigin {
            build,
            launch,
            key: CallKey(format!("m:n:{n}")),
        },
        tool: ToolName::Probe,
        args: Some(br#"{"word":"hi"}"#.to_vec()),
        reply,
    };
    (call, answer)
}

fn committed(build: BuildKey, launch: LaunchId, n: u32) -> Input {
    let caller = Caller::Mcp {
        run: build.run,
        launch,
        key: CallKey(format!("m:n:{n}")),
    };
    Input::CallCommitted {
        key: caller.key(),
        outcome: Committed::Done,
        then: None,
    }
}

#[tokio::test]
async fn a_call_is_answered_on_its_acknowledgement_and_a_stale_one_is_let_go() {
    let scratch = Scratch::new("mcp-brain");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (_start, launch) = live(&mut brain);
    let id = launch.id();
    assert!(
        !bind(&mut brain, build, LaunchId(7)),
        "a launch the lane doesn't hold"
    );
    let (revoked, gone) = live(&mut brain);
    revoked.control().withdraw();
    assert!(!bind(&mut brain, build, gone.id()), "a revoked launch");
    assert!(bind(&mut brain, build, id));

    let (refused, mut at_once) = call(build, LaunchId(7), 1);
    brain.mcp_call(refused, None).expect("no link");
    let reply = at_once.try_recv().expect("answered at once");
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (ToolError::Ended.text(), true)
    );

    let (first, mut answer) = call(build, id, 1);
    brain.mcp_call(first, None).expect("no link");
    assert!(
        matches!(answer.try_recv(), Err(TryRecvError::Empty)),
        "not before its write"
    );
    let (gone, nobody) = call(build, id, 2);
    brain.mcp_call(gone, None).expect("no link");
    drop(nobody);
    assert!(brain.handle(committed(build, id, 1)).is_some());
    let reply = answer.try_recv().expect("answered on the acknowledgement");
    assert_eq!((reply.text.as_str(), reply.error), ("probed hi", false));
    assert!(
        brain.claude.waiting.is_empty(),
        "a call nobody waits for is let go"
    );

    let (stale, mut lost) = call(build, id, 3);
    brain.mcp_call(stale, None).expect("no link");
    assert_eq!(brain.claude.waiting.len(), 1);
    brain.handle(Input::ShellLost);
    assert!(
        matches!(lost.try_recv(), Err(TryRecvError::Closed)),
        "its run ended, so it's let go unanswered"
    );
    assert!(brain.claude.waiting.is_empty());
}

#[tokio::test]
async fn arguments_over_the_cap_are_refused_by_size_and_stay_refused() {
    let scratch = Scratch::new("mcp-over-cap");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (_start, launch) = live(&mut brain);
    let id = launch.id();
    assert!(bind(&mut brain, build, id));
    let (mut over, mut refused) = call(build, id, 1);
    over.args = None;
    brain.mcp_call(over, None).expect("no link");
    let reply = refused.try_recv().expect("refused at once");
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (ToolError::TooLarge.text(), true)
    );
    let (retry, mut again) = call(build, id, 1);
    brain.mcp_call(retry, None).expect("no link");
    let reply = again.try_recv().expect("the refusal stays");
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (ToolError::TooLarge.text(), true)
    );
    assert!(brain.claude.waiting.is_empty(), "nothing waits on a write");
}

#[tokio::test]
async fn a_call_held_across_withdrawal_or_expiry_starts_nothing_and_no_reply_follows() {
    for case in ["withdrawn", "replaced", "expired", "failed its audit"] {
        let scratch = Scratch::new("mcp-held");
        let mut brain = crate::test_support::brain(&scratch, "");
        let build = running(&mut brain);
        let (start, launch) = live(&mut brain);
        let id = launch.id();
        let until =
            Instant::now() + Duration::from_millis(if case == "expired" { 500 } else { 60_000 });
        assert!(bind_until(&mut brain, build, id, until));
        let (cached, mut answer) = call(build, id, 1);
        brain.mcp_call(cached, None).expect("no link");
        assert!(brain.handle(committed(build, id, 1)).is_some());
        assert_eq!(answer.try_recv().expect("answered").text, "probed hi");
        let (pending, mut late) = call(build, id, 2);
        brain.mcp_call(pending, None).expect("no link");
        assert_eq!(brain.claude.waiting.len(), 1);

        // Held in the mailbox while only the launch loses its authority; its build stays current.
        let held = [call(build, id, 3), call(build, id, 1)];
        match case {
            "withdrawn" => {
                start.control().withdraw();
            }
            "replaced" => brain.claude.replaced(id),
            "expired" => tokio::time::sleep_until(until.into()).await,
            _ => launch.gate().fail(),
        }
        assert_eq!(brain.coordinator.current_run(), Some(build.run));
        for (call, mut answer) in held {
            brain.mcp_call(call, None).expect("no link");
            let reply = answer
                .try_recv()
                .expect("refused at once, never run or replayed");
            assert_eq!(
                (reply.text.as_str(), reply.error),
                (ToolError::Ended.text(), true),
                "{case}"
            );
        }
        brain.handle(committed(build, id, 2));
        assert!(
            matches!(late.try_recv(), Err(TryRecvError::Closed)),
            "no reply leaves for a launch that lost its authority"
        );
        assert!(brain.claude.waiting.is_empty());
    }
}

#[tokio::test]
async fn a_call_nobody_waits_for_any_more_starts_nothing() {
    let scratch = Scratch::new("mcp-unwaited");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (_start, launch) = live(&mut brain);
    let id = launch.id();
    assert!(bind(&mut brain, build, id));
    let (gone, nobody) = call(build, id, 1);
    drop(nobody);
    brain.mcp_call(gone, None).expect("no link");
    brain.handle(committed(build, id, 1));
    let (again, mut answer) = call(build, id, 1);
    brain.mcp_call(again, None).expect("no link");
    assert!(
        matches!(answer.try_recv(), Err(TryRecvError::Empty)),
        "the key's first run waits on its write; nothing earlier ran for it to replay"
    );
}

/// Production's order: bound while its audit is pending, the seed's send granted, then early
/// calls refused at once and uncharged with the binding kept; once the audit passes the same key
/// runs fresh, and after a failure a late pass reopens nothing.
#[tokio::test]
async fn a_launch_bound_before_its_audit_takes_no_work_until_it_passes_and_a_failure_stays() {
    let scratch = Scratch::new("mcp-pending");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (_start, launch) = seeded(&mut brain);
    let id = launch.id();
    assert!(
        bind(&mut brain, build, id),
        "bound while its audit is pending"
    );
    let granted = brain
        .coordinator
        .handle(Input::SendReady {
            key: SendKey::Build(build),
            hold: None,
        })
        .expect("current");
    assert!(
        granted
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_))),
        "its seed's send is granted"
    );
    for n in 1..=17 {
        let (early, mut answer) = call(build, id, n);
        brain.mcp_call(early, None).expect("no link");
        let reply = answer.try_recv().expect("refused at once");
        assert_eq!(
            (reply.text.as_str(), reply.error),
            (ToolError::Ended.text(), true),
            "call {n}"
        );
    }
    assert_eq!(
        brain.coordinator.current_run(),
        Some(build.run),
        "no budget end"
    );
    assert!(brain.claude.waiting.is_empty());

    launch.gate().pass();
    let (first, mut answer) = call(build, id, 1);
    brain.mcp_call(first, None).expect("no link");
    assert!(brain.handle(committed(build, id, 1)).is_some());
    assert_eq!(
        answer.try_recv().expect("answered").text,
        "probed hi",
        "still bound, and the early refusal wasn't kept for its key"
    );

    launch.gate().fail();
    launch.gate().pass();
    let (late, mut refused) = call(build, id, 2);
    brain.mcp_call(late, None).expect("no link");
    let reply = refused.try_recv().expect("refused at once");
    assert_eq!(
        (reply.text.as_str(), reply.error),
        (ToolError::Ended.text(), true)
    );
}

/// The bridge's side of a listener session: reads `Hello`, then asks for the probe as call 1.
async fn ask(pipe: &mut tokio::io::DuplexStream) {
    let hello = crate::ipc::read::<ToBridge>(pipe, Pipe::Data).await;
    assert!(matches!(hello, Ok(Some(ToBridge::Hello { .. }))), "hello");
    let request = ToBrain::Request {
        key: CallKey("m:n:1".to_owned()),
        method: Method::ToolsCall,
        chunk: br#"{"name":"probe","arguments":{"word":"hi"}}"#.to_vec(),
        last: true,
    };
    let frame = frame::encode(&request, Pipe::Data).expect("encodes");
    pipe.write_all(&frame).await.expect("written");
}

/// A content reply already queued for the listener, then a transition in the core alone that
/// ends the run, with nothing failing the transport separately: the listener lets none of it
/// leave.
#[tokio::test]
async fn a_reply_queued_before_a_core_only_revocation_never_leaves_the_listener() {
    let scratch = Scratch::new("mcp-queued");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (_start, launch) = live(&mut brain);
    let id = launch.id();
    let until = Instant::now() + Duration::from_secs(60);
    assert!(bind_until(&mut brain, build, id, until));
    let tools = [ToolSchema {
        name: "probe",
        description: "Echoes a word.",
        parameters: json!({"type": "object"}),
    }];
    let (calls, mut mailbox) = tokio::sync::mpsc::channel(1);
    let context = LaunchContext {
        deadline: until.into(),
        tools: &tools,
        gate: launch.gate().clone(),
        binding: Binding::default(),
        calls: Some(calls),
    };
    context
        .binding
        .bind(Some((build, until.into())))
        .expect("first bind");
    let (ours, mut theirs) = tokio::io::duplex(1 << 20);
    let drive = async move {
        ask(&mut theirs).await;
        let call = mailbox.recv().await.expect("admitted");
        brain.mcp_call(call, None).expect("no link");
        assert!(brain.handle(committed(build, id, 1)).is_some());
        assert!(brain.claude.waiting.is_empty(), "its answer is queued");
        brain
            .coordinator
            .handle(Input::Stop {
                run: build.run,
                revealed: Vec::new(),
            })
            .expect("current");
        let reply = crate::ipc::read::<ToBridge>(&mut theirs, Pipe::Data).await;
        let Ok(Some(ToBridge::Reply {
            chunk,
            last: true,
            error: false,
            ..
        })) = reply
        else {
            panic!("one whole reply");
        };
        let body: Value = serde_json::from_slice(&chunk).expect("json");
        let ended = "This call ended before it finished.";
        assert_eq!(
            body,
            json!({"content": [{"type": "text", "text": ended}], "isError": true})
        );
    };
    let (end, ()) = tokio::join!(session(ours, id, &context), drive);
    assert_eq!(end, SessionEnd::Closed);
}

/// A launch withdrawn while a call of it waits has that call let go as soon as Claude Code's
/// state is reported to the core, not at some later brain step.
#[tokio::test]
async fn a_withdrawn_launchs_waiter_goes_when_claude_code_is_next_reported() {
    let scratch = Scratch::new("mcp-reported");
    let mut brain = crate::test_support::brain(&scratch, "");
    let build = running(&mut brain);
    let (start, launch) = live(&mut brain);
    let id = launch.id();
    assert!(bind(&mut brain, build, id));
    let (waiting, mut answer) = call(build, id, 1);
    brain.mcp_call(waiting, None).expect("no link");
    assert_eq!(brain.claude.waiting.len(), 1);
    start.control().withdraw();
    brain.claude_reported(None).expect("no shell");
    assert!(brain.claude.waiting.is_empty());
    assert!(matches!(answer.try_recv(), Err(TryRecvError::Closed)));
}
