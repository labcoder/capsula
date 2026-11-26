//! The background job slot: a task's title job after a run ends `Done`, fenced by new runs,
//! deletes and sign-out, and keyed so an old job's answers never touch a new one.

use std::time::Duration;

use kumochi_protocol::message::{
    AssistantName, ConnectionKind, ControlToShell, Deleted, Effort, EffortSet, EventId,
    ModelChoice, ModelInfo, ModelName, ModelSlug, Phase, RunId, ThreadId, ThreadTitle,
};

use super::{R, T, admit, built, ends_failed, engine, fresh, request, run_end, state};
use crate::context::compact::COMPACT_AT;
use crate::context::{Built, ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::availability::model_unavailable;
use crate::coordinator::{
    Availability, BuildKey, ContextBuilt, Coordinator, Effect, Input, JobAsk, JobBuilt, JobKey,
    JobKind, JobOutput, LeaseId, ReadJob, Ready, SEND_DEADLINE, SendKey, Stale, StoreOp,
    SummaryText, Unbuilt,
};
use crate::engines::{Finish, RunRequest, TurnEvent};
use crate::store::{Boundary, Failure, Generation, TitleSource};

/// The deletion generation every test read finds.
const G: Generation = Generation(5);
/// Run 10's successor in task 4.
const R2: RunId = RunId(11);
/// Kumo's thread, as `fresh` makes it.
const MAIN: ThreadId = ThreadId(1);

#[test]
fn lifecycle_job_reader_is_canceled_by_new_runs_and_sign_out() {
    for sign_out in [false, true] {
        let mut core = signed_in();
        let effects = run_done(&mut core, None, R);
        let read = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::ReadJob(read)) => Some(read.control),
                _ => None,
            })
            .unwrap();
        assert_eq!(read.check(), Ok(()));
        let input = if sign_out {
            Input::SignedOut { generation: 1 }
        } else {
            Input::Turn {
                turn: 2,
                thread: Some(T),
                text: "next".into(),
            }
        };
        core.handle(input).unwrap();
        assert_eq!(read.check(), Err(crate::recall::Error::Ended));
    }
}

fn signed_in() -> Coordinator {
    let mut core = fresh();
    assert_eq!(
        core.handle(Input::SignedIn { generation: 1 }),
        Ok(Vec::new())
    );
    core
}

/// Takes a turn in `thread` (a new task when `None`) and runs it to a stored `Done`; what that
/// stored outcome did.
fn run_done(core: &mut Coordinator, thread: Option<ThreadId>, run: RunId) -> Vec<Effect> {
    core.handle(Input::Turn {
        turn: 1,
        thread,
        text: "Plan a trip".into(),
    })
    .expect("a turn is never stale");
    admit(core, run);
    core.handle(engine(
        super::engine_key(core, run),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    core.handle(Input::RunStored { run }).expect("ending")
}

fn key(serial: u64) -> JobKey {
    JobKey { thread: T, serial }
}

/// What a stored `Done` of `run` in task 4 does when it starts job `serial`.
fn done_starting(run: RunId, serial: u64) -> Vec<Effect> {
    vec![
        read_job(key(serial), JobKind::Title),
        state(run, T, Phase::Done),
        run_end(run),
    ]
}

/// A signed-in coordinator whose run 10 in new task 4 ended `Done`: job 2's read is out.
fn reading() -> Coordinator {
    let mut core = signed_in();
    assert_eq!(run_done(&mut core, None, R), done_starting(R, 2));
    core
}

fn read_job(key: JobKey, kind: JobKind) -> Effect {
    Effect::Store(StoreOp::ReadJob(ReadJob {
        require_recovery: None,
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key,
        kind,
        origin: if key.thread == MAIN {
            Origin::Kumo
        } else {
            Origin::Task(key.thread)
        },
        snapshot: None,
        compact_at: COMPACT_AT,
    }))
}

/// Task 4's compaction read, as job `serial`.
fn compacts(serial: u64) -> Effect {
    read_job(key(serial), JobKind::Compact)
}

fn job_built(key: JobKey, sources: &[ThreadId]) -> Input {
    Input::JobBuilt(JobBuilt {
        key,
        result: Ok(Some(JobAsk {
            generation: G,
            built: Built {
                recovered_context: None,
                request: request(),
                sources: sources.to_vec(),
            },
            covers: None,
        })),
    })
}

fn start_job(key: JobKey, run: RunId) -> Effect {
    Effect::StartJob {
        job: key,
        kind: JobKind::Title,
        request: RunRequest {
            key: SendKey::Job(key),
            run,
            thread: T,
            origin: Origin::Task(T),
            request: request(),
        },
    }
}

fn ready(key: JobKey) -> Input {
    Input::SendReady {
        key: SendKey::Job(key),
        hold: None,
    }
}

fn lease(key: JobKey, send: u64) -> LeaseId {
    LeaseId {
        key: SendKey::Job(key),
        send,
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

fn rebuild(key: JobKey) -> Vec<Effect> {
    vec![Effect::Rebuild(SendKey::Job(key))]
}

fn titled(text: &str) -> ThreadTitle {
    ThreadTitle::new(text).expect("valid")
}

fn job_done(key: JobKey, title: Option<ThreadTitle>) -> Input {
    Input::JobDone {
        key,
        output: title.map(JobOutput::Title),
    }
}

fn set_title(key: JobKey, text: &str) -> Effect {
    Effect::Store(StoreOp::SetTitle {
        thread: T,
        title: ThreadTitle::new(text).expect("valid"),
        source: TitleSource::Model(G),
        job: Some(key),
    })
}

fn nudge() -> Effect {
    Effect::Control(ControlToShell::ThreadsChanged)
}

fn delete(thread: ThreadId, after_turns: u32) -> Input {
    Input::Delete {
        thread,
        after_turns,
        resend: false,
    }
}

/// Job `key` of a coordinator from [`reading`], up to the write of its title.
fn writing(core: &mut Coordinator, key: JobKey) {
    assert_eq!(
        core.handle(job_built(key, &[T])),
        Ok(vec![start_job(key, R)])
    );
    assert_eq!(core.handle(ready(key)), Ok(granted(lease(key, 1))));
    assert_eq!(
        core.handle(Input::SendSettled {
            lease: lease(key, 1)
        }),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(job_done(key, Some(titled("Lisbon trip")))),
        Ok(vec![set_title(key, "Lisbon trip")])
    );
}

#[test]
fn a_words_task_takes_the_models_title_after_its_first_done_run() {
    let mut core = reading();
    writing(&mut core, key(2));
    assert_eq!(
        core.handle(Input::TitleSet {
            job: Some(key(2)),
            applied: true
        }),
        Ok(vec![nudge(), compacts(3)]),
        "the shell reads its list again, and compaction follows"
    );
    assert_eq!(
        core.handle(job_done(key(2), Some(titled("Again")))),
        Err(Stale),
        "the slot is empty"
    );
}

#[test]
fn kumo_is_compacted_never_titled_and_a_signed_out_brain_starts_no_job() {
    let mut core = fresh();
    assert_eq!(
        run_done(&mut core, None, R),
        vec![state(R, T, Phase::Done), run_end(R)],
        "nobody is signed in"
    );
    let mut core = signed_in();
    assert_eq!(
        run_done(&mut core, Some(MAIN), R),
        vec![
            read_job(
                JobKey {
                    thread: MAIN,
                    serial: 2
                },
                JobKind::Compact
            ),
            state(R, MAIN, Phase::Done),
            run_end(R)
        ],
        "Kumo is never titled, only compacted"
    );
}

#[test]
fn a_read_that_finds_no_first_words_or_fails_ends_the_job_unsent() {
    for result in [Ok(None), Err(Unbuilt::Failed)] {
        let mut core = reading();
        let read = Input::JobBuilt(JobBuilt {
            key: key(2),
            result,
        });
        assert_eq!(core.handle(read), Ok(vec![compacts(3)]));
        assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
    }
}

#[test]
fn a_user_rename_during_the_job_wins_without_a_nudge() {
    let mut core = reading();
    writing(&mut core, key(2));
    assert_eq!(
        core.handle(Input::TitleSet {
            job: Some(key(2)),
            applied: false
        }),
        Ok(vec![compacts(3)]),
        "the rename landed first, so nothing changed; compaction follows"
    );
    assert_eq!(
        core.handle(Input::TitleSet {
            job: None,
            applied: true
        }),
        Ok(vec![nudge()]),
        "a committed rename is a committed change"
    );
}

#[test]
fn a_delete_at_the_read_voids_the_job() {
    let mut core = reading();
    assert_eq!(
        core.handle(delete(T, 1)),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: T })])
    );
    assert_eq!(core.handle(job_built(key(2), &[T])), Ok(Vec::new()));
    assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
}

