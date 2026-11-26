use super::super::engine;
use super::*;
use crate::dispatch::ReadFinished;
use crate::engines::{Finish, TurnEvent};
use crate::recall::Selection;

#[test]
fn storage_and_handler_errors_stay_storage_failures_after_the_completion_deadline() {
    for error in [crate::recall::Error::Read, crate::recall::Error::Handler] {
        let mut core = kumo_rendered_b();
        let call = start_read(&mut core);
        *call.control.test.time.lock().unwrap() = Some(call.control.end);
        let mut done = empty_result(&call);
        done.result = Err(error);
        let effects = core.handle(Input::ReadFinished(done.clone())).unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Store(StoreOp::EndRun {
                end: RunEnd::Failed(Failure::Store),
                ..
            })
        )));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::CommitCall(_))))
        );
        assert_eq!(core.current_run(), None);
        assert_eq!(core.handle(Input::ReadFinished(done)), Err(Stale));
    }
}

#[test]
fn a_newly_selected_source_is_fenced_even_when_the_calling_build_did_not_render_it() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(KUMO),
        text: "hi".into(),
    })
    .unwrap();
    core.handle(Input::TurnStored { run: R }).unwrap();
    core.handle(rendering(build_key(1), Origin::Kumo, vec![KUMO]))
        .unwrap();
    let call = start_read(&mut core);
    let mut done = empty_result(&call);
    done.result = Ok((
        Selection::Search {
            query: "needle".into(),
            hits: vec![crate::recall::Hit { task: 2, event: 10 }],
            more: false,
        },
        vec![B],
    ));
    delete_b(&mut core);
    assert_eq!(
        core.handle(Input::ReadFinished(done.clone())),
        Ok(vec![Effect::CancelEngine { run: R }])
    );
    assert_eq!(core.handle(Input::ReadFinished(done)), Ok(Vec::new()));
    assert!(!core.current(R).unwrap().tools.calling());
    assert_eq!(call.control.check(), Err(crate::recall::Error::Ended));
}

#[test]
fn a_finishing_run_or_a_departed_caller_cannot_persist_a_late_read() {
    for finishing in [false, true] {
        let mut core = kumo_rendered_b();
        let call = start_read(&mut core);
        if finishing {
            core.current(R).unwrap().finishing = true;
        } else {
            core.current(R).unwrap().tools.drop_round();
        }
        assert_eq!(
            core.handle(Input::ReadFinished(empty_result(&call))),
            Ok(Vec::new())
        );
        core.current(R).unwrap().finishing = false;
        let mut effects = Vec::new();
        assert_eq!(
            core.tool_call(
                model(R, "history"),
                ToolName::SearchHistory,
                br#"{"query":"needle"}"#,
                &mut effects
            ),
            ended()
        );
        assert_eq!(effects, Vec::new());
        assert_eq!(
            core.handle(Input::ReadFinished(empty_result(&call))),
            Ok(Vec::new())
        );
    }
}

#[test]
fn a_replacement_build_refuses_the_old_key_before_its_read_completes() {
    let mut core = kumo_rendered_b();
    let call = start_read(&mut core);
    delete_b(&mut core);
    let mut effects = Vec::new();
    assert_eq!(
        core.tool_call(model(R, "other"), ToolName::Probe, ARGS, &mut effects),
        ended()
    );
    assert_eq!(effects, vec![Effect::CancelEngine { run: R }]);
    assert!(!core.current(R).unwrap().tools.calling());
    core.handle(b_hidden()).unwrap();
    core.handle(rendering(build_key(2), Origin::Kumo, vec![KUMO]))
        .unwrap();
    let mut effects = Vec::new();
    assert_eq!(
        core.tool_call(
            model(R, "history"),
            ToolName::SearchHistory,
            br#"{"query":"needle"}"#,
            &mut effects
        ),
        ended()
    );
    assert_eq!(effects, Vec::new());
    assert_eq!(
        core.handle(Input::ReadFinished(empty_result(&call))),
        Ok(Vec::new())
    );
}

#[test]
fn an_expired_completion_is_a_terminal_timeout_not_a_commit() {
    let mut core = kumo_rendered_b();
    let call = start_read(&mut core);
    *call.control.test.time.lock().unwrap() = Some(call.control.end);
    let effects = core
        .handle(Input::ReadFinished(empty_result(&call)))
        .unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a timeout answer: {effects:?}");
    };
    assert_eq!(
        round.calls,
        vec![(
            CallId::new("history").unwrap(),
            ToolOutput::Error(ToolError::Timeout)
        )]
    );
    assert_eq!(
        core.handle(Input::ReadFinished(empty_result(&call))),
        Ok(Vec::new())
    );
}

