//! The main thread in the core loop: deleting it changes nothing anywhere, and renaming it never
//! reaches the store.

use kumochi_protocol::message::{
    ControlToShell, Deleted, Effort, ModelChoice, ModelSlug, Phase, RunId, ThreadId, ThreadOp,
    ThreadTitle,
};

use super::{R, T, fresh, state};
use crate::coordinator::{Effect, Input, StoreOp};
use crate::store::{Interruption, RunEnd, TitleSource};

/// `fresh`'s main thread.
const MAIN: ThreadId = ThreadId(1);

#[test]
fn deleting_the_main_thread_answers_not_hidden_before_anything_else() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(MAIN),
        text: "hi".to_owned(),
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
            text: "again".to_owned(),
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
                text: "again".to_owned(),
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
            thread: MAIN,
            choice: choice.clone(),
        })),
        Ok(vec![Effect::Store(StoreOp::SetModel {
            thread: MAIN,
            choice,
        })])
    );
}