#[test]
fn a_delete_mid_send_is_held_and_the_job_writes_nothing() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
    assert_eq!(
        core.handle(delete(T, 1)),
        Ok(Vec::new()),
        "the job's lease holds it"
    );
    assert_eq!(
        core.handle(Input::SendSettled {
            lease: lease(key(2), 1)
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: T })])
    );
    assert_eq!(
        core.handle(job_done(key(2), Some(titled("Lisbon trip")))),
        Ok(Vec::new()),
        "the delete reached the task, so its title isn't written"
    );
}

#[test]
fn a_delete_before_the_grant_drops_the_body_unsent() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(
        core.handle(delete(T, 1)),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: T })])
    );
    assert_eq!(
        core.handle(ready(key(2))),
        Ok(vec![
            Effect::Rebuild(SendKey::Job(key(2))),
            Effect::CancelJob(key(2))
        ])
    );
}

#[test]
fn a_new_run_cancels_the_job_and_its_late_answers_are_stale() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    let effects = core
        .handle(Input::Turn {
            turn: 1,
            thread: Some(T),
            text: "More".into(),
        })
        .expect("a turn is never stale");
    assert_eq!(effects.first(), Some(&Effect::CancelJob(key(2))));
    assert_eq!(
        core.handle(job_done(key(2), Some(titled("Late")))),
        Err(Stale)
    );
    assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
    assert_eq!(core.handle(job_built(key(2), &[T])), Err(Stale));
}

#[test]
fn a_failed_job_starts_nothing_after_it_and_the_next_done_retries() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(
        core.handle(job_done(key(2), None)),
        Ok(Vec::new()),
        "a request went out and failed, so no compaction follows it"
    );
    assert_eq!(core.handle(job_done(key(2), None)), Err(Stale));
    assert_eq!(
        run_done(&mut core, Some(T), R2),
        done_starting(R2, 4),
        "the next Done tries again, with a new key"
    );
}

#[test]
fn sign_out_while_the_request_is_held_writes_nothing_and_starts_no_more() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
    assert_eq!(
        core.handle(Input::SignedOut { generation: 1 }),
        Ok(vec![Effect::CancelJob(key(2))])
    );
    assert_eq!(
        core.handle(job_done(key(2), Some(titled("Late")))),
        Err(Stale)
    );
    assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
    assert_eq!(
        run_done(&mut core, Some(T), R2),
        vec![state(R2, T, Phase::Done), run_end(R2)],
        "no job starts while signed out"
    );
    assert_eq!(
        core.handle(Input::SignedIn { generation: 1 }),
        Err(Stale),
        "the signed-out generation can't come back"
    );
}

