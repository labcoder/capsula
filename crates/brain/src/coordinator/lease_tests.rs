//! The send lease: a granted send holds back each delete of a source it carries until every
//! lease carrying it settles, and one that never settles refuses them for good.

use std::time::Duration;

use kumochi_protocol::message::{ControlToShell, DataToShell, Deleted, Phase, RunId, ThreadId};

use super::{R, T, build_context, built, ends_failed, fresh, start_engine};
use crate::coordinator::{
    BuildKey, Coordinator, Effect, Input, LeaseId, MAX_HOLD, SEND_DEADLINE, SendKey, Stale, StoreOp,
};
use crate::store::{Failure, Interruption, RunEnd};

const B: ThreadId = ThreadId(2);
const C: ThreadId = ThreadId(3);
/// The run that replaces run 10 in task 4.
const R2: RunId = RunId(11);

fn key(run: RunId, attempt: u64) -> SendKey {
    SendKey::Build(BuildKey {
        run,
        round: 1,
        attempt,
    })
}

fn lease(run: RunId, attempt: u64, send: u64) -> LeaseId {
    LeaseId {
        key: key(run, attempt),
        send,
    }
}

fn ready(run: RunId, attempt: u64) -> Input {
    Input::SendReady {
        key: key(run, attempt),
        hold: None,
    }
}

fn granted(lease: LeaseId) -> Vec<Effect> {
    vec![
        Effect::Grant(lease),
        Effect::ArmSendDeadline {
            lease,
            after: SEND_DEADLINE,
        },
    ]
}

fn settled(lease: LeaseId) -> Input {
    Input::SendSettled { lease }
}

fn due(lease: LeaseId) -> Input {
    Input::SendDeadline { lease }
}

fn delete(thread: ThreadId) -> Input {
    Input::Delete {
        thread,
        after_turns: 1,
        resend: false,
    }
}

fn store_delete(thread: ThreadId) -> Effect {
    Effect::Store(StoreOp::Delete { thread })
}

fn hidden(thread: ThreadId) -> Input {
    Input::Deleted {
        thread,
        outcome: Deleted::Hidden,
        last_run: None,
    }
}

fn told(thread: ThreadId, outcome: Deleted) -> Effect {
    Effect::Control(ControlToShell::ThreadDeleted {
        thread,
        outcome,
        last_run: None,
    })
}

/// Run 10 in a new task 4, its build of `sources` admitted and its first send granted.
fn sending(sources: &[ThreadId]) -> Coordinator {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    assert_eq!(
        core.handle(Input::TurnStored { run: R }).expect("current"),
        vec![build_context(R, T, 1)]
    );
    assert_eq!(
        core.handle(built(R, 1, sources)).expect("current"),
        vec![start_engine(R, T, 1)]
    );
    assert_eq!(
        core.handle(ready(R, 1)).expect("asked"),
        granted(lease(R, 1, 1))
    );
    core
}

/// Stops run 10 mid-send and starts its replacement in task 4, up to its first build.
fn replace(core: &mut Coordinator) {
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    core.handle(Input::RunStored { run: R }).expect("ending");
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "again".into(),
    })
    .expect("a turn is never stale");
    assert_eq!(
        core.handle(Input::TurnStored { run: R2 }).expect("current"),
        vec![build_context(R2, T, 2)]
    );
}

/// Run 10 and its replacement each hold a lease carrying B.
fn two_sending() -> Coordinator {
    let mut core = sending(&[T, B]);
    replace(&mut core);
    assert_eq!(
        core.handle(built(R2, 2, &[T, B])).expect("current"),
        vec![start_engine(R2, T, 2)]
    );
    assert_eq!(
        core.handle(ready(R2, 2)).expect("asked"),
        granted(lease(R2, 2, 2))
    );
    core
}

#[test]
fn two_deletes_held_by_one_lease_are_each_emitted_once() {
    let mut core = sending(&[T, B, C]);
    assert_eq!(core.handle(delete(C)), Ok(Vec::new()), "held");
    assert_eq!(core.handle(delete(B)), Ok(Vec::new()), "held");
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(vec![store_delete(C), store_delete(B)]),
        "in the order accepted"
    );
    assert_eq!(core.handle(settled(lease(R, 1, 1))), Err(Stale));
    assert_eq!(core.handle(hidden(C)), Ok(vec![told(C, Deleted::Hidden)]));
}

#[test]
fn a_delete_while_waiting_goes_out_and_the_body_is_rebuilt_unsent() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(R, 1, &[T, B])).expect("current");
    assert_eq!(
        core.handle(delete(B)),
        Ok(vec![store_delete(B)]),
        "no lease carries B"
    );
    assert_eq!(
        core.handle(ready(R, 1)),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Rebuild(key(R, 1))
        ]),
        "the rebuild waits for B's answer"
    );
}

