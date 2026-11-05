//! The background job slot: a task's title job after a run ends `Done`, fenced by new runs,
//! deletes and sign-out, and keyed so an old job's answers never touch a new one.

use kumochi_protocol::message::{ControlToShell, EventId, Phase, RunId, ThreadId, ThreadTitle};

use super::{R, T, admit, engine, fresh, request, run_end, state};
use crate::context::compact::COMPACT_AT;
use crate::context::{Built, Origin};
use crate::coordinator::{
    Coordinator, Effect, Input, JobAsk, JobBuilt, JobKey, JobKind, JobOutput, LeaseId, ReadJob,
    SEND_DEADLINE, SendKey, Stale, StoreOp, SummaryText, Unbuilt,
};
use crate::engines::{Finish, RunRequest, TurnEvent};
use crate::store::{Generation, TitleSource};

/// The deletion generation every test read finds.
const G: Generation = Generation(5);
/// Run 10's successor in task 4.
const R2: RunId = RunId(11);
/// Kumo's thread, as `fresh` makes it.
const MAIN: ThreadId = ThreadId(1);

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
        text: "Plan a trip".to_owned(),
    })
    .expect("a turn is never stale");
    admit(core, run);
    core.handle(engine(run, TurnEvent::Finished(Finish::Completed)))
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
            text: "More".to_owned(),
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
fn a_failed_job_keeps_the_first_words_and_the_next_done_retries() {
    let mut core = reading();
    assert_eq!(
        core.handle(job_built(key(2), &[T])),
        Ok(vec![start_job(key(2), R)])
    );
    assert_eq!(core.handle(job_done(key(2), None)), Ok(vec![compacts(3)]));
    assert_eq!(
        run_done(&mut core, Some(T), R2),
        done_starting(R2, 5),
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
        text: "Plan a trip".to_owned(),
    })
    .expect("a turn is never stale");
    admit(&mut core, R);
    core.handle(engine(R, TurnEvent::Finished(Finish::Completed)))
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

/// The events every test compaction covers.
const COVERS: (EventId, EventId) = (EventId(20), EventId(31));

fn compact_built(key: JobKey, sources: &[ThreadId]) -> Input {
    Input::JobBuilt(JobBuilt {
        key,
        result: Ok(Some(JobAsk {
            generation: G,
            built: Built {
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
    assert_eq!(core.handle(summarized(key(2))), Ok(vec![compacts(3)]));
}