#[test]
fn an_older_sign_out_leaves_a_newer_accounts_job_running() {
    let mut core = signed_in();
    assert_eq!(
        core.handle(Input::SignedOut { generation: 1 }),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(Input::SignedIn { generation: 2 }),
        Ok(Vec::new())
    );
    assert_eq!(run_done(&mut core, None, R), done_starting(R, 2));
    assert_eq!(core.handle(Input::SignedOut { generation: 1 }), Err(Stale));
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(
        core.handle(Input::SignedIn { generation: 3 }),
        Ok(vec![Effect::CancelJob(key(2))]),
        "another account's sign-in ends the job"
    );
}

#[test]
fn a_done_whose_write_failed_starts_no_job() {
    let mut core = signed_in();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "Plan a trip".into(),
    })
    .expect("a turn is never stale");
    admit(&mut core, R);
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    core.handle(Input::StoreFailed { run: R }).expect("ending");
    let stored = core.handle(Input::RunStored { run: R }).expect("ending");
    assert!(
        !stored
            .iter()
            .any(|effect| matches!(effect, Effect::Store(StoreOp::ReadJob(_)))),
        "{stored:?}"
    );
}

#[test]
fn an_old_title_ack_never_clears_a_replacement_job() {
    for applied in [true, false] {
        let mut core = reading();
        writing(&mut core, key(2));
        assert_eq!(
            run_done(&mut core, Some(T), R2),
            done_starting(R2, 4),
            "the new run cancelled job 2 and its Done started job 4"
        );
        assert_eq!(
            core.handle(Input::TitleSet {
                job: Some(key(2)),
                applied
            }),
            Ok(if applied { vec![nudge()] } else { Vec::new() }),
            "a committed write is still a change"
        );
        assert_eq!(
            core.handle(job_built(key(4), &[T])),
            Ok(vec![start_job(key(4), R2)]),
            "job 4 is still in the slot"
        );
    }
}

#[test]
fn a_job_lease_past_its_deadline_cancels_the_job_and_fails_closed() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
    assert_eq!(
        core.handle(Input::SendDeadline {
            lease: lease(key(2), 1)
        }),
        Ok(vec![Effect::CancelJob(key(2))])
    );
    assert_eq!(
        run_done(&mut core, Some(T), R2),
        vec![state(R2, T, Phase::Done), run_end(R2)],
        "no job starts once sends failed closed"
    );
}