#[test]
fn the_run_deadline_does_not_reset_on_rebuild_or_retry() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    assert_eq!(
        core.handle(built(R, 1, &[T, B])),
        Ok(vec![start_engine(R, T, 1)])
    );
    assert_eq!(core.handle(delete(B)), Ok(vec![store_delete(B)]));
    assert_eq!(
        core.handle(ready(R, 1)),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Rebuild(key(R, 1))
        ])
    );
    assert_eq!(
        core.handle(hidden(B)),
        Ok(vec![build_context(R, T, 2), told(B, Deleted::Hidden)])
    );
    assert_eq!(
        core.handle(built(R, 2, &[T])),
        Ok(vec![start_engine(R, T, 2)])
    );
    assert_eq!(core.handle(ready(R, 2)), Ok(granted(lease(R, 2, 1))));
    assert_eq!(core.handle(settled(lease(R, 2, 1))), Ok(Vec::new()));
    assert_eq!(
        core.handle(ready(R, 2)),
        Ok(granted(lease(R, 2, 2))),
        "a retry takes a new lease, and no step re-arms the run's deadline"
    );
}

#[test]
fn a_build_started_after_a_held_delete_gets_no_grant_containing_its_source() {
    let mut core = sending(&[T, B]);
    assert_eq!(core.handle(delete(B)), Ok(Vec::new()), "held");
    replace(&mut core);
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(vec![store_delete(B)])
    );
    assert_eq!(core.handle(hidden(B)), Ok(vec![told(B, Deleted::Hidden)]));
    assert_eq!(
        core.handle(built(R2, 2, &[T, B])),
        Ok(vec![build_context(R2, T, 3)]),
        "B was pending when the build began; nothing is pending now, so the rebuild starts"
    );
    assert_eq!(
        core.handle(ready(R2, 2)),
        Ok(vec![Effect::Rebuild(key(R2, 2))])
    );
    assert_eq!(
        core.handle(built(R2, 3, &[T])),
        Ok(vec![start_engine(R2, T, 3)])
    );
    assert_eq!(core.handle(ready(R2, 3)), Ok(granted(lease(R2, 3, 2))));
}

#[test]
fn an_old_settlement_after_a_replacement_run_retires_only_its_lease() {
    let mut core = two_sending();
    assert_eq!(
        core.handle(ready(R, 1)),
        Ok(vec![Effect::Rebuild(key(R, 1))]),
        "ended"
    );
    assert_eq!(core.handle(delete(B)), Ok(Vec::new()), "held");
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(Vec::new()),
        "R2 still sends B"
    );
    assert_eq!(core.handle(settled(lease(R, 1, 1))), Err(Stale));
    assert_eq!(
        core.handle(settled(lease(R2, 2, 2))),
        Ok(vec![store_delete(B)])
    );
}

#[test]
fn a_delete_held_by_two_leases_waits_for_both_and_is_refused_if_either_times_out() {
    let mut core = two_sending();
    core.handle(delete(B)).expect("held");
    assert_eq!(core.handle(settled(lease(R2, 2, 2))), Ok(Vec::new()));
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(vec![store_delete(B)])
    );

    let mut core = two_sending();
    core.handle(delete(B)).expect("held");
    assert_eq!(core.handle(settled(lease(R2, 2, 2))), Ok(Vec::new()));
    assert_eq!(
        core.handle(due(lease(R, 1, 1))),
        Ok(vec![
            Effect::CancelEngine { run: R },
            told(B, Deleted::NotHidden)
        ]),
        "the old run has ended, so only its engine is cancelled"
    );
}

#[test]
fn a_send_that_never_settles_refuses_its_held_deletes_and_stops_granting() {
    let mut core = sending(&[T, B]);
    core.handle(delete(B)).expect("held");
    let mut expected = ends_failed(Failure::Engine);
    expected.push(told(B, Deleted::NotHidden));
    assert_eq!(core.handle(due(lease(R, 1, 1))), Ok(expected));
    assert!(
        core.refused_delete(),
        "a quit after this refusal is incomplete"
    );
    assert_eq!(
        core.handle(due(lease(R, 1, 1))),
        Err(Stale),
        "reported once"
    );
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(Vec::new()),
        "a late settlement releases nothing"
    );
    assert_eq!(
        core.handle(delete(B)),
        Ok(vec![told(B, Deleted::NotHidden)]),
        "B stays fenced"
    );
    assert!(core.refused_delete());
}

