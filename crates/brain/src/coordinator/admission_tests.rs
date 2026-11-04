//! Admission: a round starts only with a build no accepted or pending delete has touched.

use kumochi_protocol::message::{AssistantName, ControlToShell, Deleted, ThreadId};

use super::{R, T, build_context, built, ends_failed, fresh, request};
use crate::context::{ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::{
    BuildContext, BuildKey, ContextBuilt, Coordinator, Effect, Input, Stale, StoreOp, Unbuilt,
};
use crate::engines::RunRequest;
use crate::store::Failure;

const MAIN: ThreadId = ThreadId(1);
const B: ThreadId = ThreadId(2);
const C: ThreadId = ThreadId(3);

fn key(attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt,
    }
}

fn kumo_build(attempt: u64, snapshot: Option<RunSnapshot>) -> Effect {
    Effect::Store(StoreOp::BuildContext(BuildContext {
        key: key(attempt),
        thread: MAIN,
        origin: Origin::Kumo,
        first: snapshot.is_none(),
        snapshot,
    }))
}

fn kumo_built(attempt: u64, sources: &[ThreadId]) -> Input {
    built(R, attempt, sources)
}

fn kumo_start(attempt: u64) -> Effect {
    Effect::StartEngine {
        request: RunRequest {
            key: key(attempt),
            run: R,
            thread: MAIN,
            origin: Origin::Kumo,
            request: request(),
        },
    }
}

fn delete(thread: ThreadId) -> Input {
    Input::Delete {
        thread,
        after_turns: 1,
        resend: false,
    }
}

fn hidden(thread: ThreadId) -> Input {
    Input::Deleted {
        thread,
        outcome: Deleted::Hidden,
        last_run: None,
    }
}

fn answered(thread: ThreadId) -> Effect {
    Effect::Control(ControlToShell::ThreadDeleted {
        thread,
        outcome: Deleted::Hidden,
        last_run: None,
    })
}

/// Run 10 in Kumo's thread, its turn stored and its first build out.
fn kumo_building() -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(MAIN),
        text: "what happened?".to_owned(),
    })
    .expect("a turn is never stale");
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("current"),
        vec![kumo_build(1, None)]
    );
    core
}

#[test]
fn a_build_that_read_a_thread_deleted_since_it_began_waits_for_the_delete_and_builds_again() {
    let mut core = kumo_building();
    assert_eq!(
        core.handle(delete(B)).expect("accepted"),
        vec![Effect::Store(StoreOp::Delete { thread: B })]
    );
    assert_eq!(
        core.handle(kumo_built(1, &[MAIN, B])).expect("current"),
        Vec::new(),
        "B's text is dropped, and nothing builds while its delete is pending"
    );
    assert_eq!(
        core.handle(hidden(B)).expect("pending"),
        vec![kumo_build(2, None), answered(B)]
    );
    assert_eq!(
        core.handle(kumo_built(2, &[MAIN])).expect("current"),
        vec![kumo_start(2)]
    );
}

#[test]
fn a_delete_answered_before_the_old_result_arrives_still_voids_it() {
    let mut core = kumo_building();
    core.handle(delete(B)).expect("accepted");
    assert_eq!(core.handle(hidden(B)).expect("pending"), vec![answered(B)]);
    assert_eq!(
        core.handle(kumo_built(1, &[MAIN, B])).expect("current"),
        vec![kumo_build(2, None)],
        "the delete is already answered, so the rebuild starts at once"
    );
}

#[test]
fn a_delete_pending_when_the_build_began_voids_it_even_once_answered() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(MAIN),
        text: "first".to_owned(),
    })
    .expect("a turn is never stale");
    core.handle(delete(B)).expect("accepted");
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("current"),
        vec![kumo_build(1, None)]
    );
    core.handle(hidden(B)).expect("pending");
    assert_eq!(
        core.handle(kumo_built(1, &[MAIN, B])).expect("current"),
        vec![kumo_build(2, None)],
        "B was pending at the build's start, so its old text never starts a round"
    );
}

#[test]
fn a_result_of_an_older_attempt_is_stale() {
    let mut core = kumo_building();
    core.handle(delete(B)).expect("accepted");
    core.handle(hidden(B)).expect("pending");
    core.handle(kumo_built(1, &[MAIN, B])).expect("current");
    assert_eq!(core.handle(kumo_built(1, &[MAIN])), Err(Stale));
    assert_eq!(
        core.handle(kumo_built(2, &[MAIN])).expect("current"),
        vec![kumo_start(2)]
    );
    assert_eq!(
        core.handle(kumo_built(2, &[MAIN])),
        Err(Stale),
        "admitted once"
    );
}

#[test]
fn a_result_blocked_on_a_pending_delete_takes_no_other_result() {
    let mut core = kumo_building();
    core.handle(delete(B)).expect("accepted");
    core.handle(kumo_built(1, &[MAIN, B])).expect("current");
    assert_eq!(core.handle(kumo_built(1, &[MAIN])), Err(Stale));
    assert_eq!(
        core.handle(kumo_built(2, &[MAIN])),
        Err(Stale),
        "not started"
    );
}

#[test]
fn a_delete_of_a_thread_the_build_didnt_read_changes_nothing() {
    let mut core = kumo_building();
    core.handle(delete(C)).expect("accepted");
    assert_eq!(
        core.handle(kumo_built(1, &[MAIN, B])).expect("current"),
        vec![kumo_start(1)]
    );
}

#[test]
fn a_rebuild_reuses_the_snapshot_the_first_build_read() {
    let mut core = kumo_building();
    core.handle(delete(B)).expect("accepted");
    core.handle(hidden(B)).expect("pending");
    let snapshot = RunSnapshot {
        origin: Origin::Kumo,
        assistant: AssistantName::new("Kumo").expect("name"),
        user: None,
        choice: None,
        connection: ConnectionTag(0),
    };
    let first = Input::ContextBuilt(ContextBuilt {
        key: key(1),
        snapshot: Some(snapshot.clone()),
        result: Ok(crate::context::Built {
            request: request(),
            sources: vec![MAIN, B],
        }),
    });
    assert_eq!(
        core.handle(first).expect("current"),
        vec![kumo_build(2, Some(snapshot))],
        "a rename after the run started applies only from the next run"
    );
}

#[test]
fn a_build_too_large_fails_the_run_on_its_budget_and_a_failed_one_on_the_store() {
    for (unbuilt, failure) in [
        (Unbuilt::TooLarge, Failure::Budget),
        (Unbuilt::Failed, Failure::Store),
    ] {
        let mut core = fresh();
        core.handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".to_owned(),
        })
        .expect("a turn is never stale");
        assert_eq!(
            core.handle(Input::TurnStored { run: R }).expect("current"),
            vec![build_context(R, T, 1)]
        );
        let result = Input::ContextBuilt(ContextBuilt {
            key: key(1),
            snapshot: None,
            result: Err(unbuilt),
        });
        assert_eq!(core.handle(result).expect("current"), ends_failed(failure));
    }
}

#[test]
fn a_stopped_run_takes_no_build_result() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".to_owned(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(core.handle(built(R, 1, &[T])), Err(Stale));
}
