//! Admission: a round starts only with a build no accepted or pending delete has touched.

use kumochi_protocol::message::{AssistantName, ControlToShell, Deleted, ThreadId};

use super::{R, T, build_context, built, ends_failed, fresh, request};
use crate::context::{ConnectionTag, Origin, RunSnapshot};
use crate::coordinator::{
    BuildContext, BuildKey, ContextBuilt, Coordinator, Effect, Input, SendKey, Stale, StoreOp,
    Unbuilt,
};
use crate::engines::RunRequest;
use crate::store::Failure;

const MAIN: ThreadId = ThreadId(1);
const B: ThreadId = ThreadId(2);
const C: ThreadId = ThreadId(3);

#[test]
fn lifecycle_replacement_preserves_the_end_and_stop_cancels_before_store_ack() {
    use crate::recall::Error;
    let mut core = kumo_building();
    let first = core
        .current
        .as_ref()
        .unwrap()
        .build
        .as_ref()
        .unwrap()
        .control
        .0
        .clone();
    let mut effects = Vec::new();
    core.build(R, 2, &mut effects);
    let second = core
        .current
        .as_ref()
        .unwrap()
        .build
        .as_ref()
        .unwrap()
        .control
        .0
        .clone();
    assert_eq!(first.check(), Err(Error::Ended));
    assert_eq!(second.end, first.end);
    assert_eq!(second.check(), Ok(()));
    core.handle(Input::Stop {
        run: R,
        revealed: vec![],
    })
    .unwrap();
    assert_eq!(core.ending_runs(), 1);
    assert_eq!(second.check(), Err(Error::Ended));
}

#[test]
fn lifecycle_voided_build_cancels_while_it_waits_for_a_delete() {
    use crate::recall::Error;
    let mut core = kumo_building();
    let read = core
        .current
        .as_ref()
        .unwrap()
        .build
        .as_ref()
        .unwrap()
        .control
        .0
        .clone();
    core.handle(delete(B)).unwrap();
    core.handle(kumo_built(1, &[MAIN, B])).unwrap();
    assert_eq!(read.check(), Err(Error::Ended));
    assert!(
        !core
            .current
            .as_ref()
            .unwrap()
            .build
            .as_ref()
            .unwrap()
            .admitted()
    );
    core.handle(hidden(B)).unwrap();
    let next = &core
        .current
        .as_ref()
        .unwrap()
        .build
        .as_ref()
        .unwrap()
        .control
        .0;
    assert_eq!(next.end, read.end);
    assert_eq!(next.check(), Ok(()));
}

fn key(attempt: u64) -> BuildKey {
    BuildKey {
        run: R,
        round: 1,
        attempt,
    }
}

fn kumo_build(attempt: u64, snapshot: Option<RunSnapshot>) -> Effect {
    Effect::Store(StoreOp::BuildContext(BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: key(attempt),
        thread: MAIN,
        origin: Origin::Kumo,
        first: snapshot.is_none(),
        arrived: crate::coordinator::Arrived::of(
            kumochi_protocol::message::ConnectionKind::ChatGpt,
            crate::context::ConnectionTag(0),
        ),
        snapshot,
        drawings: crate::drawing::Digest::default(),
        picture_models: Vec::new(),
        key_results: Vec::new(),
    }))
}

fn kumo_built(attempt: u64, sources: &[ThreadId]) -> Input {
    built(R, attempt, sources)
}

