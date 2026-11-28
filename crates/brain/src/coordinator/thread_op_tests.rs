//! The main thread in the core loop: deleting it changes nothing anywhere, and renaming it never
//! reaches the store.

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ControlToShell, Deleted, Effort, ModelChoice, ModelSlug, Phase, RunId,
    SavedChoice, ThreadId, ThreadOp, ThreadTitle,
};

use super::{R, T, fresh, state};
use crate::coordinator::{Effect, Input, StoreOp};
use crate::store::{Interruption, RunEnd, TitleSource};

/// `fresh`'s main thread.
const MAIN: ThreadId = ThreadId(1);

/// No build serves a local server yet, so a thread's local choice is refused before the store,
/// and while a local default is saved so is every set or clear: a thread that inherits it stays
/// local.
#[test]
fn local_routes_are_refused_before_the_store() {
    use crate::config::DefaultIntent;
    let on = |connection| ModelChoice {
        model: ModelSlug::new("gpt-6.1-sol").expect("valid"),
        effort: Effort::Medium,
        connection,
    };
    for (local_default, choice) in [
        (
            DefaultIntent::Other,
            Some(on(kumochi_protocol::message::ConnectionKind::LocalChat)),
        ),
        (
            DefaultIntent::Local,
            Some(on(kumochi_protocol::message::ConnectionKind::ChatGpt)),
        ),
        (DefaultIntent::Local, None),
        (
            DefaultIntent::Unknown,
            Some(on(kumochi_protocol::message::ConnectionKind::ChatGpt)),
        ),
    ] {
        let mut core = fresh();
        core.local_default = local_default;
        assert_eq!(
            core.handle(Input::ThreadOp(ThreadOp::SetModel {
                request: 7,
                thread: T,
                choice: choice.clone(),
            })),
            Ok(vec![Effect::Control(ControlToShell::ThreadModelSet {
                request: 7,
                thread: T,
                result: kumochi_protocol::message::ChoiceRead::Failed,
            })]),
            "{local_default:?} {choice:?}"
        );
    }
}

#[test]
fn model_results_echo_the_exact_write_and_refusals_have_an_answer() {
    let mut core = fresh();
    let choice = ModelChoice {
        model: ModelSlug::new("model-result-sentinel").expect("valid"),
        effort: Effort::XHigh,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    };
    let session = core.model_session();
    let op = StoreOp::SetModel {
        session,
        request: 7,
        thread: T,
        choice: Some(choice.clone()),
    };
    assert_eq!(
        op.failed(),
        Some(Input::ModelSet {
            session,
            request: 7,
            thread: T,
            result: ChoiceRead::Failed,
        })
    );
    assert_eq!(
        StoreOp::Heard {
            run: kumochi_protocol::message::RunId(1),
            heard: crate::voice::reply::Heard {
                segment: kumochi_protocol::message::SegmentId(0),
                sample: 1,
                text: 1,
            },
        }
        .failed(),
        None,
        "a heard point that can't reach the store answers nothing"
    );
    let found = ChoiceRead::Found {
        choice: SavedChoice::Chosen(choice),
        revision: ChoiceRev(3),
    };
    for result in [ChoiceRead::Missing, ChoiceRead::Failed, found] {
        let input = Input::ModelSet {
            session,
            request: 7,
            thread: T,
            result: result.clone(),
        };
        let expected = vec![Effect::Control(ControlToShell::ThreadModelSet {
            request: 7,
            thread: T,
            result,
        })];
        for debug in [
            format!("{input:?}"),
            format!("{expected:?}"),
            format!("{op:?}"),
        ] {
            assert!(!debug.contains("model-result-sentinel"));
            assert!(!debug.contains(&format!("{:?}", b"model-result-sentinel")));
        }
        assert_eq!(core.handle(input), Ok(expected));
    }
}

#[test]
fn deleting_the_main_thread_answers_not_hidden_before_anything_else() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(MAIN),
        text: "hi".into(),
    })
    .expect("turn");
    core.handle(Input::TurnStored { run: R }).expect("stored");
    let delete = Input::Delete {
        thread: MAIN,
        after_turns: 2,
        resend: false,
    };
    assert_eq!(
        core.handle(delete),
        Ok(vec![Effect::Control(ControlToShell::ThreadDeleted {
            thread: MAIN,
            outcome: Deleted::NotHidden,
            last_run: None,
        })])
    );
    assert_eq!(core.current_run(), Some(R), "its run goes on");
    assert!(!core.deletes_pending());
    // No fence: the turn the shell sent before the delete still reaches the thread.
    let next = core
        .handle(Input::Turn {
            turn: 1,
            thread: Some(MAIN),
            text: "again".into(),
        })
        .expect("turn");
    assert_eq!(
        next,
        vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            state(R, MAIN, Phase::Interrupted),
            super::run_end(R),
            Effect::Store(StoreOp::EndRun {
                screen: None,
                thread: MAIN,
                run: R,
                text: None,
                revealed: None,
                clear: None,
                end: RunEnd::Interrupted(Interruption::Replaced),
            }),
            Effect::Store(StoreOp::BeginTurn {
                thread: MAIN,
                title: None,
                run: RunId(11),
                text: "again".into(),
                screen: None,
                local_default: crate::config::DefaultIntent::Other,
            }),
            state(RunId(11), MAIN, Phase::Thinking),
            Effect::ArmDeadline {
                run: RunId(11),
                after: super::WALL_CLOCK,
            },
        ]
    );
}

#[test]
fn a_rename_of_the_main_thread_goes_nowhere_and_a_tasks_is_a_user_title() {
    let mut core = fresh();
    let title = ThreadTitle::new("Trip plan").expect("valid");
    let rename = |thread| {
        Input::ThreadOp(ThreadOp::Rename {
            thread,
            title: title.clone(),
        })
    };
    assert_eq!(core.handle(rename(MAIN)), Ok(Vec::new()));
    assert_eq!(
        core.handle(rename(T)),
        Ok(vec![Effect::Store(StoreOp::SetTitle {
            thread: T,
            title: title.clone(),
            source: TitleSource::User,
            job: None,
        })])
    );
    let choice = ModelChoice {
        model: ModelSlug::new("gpt-6.1-sol").expect("valid"),
        effort: Effort::Medium,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    };
    assert_eq!(
        core.handle(Input::ThreadOp(ThreadOp::SetModel {
            request: 2,
            thread: MAIN,
            choice: Some(choice.clone()),
        })),
        Ok(vec![Effect::Store(StoreOp::SetModel {
            session: core.model_session(),
            request: 2,
            thread: MAIN,
            choice: Some(choice),
        })])
    );
}