/// A held send is a Claude Code process on its own pipes: past its hold it fails its run and
/// fences its sources, but a later run and its title job are still granted.
#[test]
fn a_held_send_past_its_hold_fences_its_sources_and_leaves_other_sends_granted() {
    const B: ThreadId = ThreadId(2);
    let refused = Effect::Control(ControlToShell::ThreadDeleted {
        thread: B,
        outcome: Deleted::NotHidden,
        last_run: None,
    });
    let build = |run, attempt| {
        SendKey::Build(BuildKey {
            run,
            round: 1,
            attempt,
        })
    };
    let mut core = signed_in();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R }).expect("current");
    core.handle(built(R, 1, &[T, B])).expect("current");
    let held = LeaseId {
        key: build(R, 1),
        send: 1,
    };
    core.handle(Input::SendReady {
        key: held.key,
        hold: Some(Duration::from_secs(40)),
    })
    .expect("asked");
    assert_eq!(
        core.handle(delete(B, 1)),
        Ok(Vec::new()),
        "the lease holds it"
    );
    let mut expected = ends_failed(Failure::Engine);
    expected.push(refused.clone());
    assert_eq!(
        core.handle(Input::SendDeadline { lease: held }),
        Ok(expected)
    );
    assert_eq!(
        core.handle(delete(B, 1)),
        Ok(vec![refused]),
        "B stays fenced"
    );
    core.handle(Input::RunStored { run: R }).expect("ending");

    core.handle(Input::Turn {
        turn: 1,
        thread: Some(T),
        text: "again".into(),
    })
    .expect("a turn is never stale");
    core.handle(Input::TurnStored { run: R2 }).expect("current");
    core.handle(built(R2, 2, &[T])).expect("current");
    let next = LeaseId {
        key: build(R2, 2),
        send: 2,
    };
    assert_eq!(
        core.handle(Input::SendReady {
            key: next.key,
            hold: None,
        }),
        Ok(vec![
            Effect::Grant(next),
            Effect::ArmSendDeadline {
                lease: next,
                after: SEND_DEADLINE,
            },
        ]),
        "a later run is granted"
    );
    core.handle(Input::SendSettled { lease: next })
        .expect("granted");
    core.handle(engine(
        super::engine_key(&core, R2),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    assert_eq!(
        core.handle(Input::RunStored { run: R2 }),
        Ok(done_starting(R2, 3))
    );
    assert_eq!(
        core.handle(job_built(key(3), &[T])),
        Ok(vec![start_job(key(3), R2)])
    );
    assert_eq!(
        core.handle(ready(key(3))),
        Ok(granted(lease(key(3), 3))),
        "its title job is granted"
    );
}

/// The events every test compaction covers.
const COVERS: (EventId, EventId) = (EventId(20), EventId(31));

fn compact_built(key: JobKey, sources: &[ThreadId]) -> Input {
    Input::JobBuilt(JobBuilt {
        key,
        result: Ok(Some(JobAsk {
            generation: G,
            built: Built {
                recovered_context: None,
                request: request(),
                sources: sources.to_vec(),
            },
            covers: Some(COVERS),
        })),
    })
}

fn start_compact(key: JobKey, run: RunId) -> Effect {
    Effect::StartJob {
        job: key,
        kind: JobKind::Compact,
        request: RunRequest {
            key: SendKey::Job(key),
            run,
            thread: T,
            origin: Origin::Task(T),
            request: request(),
        },
    }
}

fn summarized(key: JobKey) -> Input {
    Input::JobDone {
        key,
        output: Some(JobOutput::Summary("Earlier".to_owned())),
    }
}

fn store_summary(key: JobKey) -> Effect {
    Effect::Store(StoreOp::StoreSummary {
        owner: SendKey::Job(key),
        thread: T,
        generation: G,
        covers: COVERS,
        text: SummaryText("Earlier".to_owned()),
        model: None,
    })
}

/// A coordinator from [`reading`] whose title job found nothing to do, so compaction job 3
/// read task 4 and its request was granted.
fn compacting() -> Coordinator {
    let mut core = reading();
    assert_eq!(
        core.handle(Input::JobBuilt(JobBuilt {
            key: key(2),
            result: Ok(None),
        })),
        Ok(vec![compacts(3)])
    );
    assert_eq!(
        core.handle(compact_built(key(3), &[T])),
        Ok(vec![start_compact(key(3), R)])
    );
    assert_eq!(core.handle(ready(key(3))), Ok(granted(lease(key(3), 1))));
    core
}

#[test]
fn formatting_the_summary_write_shows_its_length_never_its_text() {
    const SENTINEL: &str = "sentinel: the private summary of task four";
    let mut core = compacting();
    assert_eq!(
        core.handle(Input::SendSettled {
            lease: lease(key(3), 1)
        }),
        Ok(Vec::new())
    );
    let effects = core
        .handle(Input::JobDone {
            key: key(3),
            output: Some(JobOutput::Summary(SENTINEL.to_owned())),
        })
        .expect("the slot's own job is current");
    let [effect @ Effect::Store(op @ StoreOp::StoreSummary { .. })] = effects.as_slice() else {
        panic!("the job's only effect is its summary write");
    };
    let length = format!("SummaryText({})", SENTINEL.len());
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    let byte_list = byte_list.trim_matches(['[', ']']);
    for shown in [
        format!("{op:?}"),
        format!("{effect:?}"),
        format!("{effects:?}"),
    ] {
        assert!(!shown.contains("sentinel"), "the summary text leaked");
        assert!(!shown.contains(byte_list), "the summary bytes leaked");
        assert!(shown.contains(&length), "the length is missing");
        assert!(
            shown.contains("covers: (EventId(20), EventId(31))"),
            "the range is missing"
        );
    }
}

#[test]
fn compaction_follows_the_title_job_and_writes_its_summary_once() {
    let mut core = compacting();
    assert_eq!(
        core.handle(Input::SendSettled {
            lease: lease(key(3), 1)
        }),
        Ok(Vec::new())
    );
    assert_eq!(
        core.handle(summarized(key(3))),
        Ok(vec![store_summary(key(3))])
    );
    assert_eq!(
        core.handle(Input::SummaryStored {
            owner: SendKey::Job(key(3)),
            applied: true
        }),
        Ok(Vec::new()),
        "compaction hands over to nothing"
    );
    assert_eq!(
        core.handle(summarized(key(3))),
        Err(Stale),
        "the slot is empty"
    );
}

#[test]
fn a_new_run_cancels_compaction_and_its_old_summary_ack_touches_nothing() {
    let mut core = compacting();
    assert_eq!(
        core.handle(summarized(key(3))),
        Ok(vec![store_summary(key(3))])
    );
    assert_eq!(
        run_done(&mut core, Some(T), R2),
        done_starting(R2, 5),
        "the new run cancelled job 3 and its Done started job 5"
    );
    for applied in [true, false] {
        assert_eq!(
            core.handle(Input::SummaryStored {
                owner: SendKey::Job(key(3)),
                applied
            }),
            Ok(Vec::new())
        );
    }
    assert_eq!(
        core.handle(job_built(key(5), &[T])),
        Ok(vec![start_job(key(5), R2)]),
        "job 5 is still in the slot"
    );
}

#[test]
fn a_delete_during_compaction_stores_nothing() {
    let mut core = compacting();
    assert_eq!(
        core.handle(delete(T, 1)),
        Ok(Vec::new()),
        "the lease holds it"
    );
    assert_eq!(
        core.handle(Input::SendSettled {
            lease: lease(key(3), 1)
        }),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: T })])
    );
    assert_eq!(core.handle(summarized(key(3))), Ok(Vec::new()));
}

#[test]
fn a_delete_of_a_quoted_task_voids_kumos_compaction() {
    let mut core = signed_in();
    let kumo = JobKey {
        thread: MAIN,
        serial: 2,
    };
    run_done(&mut core, Some(MAIN), R);
    assert_eq!(
        core.handle(delete(T, 0)),
        Ok(vec![Effect::Store(StoreOp::Delete { thread: T })])
    );
    assert_eq!(
        core.handle(Input::JobBuilt(JobBuilt {
            key: kumo,
            result: Ok(Some(JobAsk {
                generation: G,
                built: Built {
                    recovered_context: None,
                    request: request(),
                    sources: vec![MAIN, T],
                },
                covers: Some(COVERS),
            })),
        })),
        Ok(Vec::new()),
        "the summary would have carried the deleted task's text"
    );
}

#[test]
fn sign_out_while_a_summary_request_is_held_stores_nothing() {
    let mut core = compacting();
    assert_eq!(
        core.handle(Input::SignedOut { generation: 1 }),
        Ok(vec![Effect::CancelJob(key(3))])
    );
    assert_eq!(core.handle(summarized(key(3))), Err(Stale));
    assert_eq!(core.handle(ready(key(3))), Ok(rebuild(key(3))));
}

#[test]
fn a_title_output_never_becomes_a_summary_or_the_reverse() {
    let mut core = compacting();
    assert_eq!(
        core.handle(job_done(key(3), Some(titled("Not a summary")))),
        Ok(Vec::new())
    );
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(
        core.handle(summarized(key(2))),
        Ok(Vec::new()),
        "a title request whose answer is unusable failed, so no compaction follows it"
    );
}