#[test]
fn a_failed_transport_grants_nothing_until_the_brain_restarts() {
    let mut core = sending(&[T]);
    core.handle(due(lease(R, 1, 1))).expect("granted");
    core.handle(Input::RunStored { run: R }).expect("ending");
    core.handle(Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "again".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R2 }).expect("current");
    core.handle(built(R2, 2, &[T])).expect("current");
    assert_eq!(
        core.handle(ready(R2, 2)),
        Ok(vec![
            Effect::Rebuild(key(R2, 2)),
            Effect::CancelEngine { run: R2 },
            Effect::Control(ControlToShell::RunState {
                origin: None,
                run: R2,
                thread: T,
                phase: Phase::Failed,
                error: None,
            }),
            Effect::Data(DataToShell::RunEnd { run: R2 }),
            Effect::Store(StoreOp::EndRun {
                screen: None,
                thread: T,
                run: R2,
                text: None,
                revealed: None,
                clear: None,
                end: RunEnd::Failed(Failure::Engine),
            }),
        ])
    );
}

#[test]
fn deleting_the_sending_runs_own_task_settles_without_a_cycle() {
    let mut core = sending(&[T]);
    assert_eq!(
        core.handle(delete(T)),
        Ok(vec![
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            Effect::Control(ControlToShell::RunState {
                origin: None,
                run: R,
                thread: T,
                phase: Phase::Interrupted,
                error: None,
            }),
            Effect::Data(DataToShell::RunEnd { run: R }),
            Effect::Store(StoreOp::EndRun {
                screen: None,
                thread: T,
                run: R,
                text: None,
                revealed: None,
                clear: None,
                end: RunEnd::Interrupted(Interruption::Deleted),
            }),
        ]),
        "the run ends, and its delete waits for the send it started"
    );
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(vec![store_delete(T)])
    );
}

#[test]
fn a_held_delete_survives_a_shell_reconnect() {
    let mut core = sending(&[T, B]);
    core.handle(delete(B)).expect("held");
    core.handle(Input::ShellLost).expect("current");
    core.begin_session();
    let resend = Input::Delete {
        thread: B,
        after_turns: 0,
        resend: true,
    };
    assert_eq!(core.handle(resend), Ok(Vec::new()), "no second op");
    assert_eq!(
        core.handle(settled(lease(R, 1, 1))),
        Ok(vec![store_delete(B)])
    );
}

/// What a held ask arms for run 10's first send: `hold` up to [`MAX_HOLD`], or
/// [`SEND_DEADLINE`] without one.
fn armed_for_build(hold: Option<Duration>, send_lease_s: u64) -> Vec<Effect> {
    let mut core = fresh();
    core.tuning.send_lease = Duration::from_secs(send_lease_s);
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(R, 1, &[T])).expect("current");
    core.handle(Input::SendReady {
        key: key(R, 1),
        hold,
    })
    .expect("asked")
}

fn armed(lease: LeaseId, seconds: u64) -> Vec<Effect> {
    vec![
        Effect::Grant(lease),
        Effect::ArmSendDeadline {
            lease,
            after: Duration::from_secs(seconds),
        },
    ]
}

/// H-h, a build's send: the default, under the cap, exactly at it, and over it.
#[tokio::test(start_paused = true)]
async fn a_held_build_send_arms_its_hold_capped_at_165_seconds() {
    assert_eq!(MAX_HOLD, Duration::from_secs(165));
    let first = lease(R, 1, 1);
    assert_eq!(armed_for_build(None, 15), armed(first, 15));
    assert_eq!(armed_for_build(None, 11), armed(first, 11));
    for (hold, seconds) in [(20, 20), (165, 165), (166, 165)] {
        let at = tokio::time::Instant::now() + Duration::from_secs(seconds);
        assert_eq!(
            armed_for_build(Some(Duration::from_secs(hold)), 11),
            [
                Effect::Grant(first),
                Effect::ArmSendDeadlineAt { lease: first, at },
            ],
        );
    }
}

/// H-h: a held send's deadline fails closed like any other.
#[test]
fn a_held_send_past_its_hold_fails_the_run_and_keeps_its_deletes() {
    let mut core = fresh();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(R, 1, &[T, B])).expect("current");
    core.handle(Input::SendReady {
        key: key(R, 1),
        hold: Some(Duration::from_secs(40)),
    })
    .expect("asked");
    core.handle(delete(B)).expect("held");
    let mut expected = ends_failed(Failure::Engine);
    expected.push(told(B, Deleted::NotHidden));
    assert_eq!(core.handle(due(lease(R, 1, 1))), Ok(expected));
    assert_eq!(core.handle(settled(lease(R, 1, 1))), Ok(Vec::new()));
}