fn kumo_start(attempt: u64) -> Effect {
    Effect::StartEngine {
        request: RunRequest {
            key: SendKey::Build(key(attempt)),
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
        text: "what happened?".into(),
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
        text: "first".into(),
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
        (Unbuilt::TooLarge { room: None }, Failure::Budget),
        (Unbuilt::Failed, Failure::Store),
    ] {
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
        text: "hi".into(),
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

/// A build already bound to a local route gathers no drawings, picture models or key results,
/// which that route never sends, while the same build bound to `ChatGPT` gathers all three.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: a drawn scene, pictures, a key result and the build"
)]
fn a_known_local_build_gathers_nothing_a_local_route_never_sends() {
    use std::sync::Arc;

    use kumochi_protocol::message::{ConnectionKind, ModelSlug, RunId};

    use crate::coordinator::{Availability, PictureRoutes, Ready};
    use crate::dispatch::ToolOutput;
    use crate::drawing::{Reply, Target, Write};
    use crate::engines::CallId;
    use crate::store::{RunEnd, Store};
    use crate::test_support::Scratch;

    for (kind, gathers) in [
        (ConnectionKind::ChatGpt, true),
        (ConnectionKind::LocalChat, false),
    ] {
        let scratch = Scratch::new("admission-local-gathers");
        let mut store = Store::open(&scratch.db()).expect("store");
        store.recover().expect("recover");
        store.main_thread().expect("main");
        store.begin_turn(T, Some("test"), R, "turn").expect("run");
        let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"car","surface":"overlay","alt":"a car"},"layout_version":2})).expect("header");
        let step =
            serde_json::from_value(serde_json::json!({"id":"s1","put":[{"view3d":{"id":"v"}},
            {"solid":{"in":"v","id":"a","shape":"box","size":[2,2,2]}}]}))
            .expect("typed step");
        let written = store
            .drawing_write(&Write::Step {
                thread: T,
                run: R,
                clear: Vec::new(),
                target: Target::New(header),
                revision: 1,
                layout_version: 2,
                step: Arc::new(step),
                protected: false,
            })
            .expect("first step");
        assert!(matches!(written, Reply::Step(_)));
        store.end_run(T, R, &[], RunEnd::Done).expect("outcome");
        let mut core = Coordinator::ready_for_tests(MAIN, T, R, Vec::new());
        assert_eq!(core.restore_drawings(store.drawing_scenes()), Vec::new());
        core.set_pictures(PictureRoutes::new(true, true));
        core.handle(Input::Engines(
            ConnectionKind::ChatGpt,
            Availability::Ready(Ready {
                images: vec![ModelSlug::new("m").expect("slug")],
                ..Ready::fixture()
            }),
        ))
        .expect("never stale");
        let seeded = vec![(
            CallId::new("call-1").expect("id"),
            ToolOutput::Text("seen".to_owned()),
        )];
        let run = RunId(R.0 + 1);
        core.handle(Input::Turn {
            turn: 1,
            thread: Some(T),
            text: "more".into(),
        })
        .expect("a turn is never stale");
        super::admit(&mut core, run);
        let current = core.current.as_mut().expect("running");
        current.snapshot = Some(RunSnapshot {
            origin: Origin::Task(T),
            assistant: AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(super::routes::choice(kind, "m")),
            connection: ConnectionTag(0),
        });
        current.route = Some(crate::context::Route {
            kind,
            tag: ConnectionTag(0),
        });
        current.tools.key_results.clone_from(&seeded);
        let mut effects = Vec::new();
        core.build(run, 2, &mut effects);
        let [Effect::Store(StoreOp::BuildContext(context))] = effects.as_slice() else {
            panic!("one build: {effects:?}");
        };
        let on_screen = core.drawing.digest(T);
        assert_ne!(
            on_screen,
            crate::drawing::Digest::default(),
            "the scene is drawn"
        );
        let expected = if gathers {
            let route = crate::context::Route {
                kind: ConnectionKind::ChatGpt,
                tag: ConnectionTag(0),
            };
            (
                on_screen,
                vec![(route, vec![ModelSlug::new("m").expect("slug")])],
                seeded,
            )
        } else {
            (crate::drawing::Digest::default(), Vec::new(), Vec::new())
        };
        assert_eq!(
            (
                context.drawings.clone(),
                context.picture_models.clone(),
                context.key_results.clone()
            ),
            expected,
            "{kind:?}"
        );
    }
}