/// H-h, a job's send: the default, under the cap, exactly at it, and over it.
#[tokio::test(start_paused = true)]
async fn a_held_job_send_arms_its_hold_capped_at_165_seconds() {
    for (hold, seconds) in [
        (None, 15),
        (Some(20), 20),
        (Some(165), 165),
        (Some(166), 165),
    ] {
        let mut core = reading();
        core.handle(job_built(key(2), &[T])).expect("current");
        let lease = lease(key(2), 1);
        let after = Duration::from_secs(seconds);
        let timer = hold.map_or(Effect::ArmSendDeadline { lease, after }, |_| {
            Effect::ArmSendDeadlineAt {
                lease,
                at: tokio::time::Instant::now() + after,
            }
        });
        assert_eq!(
            core.handle(Input::SendReady {
                key: SendKey::Job(key(2)),
                hold: hold.map(Duration::from_secs),
            }),
            Ok(vec![Effect::Grant(lease), timer]),
            "{hold:?}"
        );
    }
}

#[test]
fn a_job_on_another_connections_choice_ends_unsent_and_frees_the_slot() {
    use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
    for connection in [ConnectionKind::ClaudeCode, ConnectionKind::AnthropicKey] {
        let elsewhere = |key| {
            let Input::JobBuilt(mut built) = job_built(key, &[T]) else {
                unreachable!("job_built builds a job's read");
            };
            if let Ok(Some(ask)) = &mut built.result {
                ask.built.request.choice = Some(ModelChoice {
                    connection,
                    model: ModelSlug::new("m").expect("slug"),
                    effort: Effort::Low,
                });
            }
            Input::JobBuilt(built)
        };
        let mut core = reading();
        assert_eq!(
            core.handle(elsewhere(key(2))),
            Ok(vec![compacts(3)]),
            "the title job ends unsent, and compaction follows: {connection:?}"
        );
        assert_eq!(
            core.handle(elsewhere(key(3))),
            Ok(Vec::new()),
            "compaction ends unsent too: {connection:?}"
        );
        assert_eq!(
            core.handle(ready(key(3))),
            Ok(rebuild(key(3))),
            "the slot is empty"
        );
    }
}

const CLAUDE: ConnectionKind = ConnectionKind::ClaudeCode;

fn haiku() -> ModelChoice {
    ModelChoice {
        connection: CLAUDE,
        model: ModelSlug::new("haiku").expect("slug"),
        effort: Effort::Low,
    }
}

fn ready_on(kind: ConnectionKind, tag: u64) -> Input {
    Input::Engines(
        kind,
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: vec![ModelInfo {
                slug: ModelSlug::new("haiku").expect("slug"),
                display_name: ModelName::new("Haiku").expect("name"),
                supported_efforts: None,
            }],
            trusted_draw: false,
            images: Vec::new(),
        }),
    )
}

/// Takes a turn in `thread` (a new task when `None`) on haiku over Claude Code and runs it to a
/// stored `Done`; what that stored outcome did.
fn claude_run_done(core: &mut Coordinator, thread: Option<ThreadId>, run: RunId) -> Vec<Effect> {
    run_done_on(core, thread, run, haiku())
}

