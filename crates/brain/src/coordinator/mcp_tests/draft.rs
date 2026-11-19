use super::*;
use crate::dispatch::{CommitCall, LocalWrite, Then};
use kumochi_protocol::message::ControlToShell;

const PROPOSAL: &[u8] = br#"{"title":"Local draft","body":"first\r\nsecond"}"#;
const REORDERED: &[u8] = br#" { "body": "first\r\nsecond", "title": "Local draft" } "#;
const SAVED: &str = "Saved a local issue draft for user review. Nothing was filed on GitHub.";

fn test_offer() -> Offer {
    Offer::Mcp(Origin::Task(T), vec![ToolName::DraftIssue])
}

pub(crate) fn draft_launch() -> (Coordinator, BuildKey, LaunchId, AuditGate, ThreadId) {
    let (mut core, build) = offering(drawing());
    let (gate, until) = passed();
    assert!(binds(
        &mut core,
        (build, L1, T),
        test_offer(),
        (gate.clone(), until),
    ));
    (core, build, L1, gate, T)
}

fn draft(
    core: &mut Coordinator,
    at: (BuildKey, LaunchId, u32),
    args: &[u8],
) -> (Dispatch, Vec<Effect>) {
    mcp(core, at, ToolName::DraftIssue, args)
}

fn refused(error: ToolError) -> (Dispatch, Vec<Effect>) {
    (Dispatch::Answered(ToolOutput::Error(error)), Vec::new())
}

fn proposal(core: &mut Coordinator, at: (BuildKey, LaunchId, u32), args: &[u8]) -> CommitCall {
    let (result, effects) = draft(core, at, args);
    assert_eq!(result, Dispatch::Committing);
    let [Effect::Store(StoreOp::CommitCall(call))] = effects.as_slice() else {
        panic!("expected one local write, got {effects:?}");
    };
    assert_eq!(call.then, Some(Then::IssueDrafted));
    call.clone()
}

#[test]
fn mcp_draft_uses_a_closed_envelope_and_a_route_specific_retry_hash() {
    let (mut core, build, launch, _, _) = draft_launch();
    for (i, args) in [
        br#"{"title":"a","body":"b","repo":"owner/repo"}"#.as_slice(),
        br#"{"title":"a","body":"b","repo":"owner/unlisted"}"#,
        br#"{"title":"a","body":"b","repo":null}"#,
        br#"{"title":"a","body":"b","approval":1}"#,
        br#"{"title":"a","title":"b","body":"c"}"#,
        br#"{"title":"a","body":"b","body":"c"}"#,
        br#"{"title":"a","body":1}"#,
        br#"{"title":"a"}"#,
        br#"{"title":"a","body":"b""#,
        br#"["a","b"]"#,
        br#"{"title":"a","body":"b\u202e"}"#,
    ]
    .into_iter()
    .enumerate()
    {
        let at = (build, launch, u32::try_from(i).unwrap());
        assert_eq!(draft(&mut core, at, args), refused(ToolError::Invalid));
        assert_eq!(draft(&mut core, at, PROPOSAL), refused(ToolError::Invalid));
    }
    let mut padded = PROPOSAL.to_vec();
    padded.resize(crate::dispatch::MAX_ARGS, b' ');
    let at = (build, launch, 12);
    let call = proposal(&mut core, at, &padded);
    let LocalWrite::DraftIssue {
        title,
        body,
        repo,
        account,
    } = &call.write
    else {
        panic!("draft write");
    };
    assert_eq!(title.as_str(), "Local draft");
    assert_eq!(body.as_str(), "first\nsecond");
    assert_eq!((repo, account), (&None, &None));
    let debug = format!("{call:?}");
    assert!(!debug.contains("Local draft"));
    assert!(!debug.contains(&format!("{:?}", b"Local draft")));
    assert_eq!(
        draft(&mut core, at, REORDERED),
        (Dispatch::Joined, Vec::new())
    );
    assert_eq!(
        draft(
            &mut core,
            at,
            br#"{"title":"Edited","body":"first\r\nsecond"}"#
        ),
        refused(ToolError::Reused)
    );
    assert_eq!(
        draft(
            &mut core,
            at,
            br#"{"title":"Local draft","body":"first\r\nsecond","repo":null}"#
        ),
        refused(ToolError::Reused)
    );
    padded.push(b' ');
    assert_eq!(draft(&mut core, at, &padded), refused(ToolError::Reused));
    assert_eq!(
        draft(&mut core, (build, launch, 13), &padded),
        refused(ToolError::TooLarge)
    );
    assert_eq!(core.take_mcp_answer(build, launch, &key(12)), Ok(None));
}

