//! A local task's request: its own plain turns and answers, nothing a build adds.

use kumochi_protocol::message::{ChoiceRead, ConnectionKind};

use super::*;
use crate::context::local::build_local;

fn local() -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new("local-model").expect("slug"),
        effort: Effort::Low,
        connection: ConnectionKind::LocalChat,
    }
}

fn on_local(world: &mut World, thread: ThreadId) {
    assert!(matches!(
        world
            .store
            .set_model(thread, Some(&local()))
            .expect("saved"),
        ChoiceRead::Found { .. }
    ));
}

fn local_snapshot(task: ThreadId) -> RunSnapshot {
    RunSnapshot {
        choice: Some(local()),
        ..snapshot(Origin::Task(task), Some("Oscar"))
    }
}

fn built(
    world: &World,
    run: &RunSnapshot,
    turn: (ThreadId, RunId),
    bound: usize,
) -> Result<Built, BuildError> {
    let reader = Connection::open(world.scratch.db()).expect("reader");
    build_local(
        Snapshot::begin(&reader).expect("snapshot"),
        run,
        turn.0,
        turn.1,
        bound,
    )
}

#[test]
fn a_local_task_sends_its_own_turns_and_answers_alone() {
    let mut world = world("ctx-local");
    let (task, _) = world.run(None, "first ask", Some("first answer"));
    on_local(&mut world, task);
    let (_, current) = world.run(Some(task), "second ask", None);
    let run = local_snapshot(task);
    let built = built(&world, &run, (task, current), BOUND).expect("built");
    assert_eq!(
        built.request.input,
        vec![
            message(Role::User, "first ask"),
            message(Role::Assistant, "first answer"),
            message(Role::User, "second ask"),
        ]
    );
    assert_eq!(built.request.tools, Vec::new());
    assert_eq!(built.request.choice, Some(local()));
    assert_eq!(built.sources, vec![task]);
    let cloud = world
        .build(&run, (task, current), Supplied::default(), BOUND)
        .expect("the cloud build");
    assert_eq!(built.request.instructions, cloud.request.instructions);
}

/// Text with stored metadata, however malformed or empty, may carry more than the task's own
/// words.
#[test]
fn a_local_task_holding_more_than_plain_text_builds_nothing() {
    for (row, body) in [
        ("narration", r#"{"draw_call":"call_1"}"#),
        ("malformed", "not json"),
        ("empty", ""),
    ] {
        let mut world = world("ctx-local-content");
        let (task, _) = world.run(None, "ask", None);
        on_local(&mut world, task);
        let (_, high) = world.store.high_water().expect("ids");
        let drawn = RunId(high.0 + 1);
        world
            .store
            .begin_turn(task, None, drawn, "draw")
            .expect("turn");
        world
            .store
            .end_run(
                task,
                drawn,
                &[NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some("drawn"),
                    body: Some(body),
                }],
                RunEnd::Done,
            )
            .expect("ended");
        let (_, next) = world.run(Some(task), "next", None);
        assert!(
            matches!(
                built(&world, &local_snapshot(task), (task, next), BOUND),
                Err(BuildError::LocalContent)
            ),
            "{row}"
        );
    }
}

#[test]
fn only_a_live_task_on_its_own_local_route_builds() {
    let mut world = world("ctx-local-unbound");
    let main = world.main;
    let (inherits, inherits_run) = world.run(None, "ask", None);
    let (cloud, cloud_run) = world.run(None, "ask", None);
    assert!(matches!(
        world
            .store
            .set_model(
                cloud,
                Some(&ModelChoice {
                    connection: ConnectionKind::ChatGpt,
                    ..local()
                })
            )
            .expect("saved"),
        ChoiceRead::Found { .. }
    ));
    for (thread, run) in [(inherits, inherits_run), (cloud, cloud_run)] {
        assert!(matches!(
            built(&world, &local_snapshot(thread), (thread, run), BOUND),
            Err(BuildError::Unbound)
        ));
    }
    on_local(&mut world, main);
    let (_, kumo) = world.run(Some(main), "ask", None);
    let mut run = local_snapshot(main);
    run.origin = Origin::Kumo;
    assert!(matches!(
        built(&world, &run, (main, kumo), BOUND),
        Err(BuildError::Origin)
    ));
}

#[test]
fn older_runs_go_first_and_an_oversized_turn_has_no_room() {
    let mut world = world("ctx-local-bound");
    let (task, _) = world.run(None, "older", Some("older answer"));
    on_local(&mut world, task);
    let (_, current) = world.run(Some(task), "current", None);
    let run = local_snapshot(task);
    let whole = crate::context::bound::request_bytes(
        &built(&world, &run, (task, current), BOUND)
            .expect("built")
            .request,
    );
    let current_only = vec![message(Role::User, "current")];
    let short = built(&world, &run, (task, current), whole - 1).expect("built");
    assert_eq!(short.request.input, current_only);
    let alone = crate::context::bound::request_bytes(&short.request);
    assert!(matches!(
        built(&world, &run, (task, current), alone - 1),
        Err(BuildError::TooLarge { room: None })
    ));
}

/// A run too big for the room ends the history there: no older run fills in behind it.
#[test]
fn no_older_run_is_kept_past_one_that_did_not_fit() {
    let mut world = world("ctx-local-gap");
    let (task, _) = world.run(None, "oldest", Some("oldest answer"));
    on_local(&mut world, task);
    world.run(Some(task), &"x".repeat(4096), None);
    let (_, current) = world.run(Some(task), "current", None);
    let run = local_snapshot(task);
    let whole = crate::context::bound::request_bytes(
        &built(&world, &run, (task, current), BOUND)
            .expect("built")
            .request,
    );
    // One byte short of the newer two runs, with room left that the oldest alone would fit.
    let oldest = [
        message(Role::User, "oldest"),
        message(Role::Assistant, "oldest answer"),
    ]
    .iter()
    .map(crate::context::bound::item_bytes)
    .sum::<usize>();
    let short = built(&world, &run, (task, current), whole - oldest - 1).expect("built");
    assert_eq!(short.request.input, vec![message(Role::User, "current")]);
}