/// [`claude_run_done`] on `choice`.
fn run_done_on(
    core: &mut Coordinator,
    thread: Option<ThreadId>,
    run: RunId,
    choice: ModelChoice,
) -> Vec<Effect> {
    core.handle(Input::Turn {
        turn: 1,
        thread,
        text: "Plan a trip".into(),
    })
    .expect("a turn is never stale");
    let effects = core.handle(Input::TurnStored { run }).expect("current");
    let attempt = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key.attempt),
            _ => None,
        })
        .expect("a build starts");
    core.handle(Input::ContextBuilt(ContextBuilt {
        key: BuildKey {
            run,
            round: 1,
            attempt,
        },
        snapshot: Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(choice),
            connection: ConnectionTag(7),
        }),
        result: Ok(Built {
            recovered_context: None,
            request: request(),
            sources: vec![T],
        }),
    }))
    .expect("admitted");
    core.handle(engine(
        super::engine_key(core, run),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    core.handle(Input::RunStored { run }).expect("ending")
}

/// Only Claude Code is ready, as sign-in 7, and `ChatGPT` never signed in: run 10 in new task 4
/// ended `Done` on haiku, and job 2's read is out.
fn claude_reading() -> Coordinator {
    let mut core = Coordinator::new(MAIN, ThreadId(3), RunId(9), Vec::new());
    core.handle(ready_on(CLAUDE, 7)).expect("never stale");
    assert_eq!(claude_run_done(&mut core, None, R), done_starting(R, 2));
    core
}

/// A key job is granted only while the key's checked catalog still offers its model: a same-key
/// refresh that drops it between the job's start and its grant ends the job unsent.
#[test]
fn a_key_job_whose_model_a_refresh_dropped_ends_unsent_at_the_grant() {
    let on_key = ModelChoice {
        connection: ConnectionKind::AnthropicKey,
        ..haiku()
    };
    for (refreshed, grants) in [(&["haiku"][..], true), (&[][..], false)] {
        let mut core = Coordinator::new(MAIN, ThreadId(3), RunId(9), Vec::new());
        core.handle(ready_on(ConnectionKind::AnthropicKey, 7))
            .expect("never stale");
        assert_eq!(
            run_done_on(&mut core, None, R, on_key.clone()),
            done_starting(R, 2)
        );
        assert_eq!(
            core.handle(job_built_on(key(2), Some(on_key.clone()))),
            Ok(vec![start_job_on(key(2), R, Some(on_key.clone()))])
        );
        core.handle(Input::Engines(
            ConnectionKind::AnthropicKey,
            Availability::Ready(Ready {
                connection: ConnectionTag(7),
                models: refreshed
                    .iter()
                    .map(|slug| ModelInfo {
                        slug: ModelSlug::new(slug).expect("slug"),
                        display_name: ModelName::new("Haiku").expect("name"),
                        supported_efforts: None,
                    })
                    .collect(),
                images: Vec::new(),
                trusted_draw: false,
            }),
        ))
        .expect("the same key's refreshed catalog");
        let expected = if grants {
            granted(lease(key(2), 1))
        } else {
            vec![
                Effect::Rebuild(SendKey::Job(key(2))),
                Effect::CancelJob(key(2)),
            ]
        };
        assert_eq!(core.handle(ready(key(2))), Ok(expected), "{refreshed:?}");
    }
}

/// Job `key`'s read, finished, on a request naming `choice`.
fn job_built_on(key: JobKey, choice: Option<ModelChoice>) -> Input {
    let Input::JobBuilt(mut built) = job_built(key, &[T]) else {
        unreachable!("job_built builds a job's read");
    };
    if let Ok(Some(ask)) = &mut built.result {
        ask.built.request.choice = choice;
    }
    Input::JobBuilt(built)
}

fn chatgpt_catalog(tag: u64, model: &str, efforts: Option<EffortSet>) -> Input {
    Input::Engines(
        ConnectionKind::ChatGpt,
        Availability::Ready(Ready {
            connection: ConnectionTag(tag),
            models: vec![ModelInfo {
                slug: ModelSlug::new(model).unwrap(),
                display_name: ModelName::new("Sparse").unwrap(),
                supported_efforts: efforts,
            }],
            trusted_draw: false,
            images: Vec::new(),
        }),
    )
}

fn chatgpt_choice(effort: Effort) -> ModelChoice {
    ModelChoice {
        connection: ConnectionKind::ChatGpt,
        model: ModelSlug::new("sparse").unwrap(),
        effort,
    }
}

#[test]
fn chatgpt_title_and_summary_grant_separately_resolved_efforts() {
    let mut core = reading();
    core.handle(chatgpt_catalog(
        0,
        "sparse",
        EffortSet::new([Effort::Medium, Effort::High]),
    ))
    .unwrap();
    let title = crate::jobs::title_request(
        &RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").unwrap(),
            user: None,
            choice: Some(chatgpt_choice(Effort::XHigh)),
            connection: ConnectionTag(0),
        },
        "Plan a trip",
    );
    let Input::JobBuilt(mut done) = job_built(key(2), &[T]) else {
        unreachable!();
    };
    done.result
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .built
        .request = title;
    let body = |effects: &[Effect], job_key, job_kind| {
        let [Effect::StartJob { job, kind, request }] = effects else {
            panic!("the job starts");
        };
        assert_eq!((*job, *kind), (job_key, job_kind));
        crate::engines::responses::body(&request.request).unwrap()
    };
    let effects = core.handle(Input::JobBuilt(done)).unwrap();
    let sent = body(&effects, key(2), JobKind::Title);
    assert_eq!(sent["model"], "sparse");
    assert_eq!(sent["reasoning"]["effort"], "medium");
    assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
    core.handle(Input::SendSettled {
        lease: lease(key(2), 1),
    })
    .unwrap();
    assert_eq!(
        core.handle(job_done(key(2), Some(titled("Lisbon trip")))),
        Ok(vec![set_title(key(2), "Lisbon trip")]),
    );
    assert_eq!(
        core.handle(Input::TitleSet {
            job: Some(key(2)),
            applied: true
        }),
        Ok(vec![nudge(), compacts(3)])
    );
    let Input::JobBuilt(mut done) = compact_built(key(3), &[T]) else {
        unreachable!();
    };
    done.result
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .built
        .request
        .choice = Some(chatgpt_choice(Effort::XHigh));
    let effects = core.handle(Input::JobBuilt(done)).unwrap();
    let sent = body(&effects, key(3), JobKind::Compact);
    assert_eq!(sent["model"], "sparse");
    assert_eq!(sent["reasoning"]["effort"], "high");
    assert_eq!(core.handle(ready(key(3))), Ok(granted(lease(key(3), 2))));
}

#[test]
fn chatgpt_jobs_without_a_matching_known_catalog_end_unsent() {
    let low = EffortSet::new([Effort::Low]);
    let wanted = Some(chatgpt_choice(Effort::High));
    for (tag, model, efforts, choice) in [
        (0, "sparse", None, wanted.clone()),
        (0, "different", low, wanted.clone()),
        (1, "sparse", low, wanted),
        (0, "sparse", low, None),
        (
            0,
            "sparse",
            low,
            Some(ModelChoice {
                connection: ConnectionKind::AnthropicKey,
                ..chatgpt_choice(Effort::Low)
            }),
        ),
    ] {
        let mut core = reading();
        core.handle(chatgpt_catalog(tag, model, efforts)).unwrap();
        let title = core.handle(job_built_on(key(2), choice.clone()));
        if tag == 0 {
            assert_eq!(title, Ok(vec![compacts(3)]));
            assert_eq!(core.handle(job_built_on(key(3), choice)), Ok(Vec::new()));
        } else {
            assert_eq!(title, Ok(Vec::new()), "the retired route can't read again");
            assert_eq!(core.handle(job_built_on(key(3), choice)), Err(Stale));
        }
        assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
        assert_eq!(core.handle(ready(key(3))), Ok(rebuild(key(3))));
    }
}

/// Job `key`'s title request of `run`, naming `choice`.
fn start_job_on(key: JobKey, run: RunId, choice: Option<ModelChoice>) -> Effect {
    let Effect::StartJob {
        job,
        kind,
        mut request,
    } = start_job(key, run)
    else {
        unreachable!("start_job starts a job");
    };
    request.request.choice = choice;
    Effect::StartJob { job, kind, request }
}

/// Test 1: a Claude Code run's `Done` starts its title job with no `ChatGPT` account, and the
/// job's request goes out on the run's own Claude Code choice and is granted.
#[test]
fn a_claude_code_runs_done_titles_it_on_claude_code_with_no_chatgpt_account() {
    let mut core = claude_reading();
    assert_eq!(
        core.handle(job_built_on(key(2), Some(haiku()))),
        Ok(vec![start_job_on(key(2), R, Some(haiku()))])
    );
    assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
}

