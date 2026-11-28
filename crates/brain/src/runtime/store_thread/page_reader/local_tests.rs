//! A local task on the page reader: its first build reads its own plain text and nothing a build
//! adds, and no title or summary is read for it.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    AssistantName, ChoiceRead, ConnectionKind, Effort, ModelChoice, ModelSlug, RunId, ThreadId,
};

use crate::config::tuning::ContextTuning;
use crate::context::{ConnectionTag, InputItem, Origin, Role, Route, RunSnapshot};
use crate::coordinator::{
    BuildContext, BuildKey, CompactContext, JobKey, JobKind, ReadJob, Unbuilt,
};
use crate::recall::control::Control;
use crate::store::{EventKind, NewEvent, RunEnd, Store};
use crate::test_support::Scratch;

const TASK: ThreadId = ThreadId(2);

fn on(connection: ConnectionKind) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new("local-model").expect("slug"),
        effort: Effort::Low,
        connection,
    }
}

/// Task 2 answered once, on `connection`, and asked again as run 2.
fn task_on(scratch: &Scratch, connection: ConnectionKind) -> Store {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recovered");
    store
        .begin_turn(TASK, Some("task"), RunId(1), "first ask")
        .expect("turn");
    store
        .end_run(
            TASK,
            RunId(1),
            &[NewEvent {
                kind: EventKind::AssistantText,
                text: Some("first answer"),
                body: None,
            }],
            RunEnd::Done,
        )
        .expect("ended");
    assert!(matches!(
        store.set_model(TASK, Some(&on(connection))).expect("saved"),
        ChoiceRead::Found { .. }
    ));
    store
        .begin_turn(TASK, None, RunId(2), "second ask")
        .expect("turn");
    store
}

fn snapshot() -> RunSnapshot {
    RunSnapshot {
        origin: Origin::Task(TASK),
        assistant: AssistantName::new("Kumo").expect("name"),
        user: None,
        choice: None,
        connection: ConnectionTag(0),
    }
}

fn control() -> Control {
    Control::new(Instant::now() + Duration::from_secs(60))
}

/// A picture route that names the task's model, which a cloud build would picture.
fn build(control: Control, connection: ConnectionKind) -> BuildContext {
    BuildContext {
        control,
        key: BuildKey {
            run: RunId(2),
            round: 1,
            attempt: 1,
        },
        thread: TASK,
        origin: Origin::Task(TASK),
        snapshot: Some(snapshot()),
        first: true,
        arrived: crate::coordinator::Arrived::default(),
        drawings: crate::drawing::Digest::default(),
        picture_models: vec![(
            Route {
                kind: connection,
                tag: ConnectionTag(0),
            },
            vec![ModelSlug::new("local-model").expect("slug")],
        )],
        key_results: Vec::new(),
    }
}

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

#[test]
fn a_local_tasks_build_is_its_own_text_alone() {
    let scratch = Scratch::new("page-local-build");
    let store = task_on(&scratch, ConnectionKind::LocalChat);
    let side = store.side();
    let built = super::context::read(
        Some(&side),
        build(control(), ConnectionKind::LocalChat),
        ContextTuning::default(),
        || (),
    );
    let built = built.result.expect("built");
    assert_eq!(
        built.request.input,
        vec![
            message(Role::User, "first ask"),
            message(Role::Assistant, "first answer"),
            message(Role::User, "second ask"),
        ]
    );
    assert_eq!(built.request.tools, Vec::new());
    assert_eq!(built.request.choice, Some(on(ConnectionKind::LocalChat)));
    assert_eq!(built.sources, vec![TASK]);
}

/// A cancel that lands once the read began still ends a local task's build.
#[test]
fn a_local_build_cancelled_mid_read_builds_nothing() {
    let scratch = Scratch::new("page-local-cancel");
    let store = task_on(&scratch, ConnectionKind::LocalChat);
    let side = store.side();
    let control = control();
    let cancel = control.clone();
    let built = super::context::read(
        Some(&side),
        build(control, ConnectionKind::LocalChat),
        ContextTuning::default(),
        move || cancel.cancel(),
    );
    assert!(matches!(built.result, Err(Unbuilt::Failed)));
}

/// The same task on a cloud route gets the usual build: its local time and its tools.
#[test]
fn the_same_task_on_a_cloud_route_builds_as_usual() {
    let scratch = Scratch::new("page-cloud-build");
    let store = task_on(&scratch, ConnectionKind::AnthropicKey);
    let side = store.side();
    let built = super::context::read(
        Some(&side),
        build(control(), ConnectionKind::AnthropicKey),
        ContextTuning::default(),
        || (),
    )
    .result
    .expect("built");
    assert!(!built.request.tools.is_empty());
    assert_ne!(
        built.request.input.last(),
        Some(&message(Role::User, "second ask"))
    );
}

#[test]
fn no_title_or_summary_is_read_for_a_local_task() {
    for (connection, titled) in [
        (ConnectionKind::AnthropicKey, true),
        (ConnectionKind::LocalChat, false),
    ] {
        let scratch = Scratch::new("page-local-jobs");
        let store = task_on(&scratch, connection);
        let side = store.side();
        let read = ReadJob {
            control: control(),
            key: JobKey {
                thread: TASK,
                serial: 1,
            },
            kind: JobKind::Title,
            origin: Origin::Task(TASK),
            snapshot: Some(snapshot()),
            compact_at: 0,
        };
        let job = super::context::read_job(Some(&side), read, ContextTuning::default(), || ());
        assert_eq!(
            job.result.expect("read").is_some(),
            titled,
            "{connection:?}"
        );
        let compact = CompactContext {
            control: control(),
            key: build(control(), connection).key,
            thread: TASK,
            snapshot: Some(RunSnapshot {
                choice: Some(on(connection)),
                ..snapshot()
            }),
            room: 1000,
        };
        let compact = super::context::read_compact(Some(&side), compact, || ());
        assert_eq!(
            compact
                .result
                .is_err_and(|failed| failed == Unbuilt::Failed),
            !titled,
            "{connection:?}"
        );
    }
}
