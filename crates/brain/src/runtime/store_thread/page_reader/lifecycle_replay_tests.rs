use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::context::{ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::{
    BuildContext, BuildKey, CompactContext, JobKey, JobKind, ReadJob, Unbuilt,
};
use crate::dispatch::ToolOutput;
use crate::engines::{CallId, Opaque};
use crate::recall::{
    Selection,
    control::{Control, tests::Gate},
};
use crate::store::{Generation, RunEnd, Store};
use crate::test_support::Scratch;
use kumochi_protocol::message::{AssistantName, EventId, RunId, ThreadId};

fn replay_store(scratch: &Scratch) -> Store {
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    assert_eq!(store.main_thread().unwrap(), ThreadId(1));
    store
        .begin_turn(
            ThreadId(2),
            Some("source"),
            RunId(1),
            "private-replay-marker",
        )
        .unwrap();
    store
        .end_run(ThreadId(2), RunId(1), &[], RunEnd::Done)
        .unwrap();
    store
        .begin_turn(ThreadId(1), None, RunId(2), "recall")
        .unwrap();
    store.append_round((ThreadId(1), RunId(2)), crate::context::Route { kind: kumochi_protocol::message::ConnectionKind::ChatGpt, tag: ConnectionTag(0) }, None,
        &[Opaque::new(br#"{"type":"function_call","call_id":"recall","name":"read_history","arguments":"{}"}"#.to_vec()).unwrap()],
        &[(CallId::new("recall").unwrap(), ToolOutput::Recall(Selection::Read {
            task: 2, first_shown: Some(1), last_shown: Some(1), cut: None, next: None,
        }))]).unwrap();
    store
}

fn snapshot() -> RunSnapshot {
    RunSnapshot {
        origin: Origin::Kumo,
        assistant: AssistantName::new("Kumo").unwrap(),
        user: None,
        choice: None,
        connection: ConnectionTag(0),
    }
}

fn build(control: Control) -> BuildContext {
    BuildContext {
        control,
        key: BuildKey {
            run: RunId(2),
            round: 2,
            attempt: 1,
        },
        thread: ThreadId(1),
        origin: Origin::Kumo,
        snapshot: Some(snapshot()),
        first: false,
        arrived: crate::coordinator::Arrived::default(),
        drawings: crate::drawing::Digest::default(),
        picture_models: Vec::new(),
        key_results: Vec::new(),
    }
}

fn exhausted(control: &Control) {
    *control.test.time.lock().unwrap() = Some(control.end);
}

fn ordinary_read_after_cleanup(store: &Store, control: &Control) {
    let (entered, _) = mpsc::channel();
    let (_, release) = mpsc::channel();
    *control.test.gate.lock().unwrap() = Some(Gate { entered, release });
    let total = store.side().read(|conn| Ok(conn.query_row::<i64, _, _>(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT sum(x) FROM n",
        [], |row| row.get(0))?)).unwrap();
    assert_eq!(total, 500_000_500_000);
}

#[test]
fn lifecycle_replay_deadline_is_budget_not_store_failure_or_partial_request() {
    let scratch = Scratch::new("recall-build-budget");
    let store = replay_store(&scratch);
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    let side = store.side();
    let built = super::context::read(
        Some(&side),
        build(control.clone()),
        Default::default(),
        || exhausted(&control),
    );
    assert_eq!(built.result, Err(Unbuilt::TooLarge { room: None }));
    ordinary_read_after_cleanup(&store, &control);
}

#[test]
fn lifecycle_background_compaction_remains_text_only_with_an_inactive_handler() {
    let scratch = Scratch::new("recall-compact-budget");
    let mut store = replay_store(&scratch);
    store
        .end_run(ThreadId(1), RunId(2), &[], RunEnd::Done)
        .unwrap();
    for run in 3..=11 {
        store
            .begin_turn(ThreadId(1), None, RunId(run), "later")
            .unwrap();
        store
            .end_run(ThreadId(1), RunId(run), &[], RunEnd::Done)
            .unwrap();
    }
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    let side = store.side();
    let read = ReadJob {
        control: control.clone(),
        key: JobKey {
            thread: ThreadId(1),
            serial: 1,
        },
        kind: JobKind::Compact,
        origin: Origin::Kumo,
        snapshot: Some(snapshot()),
        compact_at: 0,
    };
    let built = super::context::read_job(Some(&side), read, Default::default(), || {
        exhausted(&control)
    });
    let ask = built.result.unwrap().unwrap();
    assert_eq!(ask.generation, Generation(0));
    assert_eq!(ask.covers, Some((EventId(2), EventId(5))));
    assert_eq!(ask.built.sources, [ThreadId(1)]);
    assert_eq!(
        ask.built.request.input,
        vec![
            crate::context::InputItem::Message {
                role: crate::context::Role::User,
                text: "recall".into()
            },
            crate::context::InputItem::Message {
                role: crate::context::Role::User,
                text: "later".into()
            },
            crate::context::InputItem::Message {
                role: crate::context::Role::Developer,
                text: "Write the new summary now.".into()
            },
        ]
    );
    ordinary_read_after_cleanup(&store, &control);
}

#[test]
fn lifecycle_inline_summary_sql_does_not_activate_the_recall_handler() {
    let scratch = Scratch::new("recall-inline-inactive");
    let mut store = replay_store(&scratch);
    assert!(
        store
            .store_summary(
                ThreadId(1),
                Generation(0),
                (EventId(2), EventId(2)),
                "summary",
                None
            )
            .unwrap()
    );
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    let side = store.side();
    let read = CompactContext {
        control: control.clone(),
        key: build(control.clone()).key,
        thread: ThreadId(1),
        snapshot: Some(snapshot()),
        room: 1000,
    };
    let built = super::context::read_compact(Some(&side), read, || exhausted(&control));
    let (ask, allowance) = built.result.unwrap().unwrap();
    assert_eq!(ask.generation, Generation(0));
    assert_eq!(ask.covers, Some((EventId(2), EventId(2))));
    assert_eq!(ask.built.sources, [ThreadId(1)]);
    assert_eq!(allowance, 908);
    ordinary_read_after_cleanup(&store, &control);
}

#[test]
fn lifecycle_expired_barrier_answers_remain_budget_errors() {
    let control = Control::new(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
    let built = super::context::read(None, build(control.clone()), Default::default(), || {
        panic!("no snapshot admitted")
    });
    assert_eq!(built.result, Err(Unbuilt::TooLarge { room: None }));
    let compact = CompactContext {
        control: control.clone(),
        key: build(control.clone()).key,
        thread: ThreadId(1),
        snapshot: Some(snapshot()),
        room: 1000,
    };
    let compact = super::context::read_compact(None, compact, || panic!("no snapshot admitted"));
    assert_eq!(compact.result, Err(Unbuilt::TooLarge { room: None }));
    let job = ReadJob {
        control,
        key: JobKey {
            thread: ThreadId(1),
            serial: 1,
        },
        kind: JobKind::Compact,
        origin: Origin::Kumo,
        snapshot: Some(snapshot()),
        compact_at: 0,
    };
    let job = super::context::read_job(None, job, Default::default(), || {
        panic!("no snapshot admitted")
    });
    assert_eq!(job.result, Err(Unbuilt::TooLarge { room: None }));
}

#[test]
fn lifecycle_hook_failures_refuse_the_whole_build() {
    for point in [1, 2, 3] {
        let scratch = Scratch::new("recall-build-hook-failure");
        let store = replay_store(&scratch);
        let control = Control::new(Instant::now() + Duration::from_secs(60));
        control
            .test
            .fault
            .store(point, std::sync::atomic::Ordering::SeqCst);
        let side = store.side();
        let built = super::context::read(
            Some(&side),
            build(control.clone()),
            Default::default(),
            || (),
        );
        assert_eq!(built.result, Err(Unbuilt::Failed), "hook point {point}");
        ordinary_read_after_cleanup(&store, &control);
    }
}

#[test]
fn lifecycle_replayed_content_is_redacted_at_the_outer_reply() {
    let scratch = Scratch::new("recall-lifecycle-debug");
    let store = replay_store(&scratch);
    let control = Control::new(Instant::now() + Duration::from_secs(60));
    let side = store.side();
    let built = super::context::read(Some(&side), build(control), Default::default(), || ());
    let request = &built.result.as_ref().unwrap().request;
    assert!(request.input.iter().any(|item| matches!(item, crate::context::InputItem::FunctionOutput { output, .. } if output.contains("private-replay-marker"))));
    let diagnostic = format!("{:?}", crate::coordinator::Input::ContextBuilt(built));
    assert!(!diagnostic.contains("private-replay-marker"));
    assert!(!diagnostic.contains(&format!("{:?}", b"private-replay-marker")));
}