/// Test 2: a `ChatGPT` run's `Done` starts no job while no `ChatGPT` account is signed in, even
/// with the fixture's `ChatGPT` availability ready and Claude Code ready too.
#[test]
fn a_chatgpt_runs_done_starts_no_job_signed_out_while_claude_code_is_ready() {
    let mut core = fresh();
    core.handle(ready_on(CLAUDE, 7)).expect("never stale");
    assert_eq!(
        run_done(&mut core, None, R),
        vec![state(R, T, Phase::Done), run_end(R)]
    );
}

/// Test 3: a Claude Code job whose read names another connection, because the thread's choice
/// changed since its run, ends unsent and frees the slot.
#[test]
fn a_claude_code_job_whose_read_names_another_connection_ends_unsent() {
    let luna = ModelChoice {
        connection: ConnectionKind::ChatGpt,
        model: ModelSlug::new("gpt-6-luna").expect("slug"),
        effort: Effort::Low,
    };
    for choice in [None, Some(luna)] {
        let mut core = claude_reading();
        assert_eq!(
            core.handle(job_built_on(key(2), choice.clone())),
            Ok(vec![compacts(3)]),
            "the title job ends unsent, and compaction follows: {choice:?}"
        );
        assert_eq!(
            core.handle(job_built_on(key(3), choice.clone())),
            Ok(Vec::new()),
            "compaction ends unsent too: {choice:?}"
        );
        assert_eq!(
            core.handle(ready(key(3))),
            Ok(rebuild(key(3))),
            "the slot is empty"
        );
    }
}

/// Test 4: a Claude Code job ends when Claude Code becomes unavailable or draws another sign-in,
/// never on `ChatGPT`'s sign-in or sign-out, and it takes no `ChatGPT` bearer.
#[test]
fn a_claude_code_job_follows_claude_codes_fences_and_not_chatgpts() {
    let mut core = claude_reading();
    assert_eq!(
        core.handle(Input::SignedIn { generation: 1 }),
        Ok(Vec::new()),
        "ChatGPT's sign-in leaves it"
    );
    core.handle(ready_on(ConnectionKind::ChatGpt, 8))
        .expect("never stale");
    assert_eq!(
        core.sending(SendKey::Job(key(2))),
        None,
        "no ChatGPT bearer"
    );
    assert_eq!(
        core.sending(SendKey::Job(key(9))),
        None,
        "an unknown job can't take a ChatGPT bearer"
    );
    assert_eq!(
        core.handle(Input::SignedOut { generation: 1 }),
        Ok(Vec::new()),
        "ChatGPT's sign-out leaves it"
    );
    assert_eq!(
        core.handle(job_built_on(key(2), Some(haiku()))),
        Ok(vec![start_job_on(key(2), R, Some(haiku()))]),
        "it's still in the slot"
    );
    assert_eq!(
        core.handle(Input::Engines(
            CLAUDE,
            Availability::Unavailable(model_unavailable())
        )),
        Ok(vec![Effect::CancelJob(key(2))])
    );

    let mut core = claude_reading();
    assert_eq!(
        core.handle(ready_on(CLAUDE, 7)),
        Ok(Vec::new()),
        "the same sign-in keeps it"
    );
    assert_eq!(
        core.handle(ready_on(CLAUDE, 8)),
        Ok(vec![Effect::CancelJob(key(2))]),
        "another sign-in ends it"
    );
}

/// Test 8: a Claude Code job's held send past its hold ends only that job; sends didn't fail
/// closed, so the next run's title job still starts.
#[tokio::test(start_paused = true)]
async fn a_claude_code_jobs_held_send_past_its_hold_ends_only_the_job() {
    let mut core = claude_reading();
    core.handle(job_built_on(key(2), Some(haiku())))
        .expect("current");
    let hold = Duration::from_secs(60);
    let at = tokio::time::Instant::now() + hold;
    assert_eq!(
        core.handle(Input::SendReady {
            key: SendKey::Job(key(2)),
            hold: Some(hold),
        }),
        Ok(vec![
            Effect::Grant(lease(key(2), 1)),
            Effect::ArmSendDeadlineAt {
                lease: lease(key(2), 1),
                at,
            },
        ])
    );
    assert_eq!(
        core.handle(Input::SendDeadline {
            lease: lease(key(2), 1)
        }),
        Ok(vec![Effect::CancelJob(key(2))])
    );
    assert_eq!(
        claude_run_done(&mut core, Some(T), R2),
        done_starting(R2, 4),
        "the next run's job starts"
    );
}

/// The job's grant checks its route as a run's send does: a `ChatGPT` job whose sign-in changed
/// since its run is dropped unsent.
#[test]
fn a_job_whose_sign_in_changed_is_dropped_at_its_grant() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    core.handle(ready_on(ConnectionKind::ChatGpt, 8))
        .expect("never stale");
    let mut dropped = rebuild(key(2));
    dropped.push(Effect::CancelJob(key(2)));
    assert_eq!(core.handle(ready(key(2))), Ok(dropped));
}