fn start_read(core: &mut Coordinator) -> crate::dispatch::ReadCall {
    let call = CallId::new("history").unwrap();
    for event in [
        TurnEvent::ToolCallStarted {
            call: call.clone(),
            name: ToolName::SearchHistory,
        },
        TurnEvent::ToolCallReady {
            call,
            args: r#"{"query":"needle"}"#.into(),
        },
    ] {
        assert_eq!(
            core.handle(engine(super::super::engine_key(core, R), event)),
            Ok(Vec::new())
        );
    }
    let effects = core
        .handle(engine(
            super::super::engine_key(core, R),
            TurnEvent::Finished(Finish::Completed),
        ))
        .unwrap();
    let [Effect::Store(StoreOp::ReadCall(call))] = effects.as_slice() else {
        panic!("one read, no commit: {effects:?}");
    };
    call.clone()
}

fn empty_result(call: &crate::dispatch::ReadCall) -> ReadFinished {
    ReadFinished {
        key: call.key.clone(),
        build: call.build,
        result: Ok((
            Selection::Search {
                query: "needle".into(),
                hits: Vec::new(),
                more: false,
            },
            Vec::new(),
        )),
    }
}

#[test]
fn an_async_read_commits_only_after_completion_and_answers_only_after_commit() {
    let mut core = kumo_rendered_b();
    let call = start_read(&mut core);
    assert_eq!(core.current(R).unwrap().answered, Vec::new());
    let effects = core
        .handle(Input::ReadFinished(empty_result(&call)))
        .unwrap();
    let [Effect::Store(StoreOp::CommitCall(commit))] = effects.as_slice() else {
        panic!("one durable reference write: {effects:?}");
    };
    assert_eq!(commit.key, call.key);
    assert_eq!(commit.write, LocalWrite::None);
    assert_eq!(
        commit.output,
        ToolOutput::Recall(Selection::Search {
            query: "needle".into(),
            hits: Vec::new(),
            more: false
        })
    );
    assert!(core.current(R).unwrap().tools.waiting("history"));
    assert_eq!(
        core.handle(Input::ReadFinished(empty_result(&call))),
        Ok(Vec::new())
    );
    let effects = core
        .handle(Input::CallCommitted {
            key: call.key,
            outcome: Committed::Done,
            then: None,
        })
        .unwrap();
    assert!(
        matches!(effects.as_slice(), [Effect::Store(StoreOp::RoundItems(round))] if round.calls.len() == 1)
    );
}

#[test]
fn a_calling_build_source_delete_fences_even_an_empty_read_and_later_retries() {
    let mut core = kumo_rendered_b();
    let call = start_read(&mut core);
    delete_b(&mut core);
    let effects = core
        .handle(Input::ReadFinished(empty_result(&call)))
        .unwrap();
    assert_eq!(effects, vec![Effect::CancelEngine { run: R }]);
    assert_eq!(call.control.check(), Err(crate::recall::Error::Ended));
    assert!(!core.current(R).unwrap().tools.calling());
    assert_eq!(
        core.handle(Input::ReadFinished(empty_result(&call))),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(b_hidden()),
        Ok(vec![
            rebuilt(2),
            Effect::Control(ControlToShell::ThreadDeleted {
                thread: B,
                outcome: Deleted::Hidden,
                last_run: None,
            })
        ])
    );
    core.handle(rendering(build_key(2), Origin::Kumo, vec![KUMO]))
        .unwrap();
    let mut effects = Vec::new();
    assert_eq!(
        core.tool_call(
            model(R, "history"),
            ToolName::SearchHistory,
            br#"{"query":"needle"}"#,
            &mut effects
        ),
        ended()
    );
    assert_eq!(effects, Vec::new());
    assert_eq!(
        core.handle(Input::ReadFinished(empty_result(&call))),
        Ok(Vec::new())
    );
}

#[test]
fn invalid_read_selection_cannot_be_persisted() {
    let mut core = kumo_rendered_b();
    let call = start_read(&mut core);
    let mut done = empty_result(&call);
    done.result = Ok((
        Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(10),
            cut: Some(0),
            next: None,
        },
        vec![B],
    ));
    let effects = core.handle(Input::ReadFinished(done)).unwrap();
    let [Effect::Store(StoreOp::RoundItems(round))] = effects.as_slice() else {
        panic!("a refused round, not a CommitCall: {effects:?}");
    };
    assert_eq!(
        round.calls,
        vec![(
            CallId::new("history").unwrap(),
            ToolOutput::Error(ToolError::Invalid)
        )]
    );
}