#[test]
fn mcp_drafts_share_the_run_limit_and_replays_do_not_charge() {
    for direct in [0, 1] {
        let (mut core, build, launch, _, _) = draft_launch();
        if direct == 1 {
            assert_eq!(
                streamed(
                    &mut core,
                    TurnEvent::ToolCallStarted {
                        call: CallId::new("model-draft").unwrap(),
                        name: ToolName::DraftIssue,
                    }
                ),
                [],
            );
        }
        for n in 1..=(16 - direct) {
            let at = (build, launch, n);
            proposal(&mut core, at, PROPOSAL);
            assert_eq!(
                draft(&mut core, at, REORDERED),
                (Dispatch::Joined, Vec::new())
            );
        }
        assert_eq!(
            draft(&mut core, (build, launch, 17 - direct), PROPOSAL),
            (ended(), super::super::ends_failed(Failure::Budget))
        );
    }
}

#[test]
fn a_different_mcp_tool_cannot_replay_a_cached_draw_refusal() {
    let (mut core, build) = offering(drawing());
    let offer = Offer::Mcp(Origin::Task(T), vec![ToolName::Draw, ToolName::DraftIssue]);
    assert!(binds(&mut core, (build, L1, T), offer, passed()));
    let at = (build, L1, 1);
    let origin = McpOrigin {
        build,
        launch: L1,
        key: key(1),
    };
    let box_target = args(Some(json!({
        "id":"s3",
        "put":[{"highlight":{"id":"c","target":{"box":[0,0,10,10]},"style":"box"}}]
    })));
    let draw_error = "drawn: []; stopped: step s3: put[0].target: scene.Unsupported; here only the overlay can be drawn on, without box targets";
    let mut call = |tool, input: &[u8], expected| {
        let (dispatch, effects) = mcp(&mut core, at, tool, input);
        let Dispatch::Answered(output) = dispatch else {
            panic!("expected an immediate refusal, got {dispatch:?}");
        };
        assert_eq!(effects, [], "no write or other effect");
        let reply = core.mcp_reply(&origin, &output);
        assert_eq!((reply.text.as_str(), reply.error), (expected, true));
    };
    call(ToolName::Draw, &box_target, draw_error);
    call(
        ToolName::DraftIssue,
        br#"{"title":"Local draft","body":"Draft body","repo":null}"#,
        "call id already used",
    );
    call(ToolName::Draw, &args(None), draw_error);
    for n in 2..=16 {
        proposal(&mut core, (build, L1, n), PROPOSAL);
    }
    assert_eq!(
        draft(&mut core, (build, L1, 17), PROPOSAL),
        (ended(), super::super::ends_failed(Failure::Budget)),
    );
}

#[test]
fn revoking_an_mcp_draft_launch_drops_its_reply_but_not_its_committed_card() {
    for stop in ["esc", "rebuild", "bridge"] {
        let scratch = Scratch::new(stop);
        let mut store = store(&scratch);
        let (mut core, build, launch, gate, thread) = draft_launch();
        let at = (build, launch, 1);
        let call = proposal(&mut core, at, PROPOSAL);
        match stop {
            "esc" => {
                core.handle(Input::Stop {
                    run: R,
                    revealed: Vec::new(),
                })
                .unwrap();
            }
            "rebuild" => core.build(R, 1, &mut Vec::new()),
            _ => gate.fail(),
        }
        assert!(!gate.may_call());
        assert_eq!(draft(&mut core, at, REORDERED), refused(ToolError::Ended));
        assert_eq!(
            draft(&mut core, (build, launch, 2), PROPOSAL),
            refused(ToolError::Ended)
        );
        assert_eq!(store.commit_call(&call).unwrap(), Committed::Done);
        assert_eq!(
            core.handle(Input::CallCommitted {
                key: call.key,
                outcome: Committed::Done,
                then: call.then
            }),
            Ok(vec![Effect::Control(ControlToShell::ThreadsChanged)]),
        );
        assert_eq!(
            core.take_mcp_answer(build, launch, &key(1)),
            Err(super::super::Stale)
        );
        let origin = McpOrigin {
            build,
            launch,
            key: key(1),
        };
        let reply = core.mcp_reply(&origin, &ToolOutput::Text(SAVED.to_owned()));
        assert_eq!(
            (reply.text.as_str(), reply.error),
            (ToolError::Ended.text(), true)
        );
        let card = Store::open(&scratch.db())
            .unwrap()
            .action(kumochi_protocol::message::DraftId(1))
            .unwrap()
            .unwrap();
        assert_eq!(card.thread, thread);
        assert_eq!(card.title.as_str(), "Local draft");
        if stop == "bridge" {
            assert!(binds(&mut core, (build, L2, T), test_offer(), passed()));
            let next = proposal(&mut core, (build, L2, 1), PROPOSAL);
            assert_eq!(next.key.route, Route::Mcp(L2));
            assert_eq!(core.take_mcp_answer(build, L2, &key(1)), Ok(None));
        }
    }
}