#[test]
fn an_obsolete_job_cannot_read_history_or_select_the_next_accounts_bearer() {
    for thread in [T, MAIN] {
        let mut core = reading();
        let old = SendKey::Job(key(2));
        assert_eq!(
            core.handle(job_built(key(2), &[T])),
            Ok(vec![start_job(key(2), R)])
        );
        assert_eq!(core.sending(old), Some(ConnectionTag(0)));

        core.handle(Input::Turn {
            turn: 2,
            thread: Some(thread),
            text: "Finish this on the original account".into(),
        })
        .unwrap();
        admit(&mut core, R2);
        core.handle(engine(
            super::engine_key(&core, R2),
            TurnEvent::Finished(Finish::Completed),
        ))
        .unwrap();
        core.handle(Input::SignedIn { generation: 2 }).unwrap();
        core.handle(chatgpt_catalog(7, "sparse", EffortSet::new([Effort::Low])))
            .unwrap();
        assert_eq!(
            (core.sending(old), core.handle(Input::RunStored { run: R2 })),
            (None, Ok(vec![state(R2, thread, Phase::Done), run_end(R2)])),
            "neither bearer selection nor a late receipt can move A's job to B"
        );

        let run = RunId(R2.0 + 1);
        let choice = chatgpt_choice(Effort::Low);
        let effects = run_done_on(&mut core, Some(T), run, choice.clone());
        let [Effect::Store(StoreOp::ReadJob(read)), ..] = effects.as_slice() else {
            panic!("the current account must read for its own title job");
        };
        let key = read.key;
        let current = SendKey::Job(key);
        assert_eq!(read.kind, JobKind::Title);
        assert_eq!(core.sending(current), None, "a read is not yet a send");
        assert_eq!(
            core.handle(job_built_on(key, Some(choice.clone()))),
            Ok(vec![start_job_on(key, run, Some(choice))])
        );
        assert_eq!(
            core.sending(old),
            None,
            "B's sending slot cannot lend its bearer"
        );
        assert_eq!(core.sending(current), Some(ConnectionTag(7)));
        assert_eq!(core.handle(ready(key)), Ok(granted(lease(key, 1))));
    }
}

/// A signed-in coordinator whose run 10 in new task 4 ended `Done` after `ChatGPT` recovered with
/// `cut`, and what job 2's read, answered with a body read to `body`, did.
fn recovered_reading(cut: Boundary, body: Option<Boundary>) -> (Coordinator, Vec<Effect>) {
    let mut core = signed_in();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "Plan a trip".into(),
    })
    .expect("a turn is never stale");
    admit(&mut core, R);
    core.recovered_chatgpt = vec![cut];
    core.handle(engine(
        super::engine_key(&core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current");
    let Effect::Store(StoreOp::ReadJob(mut read)) = read_job(key(2), JobKind::Title) else {
        unreachable!("read_job reads a job");
    };
    read.require_recovery = Some(cut);
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![
            Effect::Store(StoreOp::ReadJob(read)),
            state(R, T, Phase::Done),
            run_end(R)
        ]),
        "the job's read keeps to the cutoff"
    );
    let Input::JobBuilt(mut built) = job_built(key(2), &[T]) else {
        unreachable!("job_built builds a job's read");
    };
    if let Ok(Some(ask)) = &mut built.result {
        ask.built.recovered_context = body;
    }
    let effects = core.handle(Input::JobBuilt(built)).expect("current");
    (core, effects)
}

/// A recovered connection's job starts only on a body read to its exact cutoff, and its bearer and
/// grant check the cutoff again.
#[test]
fn a_recovered_chatgpt_job_reads_and_sends_only_to_its_cutoff() {
    let cut = Boundary::fixture(0, 3, 9);
    let other = Boundary::fixture(0, 3, 10);
    let Effect::Store(StoreOp::ReadJob(mut compact)) = compacts(3) else {
        unreachable!("compacts reads a job");
    };
    compact.require_recovery = Some(cut);
    for body in [None, Some(other)] {
        let (mut core, effects) = recovered_reading(cut, body);
        assert_eq!(
            effects,
            vec![Effect::Store(StoreOp::ReadJob(compact.clone()))],
            "ends unsent and compaction reads to the cutoff: {body:?}"
        );
        assert_eq!(
            core.handle(ready(key(2))),
            Ok(rebuild(key(2))),
            "the slot is empty"
        );
    }
    let (mut core, effects) = recovered_reading(cut, Some(cut));
    assert_eq!(effects, vec![start_job(key(2), R)]);
    assert_eq!(core.sending(SendKey::Job(key(2))), Some(ConnectionTag(0)));
    core.recovered_chatgpt = vec![other];
    assert_eq!(core.sending(SendKey::Job(key(2))), None);
    assert_eq!(
        core.handle(ready(key(2))),
        Ok(vec![
            Effect::Rebuild(SendKey::Job(key(2))),
            Effect::CancelJob(key(2))
        ])
    );
}

/// A report that refuses the recovered sign-in, a conflicting cutoff or one for another sign-in,
/// cancels its job queued or sending, so no refresh grants it and a late completion writes
/// nothing.
#[test]
fn a_refused_recovery_cancels_the_sending_job_and_its_late_answer_is_stale() {
    let cut = Boundary::fixture(0, 3, 9);
    for (boundary, sending) in [
        (Boundary::fixture(0, 3, 10), false),
        (Boundary::fixture(0, 3, 10), true),
        (Boundary::fixture(5, 3, 9), false),
        (Boundary::fixture(5, 3, 9), true),
    ] {
        let (mut core, effects) = recovered_reading(cut, Some(cut));
        assert_eq!(effects, vec![start_job(key(2), R)]);
        if sending {
            assert_eq!(core.handle(ready(key(2))), Ok(granted(lease(key(2), 1))));
        }
        assert_eq!(
            core.handle(Input::RecoveredChatGpt {
                ready: Ready::fixture(),
                boundary,
            }),
            Ok(vec![Effect::CancelJob(key(2))]),
            "{boundary:?}"
        );
        core.handle(ready_on(ConnectionKind::ChatGpt, 0))
            .expect("never stale");
        assert_eq!(
            core.sending(SendKey::Job(key(2))),
            None,
            "a refresh is no evidence"
        );
        assert_eq!(core.handle(ready(key(2))), Ok(rebuild(key(2))));
        assert_eq!(
            core.handle(job_done(key(2), Some(titled("Late")))),
            Err(Stale)
        );
    }
}
