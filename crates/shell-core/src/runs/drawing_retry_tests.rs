//! Retry after a drawing didn't come back: the notice offers the typed turn again only while
//! it's the named, unfenced run of the current thread and selection, and the retried run is an
//! ordinary typed run, so each of its endings settles the offer as any run's would.

use kumochi_protocol::message::{
    Action, ControlToBrain, DataToBrain, ErrorKey, Incarnation, Phase, RunId, RunOrigin,
    ScreenChoice, ThreadId, UserError,
};

use super::Runs;

const ASK: &str = "draw the flow ✓";

/// Connected to brain 1, with main thread 1 as Kumo and `ASK` sent as turn 1.
fn asked() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    runs.submit(ASK, |_| true).expect("sent");
    runs
}

fn state(runs: &mut Runs, run: u64, turn: u32, phase: Phase, error: Option<UserError>) {
    runs.apply_run_state(
        RunId(run),
        ThreadId(1),
        phase,
        error,
        Some(RunOrigin::Typed { turn }),
    );
}

fn not_returned(runs: &mut Runs, run: u64) -> Vec<Action> {
    runs.drawing_notice(ErrorKey::DrawingNotReturned, ThreadId(1), RunId(run))
        .actions()
        .to_vec()
}

/// Run `run` answers turn `turn` and ends `Done` with nothing drawn, in the brain's order.
fn drew_nothing(runs: &mut Runs, run: u64, turn: u32) -> Vec<Action> {
    state(runs, run, turn, Phase::Thinking, None);
    let actions = not_returned(runs, run);
    state(runs, run, turn, Phase::Done, None);
    actions
}

/// What Retry of `run` sends with an empty composer, if anything.
fn retried(runs: &mut Runs, run: u64) -> Option<DataToBrain> {
    let mut sent = None;
    let result = runs.retry(RunId(run), "", |message| {
        sent = Some(message);
        true
    });
    assert_eq!(result.is_ok(), sent.is_some());
    sent
}

fn ask(turn: u32) -> DataToBrain {
    DataToBrain::TextSubmit {
        turn,
        screen: ScreenChoice::None,
        thread: Some(ThreadId(1)),
        text: ASK.into(),
    }
}

#[test]
fn retry_after_no_drawing_resends_the_exact_turn_and_a_drawn_retry_ends_it() {
    let mut runs = asked();
    assert_eq!(drew_nothing(&mut runs, 1, 1), [Action::Retry]);
    assert!(runs.retry_ready(RunId(1)));
    assert_eq!(retried(&mut runs, 1), Some(ask(2)));
    assert_eq!(retried(&mut runs, 1), None);

    state(&mut runs, 2, 2, Phase::Thinking, None);
    state(&mut runs, 2, 2, Phase::Done, None);
    assert!(!runs.retry_ready(RunId(2)));
    assert_eq!(retried(&mut runs, 2), None);
}

#[test]
fn a_retry_that_draws_nothing_again_offers_retry_again() {
    let mut runs = asked();
    drew_nothing(&mut runs, 1, 1);
    assert_eq!(retried(&mut runs, 1), Some(ask(2)));
    assert_eq!(drew_nothing(&mut runs, 2, 2), [Action::Retry]);
    assert_eq!(retried(&mut runs, 2), Some(ask(3)));
}

#[test]
fn esc_stops_a_retried_run_like_any_other_and_its_late_notice_offers_nothing() {
    let mut runs = asked();
    drew_nothing(&mut runs, 1, 1);
    assert_eq!(retried(&mut runs, 1), Some(ask(2)));
    state(&mut runs, 2, 2, Phase::Thinking, None);
    assert_eq!(
        runs.stop(),
        Some(ControlToBrain::Stop {
            run: RunId(2),
            revealed: Vec::new(),
        })
    );
    assert_eq!(not_returned(&mut runs, 2), []);
    state(&mut runs, 2, 2, Phase::Done, None);
    assert_eq!(retried(&mut runs, 2), None);
}

#[test]
fn esc_or_a_spoken_stop_withdraws_an_offered_retry() {
    let mut runs = asked();
    drew_nothing(&mut runs, 1, 1);
    let _stop = runs.stop();
    assert_eq!(retried(&mut runs, 1), None);

    let mut runs = asked();
    state(&mut runs, 1, 1, Phase::Thinking, None);
    assert_eq!(not_returned(&mut runs, 1), [Action::Retry]);
    assert!(runs.fence_run(RunId(1)).is_some());
    state(&mut runs, 1, 1, Phase::Done, None);
    assert_eq!(retried(&mut runs, 1), None);
}

#[test]
fn a_retried_run_that_fails_gets_the_failed_runs_retry() {
    let mut runs = asked();
    drew_nothing(&mut runs, 1, 1);
    assert_eq!(retried(&mut runs, 1), Some(ask(2)));
    state(&mut runs, 2, 2, Phase::Thinking, None);
    let offline =
        UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).expect("bounded");
    state(&mut runs, 2, 2, Phase::Failed, Some(offline));
    assert_eq!(retried(&mut runs, 2), Some(ask(3)));
}

#[test]
fn only_the_current_typed_turn_with_its_text_is_offered() {
    let mut runs = asked();
    runs.apply_run_state(RunId(1), ThreadId(1), Phase::Thinking, None, None);
    assert_eq!(not_returned(&mut runs, 1), []);

    let mut runs = asked();
    state(&mut runs, 1, 1, Phase::Thinking, None);
    let other = runs.drawing_notice(ErrorKey::DrawingNotReturned, ThreadId(2), RunId(1));
    assert_eq!(other.actions(), []);
    let replaced = runs.drawing_notice(ErrorKey::DrawingReplacementFailed, ThreadId(1), RunId(1));
    assert_eq!(replaced.actions(), []);

    let mut runs = asked();
    runs.submit("newer", |_| true).expect("sent");
    assert_eq!(drew_nothing(&mut runs, 1, 1), []);
    state(&mut runs, 2, 2, Phase::Thinking, None);
    assert_eq!(not_returned(&mut runs, 2), [Action::Retry]);
    assert_eq!(
        retried(&mut runs, 2),
        Some(DataToBrain::TextSubmit {
            turn: 3,
            screen: ScreenChoice::None,
            thread: Some(ThreadId(1)),
            text: "newer".into(),
        })
    );
}
