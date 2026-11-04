use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, Incarnation, MAX_PENDING_TURNS, Phase, RunId,
    SceneDescriptor, SceneId, Step, ThreadId,
};

use kumochi_brain::coordinator::{
    ContextBuilt, Coordinator, Effect as BrainEffect, Input, StoreOp,
};
use kumochi_brain::engines::TurnEvent;
use kumochi_brain::store::Store;
use kumochi_protocol::message::ControlToShell;

use super::{Applied, Refused, Runs, TEXT_KEPT, Target};
use crate::drawing::{Chunk, Effect};
use crate::escape::{Activity, Escape, Panel, decide};

/// Every data frame in these tests is this size on the pipe.
const BYTES: u32 = 10;

#[test]
fn switching_a_cached_main_row_before_settings_keeps_kumo_symbolic() {
    use kumochi_protocol::message::{ThreadKind, ThreadRow};
    let row = ThreadRow {
        id: ThreadId(1),
        title: String::new(),
        kind: ThreadKind::Main,
        updated_ms: 0,
        choice: None,
    };
    let mut runs = Runs::default();
    runs.select_thread(Some(ThreadId(9)));
    assert!(runs.select_row(&row));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.revision(), 2);
    runs.connected(2);
    assert_eq!(
        runs.submit("home", |_| panic!("not ready")),
        Err(Refused::Offline)
    );
    runs.settings(ThreadId(99));
    assert_eq!(
        typed(&mut runs, "home"),
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(99)),
            text: "home".to_owned()
        }
    );
}

#[test]
fn kumo_is_the_default_and_each_connection_needs_its_own_settings() {
    let mut runs = Runs::default();
    assert_eq!(runs.target(), Target::Kumo);
    runs.connected(1);
    assert_eq!(
        runs.submit("first", |_| panic!("not ready")),
        Err(Refused::Offline)
    );
    runs.settings(ThreadId(7));
    assert_eq!(
        typed(&mut runs, "first"),
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(7)),
            text: "first".to_owned()
        }
    );
    runs.disconnected();
    runs.connected(2);
    assert_eq!(
        runs.submit("next", |_| panic!("not ready")),
        Err(Refused::Offline)
    );
    runs.settings(ThreadId(99));
    assert_eq!(
        typed(&mut runs, "next"),
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(99)),
            text: "next".to_owned()
        }
    );
}

#[test]
fn a_non_durable_task_and_a_deleted_focus_fall_back_to_kumo() {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.select_thread(None);
    submit(&mut runs, "task");
    runs.apply_state(RunId(1), ThreadId(9), Phase::Thinking);
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    let revision = runs.revision();
    runs.disconnected();
    runs.met(Incarnation(2));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.revision(), revision + 1);
    runs.connected(2);
    runs.settings(ThreadId(2));
    runs.select_thread(Some(ThreadId(9)));
    let revision = runs.revision();
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.revision(), revision + 1);
    assert_eq!(
        typed(&mut runs, "home"),
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(2)),
            text: "home".to_owned()
        }
    );
}

#[test]
fn a_late_answer_belongs_to_its_turn_not_the_manually_switched_focus() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "for kumo");
    runs.select_thread(Some(ThreadId(9)));
    let revision = runs.revision();
    runs.apply_state(RunId(1), ThreadId(1), Phase::Answering);
    data(&mut runs, text(1, "Kumo's answer"));
    assert_eq!(
        runs.shown(),
        Some(&super::Shown {
            run: RunId(1),
            thread: Some(ThreadId(1)),
            text: "Kumo's answer".to_owned()
        })
    );
    assert_eq!(runs.target(), Target::Task(ThreadId(9)));
    assert_eq!(runs.revision(), revision);
}

#[test]
fn three_queued_turns_a_switch_and_esc_never_move_focus_on_late_states() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "one");
    runs.select_thread(Some(ThreadId(9)));
    submit(&mut runs, "two");
    submit(&mut runs, "three");
    assert_eq!(
        runs.submitted.iter().copied().collect::<Vec<_>>(),
        [0, 1, 1]
    );
    assert!(runs.select_thread(None));
    assert_eq!(runs.target(), Target::New);
    assert_eq!(runs.revision(), 2);
    assert_eq!(runs.stop(), None);
    for run in 1..=3 {
        assert_eq!(
            runs.apply_state(RunId(run), ThreadId(run), Phase::Thinking),
            Applied {
                stop: Some(stop(run)),
                credit: 0,
            }
        );
        assert_eq!(runs.target(), Target::New);
        assert_eq!(runs.revision(), 2);
    }
    assert!(runs.submitted.is_empty());
    assert!(!runs.working());
}

#[test]
fn an_unsolicited_state_never_adopts_and_a_switch_to_main_normalizes_to_kumo() {
    let mut runs = Runs::default();
    runs.settings(ThreadId(1));
    runs.select_thread(None);
    let revision = runs.revision();
    runs.apply_state(RunId(1), ThreadId(9), Phase::Thinking);
    assert_eq!(runs.target(), Target::New);
    assert_eq!(runs.revision(), revision);
    runs.select_thread(Some(ThreadId(1)));
    assert_eq!(runs.target(), Target::Kumo);
    runs.settings(ThreadId(99));
    assert_eq!(runs.target(), Target::Kumo);
    assert_eq!(runs.revision(), revision + 1);
}

#[test]
fn selecting_and_starting_a_new_thread_only_change_the_next_turn() {
    let mut runs = answering(5);
    runs.select_thread(Some(ThreadId(9)));
    runs.apply_state(RunId(5), ThreadId(1), Phase::Answering);
    let mut sent = Vec::new();
    runs.submit("continue", |turn| {
        sent.push(turn);
        true
    })
    .expect("sent");
    runs.select_thread(None);
    runs.submit("new", |turn| {
        sent.push(turn);
        true
    })
    .expect("sent");
    assert_eq!(
        sent,
        [
            DataToBrain::TextSubmit {
                turn: 1,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(9)),
                text: "continue".to_owned()
            },
            DataToBrain::TextSubmit {
                turn: 2,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: None,
                text: "new".to_owned()
            },
        ]
    );
    assert!(runs.working(), "selection didn't stop work");
}

#[test]
fn pages_are_never_held_for_a_run_state() {
    let mut runs = Runs::default();
    let page = DataToShell::Page {
        request: 1,
        thread: ThreadId(999),
        events: Vec::new(),
        before: None,
    };
    assert_eq!(runs.apply_data(page, BYTES), BYTES);
    assert!(runs.held.is_empty());
    assert!(runs.shown().is_none());
}

#[test]
fn a_new_brain_reusing_a_run_id_is_shown_afresh_and_the_same_brain_keeps_its_fence() {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "first");
    state(&mut runs, 5, Phase::Thinking);
    data(&mut runs, step(5, 0));
    data(&mut runs, text(5, "old"));
    state(&mut runs, 5, Phase::Done);
    data(&mut runs, end(5));
    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(2);
    runs.settings(ThreadId(1));
    submit(&mut runs, "after a crash before the turn was stored");
    state(&mut runs, 5, Phase::Thinking);
    data(&mut runs, text(5, "new"));
    assert_eq!(runs.shown().map(|shown| shown.text.as_str()), Some("new"));
    assert!(
        runs.drawing.revealed(RunId(5)).is_empty(),
        "the last brain's scene isn't this run's to stop"
    );
    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(3);
    runs.settings(ThreadId(1));
    submit(&mut runs, "same brain");
    state(&mut runs, 5, Phase::Thinking);
    data(&mut runs, text(5, " stale"));
    assert_eq!(
        runs.shown().map(|shown| shown.text.as_str()),
        Some("new"),
        "the same brain's old run stays fenced"
    );
}

/// Routes a brain's effects the way the runtime would, storing a `BeginTurn` only when
/// there's a `store`.
fn deliver(runs: &mut Runs, mut store: Option<&mut Store>, effects: Vec<BrainEffect>) {
    for effect in effects {
        match effect {
            BrainEffect::Store(StoreOp::BeginTurn {
                thread,
                title,
                run,
                text,
            }) => {
                if let Some(store) = store.as_deref_mut() {
                    store
                        .begin_turn(thread, title.as_deref(), run, &text)
                        .expect("the turn is stored");
                }
            }
            BrainEffect::Control(ControlToShell::RunState {
                run, thread, phase, ..
            }) => {
                runs.apply_state(run, thread, phase);
            }
            BrainEffect::Data(message) => {
                runs.apply_data(message, BYTES);
            }
            _ => {}
        }
    }
}

/// The turn `runs` sends for `typed`, as the brain takes it.
fn typed(runs: &mut Runs, typed: &str) -> Input {
    let mut sent = None;
    runs.submit(typed, |turn| {
        sent = Some(turn);
        true
    })
    .expect("sent");
    let Some(DataToBrain::TextSubmit {
        turn,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread,
        text,
    }) = sent
    else {
        panic!("a turn was sent")
    };
    Input::Turn { turn, thread, text }
}

/// A brain started on `store`, as after a restart.
fn restarted(store: &Store) -> Coordinator {
    let (thread, run) = store.high_water().expect("ids");
    let main = store.main_thread().expect("recovered");
    Coordinator::new(main, thread, run, store.scenes().expect("scenes"))
}

#[test]
fn a_new_thread_lost_in_a_brain_crash_is_not_continued_and_an_answered_one_is() {
    let dir = std::env::temp_dir().join(format!(
        "{}-shell-runs-{}",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch folder");
    let mut store = Store::open(&dir.join("store.db")).expect("store");
    store.recover().expect("recovered");
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    let mut brain = restarted(&store);
    runs.select_thread(None);
    let turn = typed(&mut runs, "a new thread");
    let effects = brain.handle(turn).expect("turn");
    deliver(&mut runs, None, effects);

    runs.disconnected();
    runs.met(Incarnation(2));
    runs.connected(2);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    let mut brain = restarted(&store);
    let turn = typed(&mut runs, "again");
    assert_eq!(
        turn,
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(1)),
            text: "again".to_owned()
        }
    );
    let effects = brain.handle(turn).expect("turn");
    deliver(&mut runs, Some(&mut store), effects);
    let run = RunId(1);
    let effects = brain.handle(Input::TurnStored { run }).expect("stored");
    let build = effects
        .iter()
        .find_map(|effect| match effect {
            BrainEffect::Store(StoreOp::BuildContext(build)) => Some(build),
            _ => None,
        })
        .expect("the run's context is built");
    let admitted = Input::ContextBuilt(ContextBuilt::own_thread(build));
    deliver(&mut runs, Some(&mut store), effects);
    let effects = brain.handle(admitted).expect("admitted");
    deliver(&mut runs, Some(&mut store), effects);
    let answer = Input::Engine {
        run,
        event: TurnEvent::TextDelta("answered".to_owned()),
    };
    let effects = brain.handle(answer).expect("text");
    deliver(&mut runs, Some(&mut store), effects);
    assert_eq!(
        runs.shown().map(|shown| shown.text.as_str()),
        Some("answered")
    );

    runs.disconnected();
    runs.met(Incarnation(3));
    runs.connected(3);
    runs.settings(ThreadId(1));
    let mut brain = restarted(&store);
    let turn = typed(&mut runs, "continue");
    assert!(
        matches!(
            turn,
            Input::Turn {
                thread: Some(ThreadId(1)),
                ..
            }
        ),
        "the answered Kumo conversation is still continued"
    );
    let effects = brain.handle(turn).expect("turn");
    deliver(&mut runs, Some(&mut store), effects);
    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unanswered_old_turn_cannot_override_a_new_thread_choice() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "old pending turn");
    runs.select_thread(None);
    runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
    let mut sent = Vec::new();
    runs.submit("new thread turn", |turn| {
        sent.push(turn);
        true
    })
    .expect("sent");
    runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
    runs.submit("continue new thread", |turn| {
        sent.push(turn);
        true
    })
    .expect("sent");
    assert_eq!(
        sent,
        [
            DataToBrain::TextSubmit {
                turn: 2,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: None,
                text: "new thread turn".to_owned()
            },
            DataToBrain::TextSubmit {
                turn: 3,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(22)),
                text: "continue new thread".to_owned()
            },
        ]
    );
}

fn text(run: u64, delta: &str) -> DataToShell {
    DataToShell::AssistantText {
        run: RunId(run),
        delta: delta.to_owned(),
    }
}

fn step(run: u64, index: u32) -> DataToShell {
    DataToShell::StepCommit {
        run: RunId(run),
        scene: SceneId(run),
        index,
        step: Step {
            label: format!("step {index}"),
        },
    }
}

fn end(run: u64) -> DataToShell {
    DataToShell::RunEnd { run: RunId(run) }
}

fn stop(run: u64) -> ControlToBrain {
    ControlToBrain::Stop {
        run: RunId(run),
        revealed: Vec::new(),
    }
}

fn state(runs: &mut Runs, run: u64, phase: Phase) -> Applied {
    runs.apply_state(RunId(run), ThreadId(1), phase)
}

fn data(runs: &mut Runs, message: DataToShell) -> u32 {
    runs.apply_data(message, BYTES)
}

fn submit(runs: &mut Runs, typed: &str) {
    runs.submit(typed, |_| true).expect("sent");
}

/// Connected, with no scene to recover and `run` answering in thread 1.
fn answering(run: u64) -> Runs {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    state(&mut runs, run, Phase::Answering);
    runs
}

fn indices(runs: &Runs) -> Vec<u32> {
    runs.scenes()
        .flat_map(|scene| scene.steps.iter().map(|(index, _)| *index))
        .collect()
}

fn shown_text(runs: &Runs) -> Option<(RunId, &str)> {
    runs.shown().map(|shown| (shown.run, shown.text.as_str()))
}

#[test]
fn stop_fences_the_run_and_reveals_what_is_on_screen() {
    let mut runs = answering(5);
    data(&mut runs, step(5, 0));
    data(&mut runs, step(5, 1));
    assert!(runs.working());
    assert_eq!(
        runs.stop(),
        Some(ControlToBrain::Stop {
            run: RunId(5),
            revealed: vec![0, 1],
        })
    );
    assert!(!runs.working());
    assert_eq!(runs.stop(), None, "nothing else moves");
    assert!(runs.drawing(), "the steps stay");
}

#[test]
fn a_fenced_run_s_messages_are_dropped_when_applied() {
    let mut runs = answering(5);
    data(&mut runs, text(5, "kept "));
    runs.stop();
    // Already queued in the shell when Esc came.
    assert_eq!(
        data(&mut runs, text(5, "late")),
        BYTES,
        "dropped, still credited"
    );
    data(&mut runs, step(5, 0));
    state(&mut runs, 5, Phase::Answering);
    assert!(!runs.working());
    assert_eq!(runs.shown().expect("shown").text, "kept ");
    assert!(indices(&runs).is_empty());
}

#[test]
fn clear_drops_the_drawing_and_keeps_late_steps_out() {
    let mut runs = answering(5);
    data(&mut runs, text(5, "words"));
    data(&mut runs, step(5, 0));
    state(&mut runs, 5, Phase::Done);
    data(&mut runs, end(5));
    assert!(!runs.working());
    assert_eq!(
        runs.clear(),
        Effect {
            clear: Some(ControlToBrain::ClearDrawing {
                scene_ids: vec![SceneId(5)]
            }),
            credit: 0
        }
    );
    assert!(!runs.drawing());
    assert_eq!(runs.clear(), Effect::default(), "nothing left to clear");
    data(&mut runs, step(5, 1));
    assert_eq!(indices(&runs), Vec::<u32>::new());
    assert_eq!(
        shown_text(&runs),
        Some((RunId(5), "words")),
        "the text stays"
    );
    state(&mut runs, 6, Phase::Thinking);
    data(&mut runs, step(6, 0));
    assert_eq!(indices(&runs), vec![0], "a newer run draws again");
}

#[test]
fn a_newer_run_takes_over_the_view_and_older_data_is_dropped() {
    let mut runs = answering(5);
    data(&mut runs, text(5, "old"));
    state(&mut runs, 6, Phase::Thinking);
    data(&mut runs, text(6, "new"));
    data(&mut runs, text(5, " late"));
    assert_eq!(shown_text(&runs), Some((RunId(6), "new")));
}

#[test]
fn data_before_its_run_state_is_held_until_the_state_names_it() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "hi");
    assert_eq!(
        data(&mut runs, text(3, "early")),
        0,
        "held, credit withheld"
    );
    assert!(runs.shown().is_none());
    let applied = state(&mut runs, 3, Phase::Answering);
    assert_eq!(
        applied,
        Applied {
            stop: None,
            credit: BYTES
        }
    );
    assert_eq!(shown_text(&runs), Some((RunId(3), "early")));
    assert!(runs.working());
}

#[test]
fn esc_cancels_a_turn_whose_run_isnt_named_yet() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "a");
    assert!(runs.working(), "a sent turn is work");
    assert_eq!(runs.stop(), None, "nothing to name yet");
    assert!(!runs.working());
    submit(&mut runs, "b");
    assert!(runs.working(), "a turn after Esc isn't cancelled");
    assert_eq!(data(&mut runs, text(1, "a's early text")), 0);
    let applied = state(&mut runs, 1, Phase::Thinking);
    assert_eq!(
        applied,
        Applied {
            stop: Some(stop(1)),
            credit: BYTES
        }
    );
    assert!(runs.shown().is_none(), "nothing of a shows");
    assert_eq!(runs.submitted.len(), 1, "a used exactly one slot");
    data(&mut runs, text(1, "more"));
    data(&mut runs, step(1, 0));
    assert_eq!(state(&mut runs, 1, Phase::Interrupted), Applied::default());
    assert!(runs.shown().is_none());
    assert_eq!(state(&mut runs, 2, Phase::Thinking), Applied::default());
    data(&mut runs, text(2, "b runs"));
    assert_eq!(shown_text(&runs), Some((RunId(2), "b runs")));
    assert!(runs.working());
    assert_eq!(runs.submitted.len(), 0);
}

#[test]
fn esc_stops_the_known_run_and_cancels_the_turn_queued_behind_it() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "first");
    state(&mut runs, 4, Phase::Answering);
    data(&mut runs, text(4, "shown"));
    submit(&mut runs, "a");
    assert_eq!(runs.stop(), Some(stop(4)));
    submit(&mut runs, "b");
    assert_eq!(data(&mut runs, text(5, "a's text")), 0);
    assert_eq!(
        state(&mut runs, 5, Phase::Thinking),
        Applied {
            stop: Some(stop(5)),
            credit: BYTES
        },
        "exactly one Stop names a's run"
    );
    assert_eq!(state(&mut runs, 5, Phase::Answering), Applied::default());
    assert_eq!(shown_text(&runs), Some((RunId(4), "shown")));
    assert_eq!(runs.submitted.len(), 1);
    state(&mut runs, 6, Phase::Thinking);
    data(&mut runs, text(6, "b"));
    assert_eq!(shown_text(&runs), Some((RunId(6), "b")));
    assert!(runs.working());
}

#[test]
fn a_run_stays_stoppable_until_its_data_has_drained() {
    let mut runs = answering(5);
    data(&mut runs, text(5, "seen"));
    state(&mut runs, 5, Phase::Done);
    assert!(runs.working(), "its data hasn't ended");
    assert_eq!(
        runs.stop(),
        Some(ControlToBrain::Stop {
            run: RunId(5),
            revealed: Vec::new(),
        })
    );
    data(&mut runs, text(5, " late"));
    data(&mut runs, step(5, 0));
    data(&mut runs, end(5));
    assert_eq!(shown_text(&runs), Some((RunId(5), "seen")));
    assert!(!runs.drawing());
    assert!(!runs.working());
}

#[test]
fn a_run_settles_whichever_ending_arrives_first() {
    let mut runs = answering(5);
    data(&mut runs, end(5));
    assert!(runs.working(), "its state hasn't ended");
    state(&mut runs, 5, Phase::Done);
    assert!(!runs.working());
    assert_eq!(runs.stop(), None);
}

#[test]
fn a_terminal_state_first_waits_for_run_end_to_settle() {
    let mut runs = answering(5);
    state(&mut runs, 5, Phase::Done);
    assert!(runs.working(), "its data hasn't ended");
    data(&mut runs, end(5));
    assert!(!runs.working());
    assert_eq!(runs.stop(), None);
}

#[test]
fn a_reconnect_forgets_turns_esc_cancelled_and_their_held_data() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "a");
    assert_eq!(runs.stop(), None, "a isn't named yet");
    assert_eq!(data(&mut runs, text(1, "a's text")), 0, "held");
    runs.disconnected();
    runs.connected(1);
    runs.settings(ThreadId(1));
    submit(&mut runs, "b");
    assert_eq!(
        state(&mut runs, 2, Phase::Answering),
        Applied {
            stop: None,
            credit: 0
        },
        "b isn't cancelled, and a's held frame is gone"
    );
    assert_eq!(data(&mut runs, text(2, "b's answer")), BYTES);
    assert_eq!(shown_text(&runs), Some((RunId(2), "b's answer")));
    assert!(runs.working());
}

/// One Esc press, decided the way the presence decides it with the typing panel open.
fn press(runs: &mut Runs) -> (Escape, Option<ControlToBrain>) {
    let activity = Activity {
        working: runs.working(),
        speaking: false,
        drawing: runs.drawing(),
        pulling: false,
    };
    let action = decide(activity, Panel::Open);
    let message = match action {
        Escape::Stop => runs.stop(),
        Escape::Clear => runs.clear().clear,
        Escape::CancelPull | Escape::Dismiss | Escape::CancelEdit | Escape::Ignore => None,
    };
    (action, message)
}

#[test]
fn esc_stops_then_clears_then_dismisses_with_a_cancelled_turn_pending() {
    let mut runs = answering(4);
    data(&mut runs, step(4, 0));
    state(&mut runs, 4, Phase::Done);
    data(&mut runs, end(4));
    submit(&mut runs, "a");
    assert_eq!(press(&mut runs), (Escape::Stop, None), "a is cancelled");
    assert_eq!(
        press(&mut runs),
        (
            Escape::Clear,
            Some(ControlToBrain::ClearDrawing {
                scene_ids: vec![SceneId(4)]
            })
        ),
        "a cancelled turn isn't work, so the drawing clears"
    );
    assert_eq!(press(&mut runs), (Escape::Dismiss, None));
    assert_eq!(
        state(&mut runs, 5, Phase::Thinking),
        Applied {
            stop: Some(stop(5)),
            credit: 0
        },
        "a's run is stopped once it's named"
    );
    assert_eq!(press(&mut runs), (Escape::Dismiss, None));
}

#[test]
fn an_older_run_ending_never_settles_a_newer_one() {
    let mut runs = answering(5);
    state(&mut runs, 6, Phase::Thinking);
    state(&mut runs, 5, Phase::Interrupted);
    data(&mut runs, end(5));
    assert!(runs.working());
    assert_eq!(runs.stop(), Some(stop(6)));
}

#[test]
fn a_second_new_task_turn_waits_then_joins_the_first_task_without_reselection() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.select_thread(None);
    let revision = runs.revision();
    let mut sent = Vec::new();
    runs.submit("one", |turn| {
        sent.push(turn);
        true
    })
    .expect("first new turn sent");
    assert_eq!(
        runs.submit("two", |turn| {
            sent.push(turn);
            true
        }),
        Err(Refused::Busy)
    );
    assert_eq!(
        sent,
        [DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: None,
            text: "one".to_owned(),
        }]
    );
    assert_eq!(runs.submitted.len(), 1);
    assert_eq!(runs.target(), Target::New);
    assert_eq!(runs.revision(), revision);

    runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
    assert_eq!(runs.target(), Target::Task(ThreadId(11)));
    assert!(runs.submitted.is_empty());
    assert_eq!(runs.revision(), revision);
    runs.submit("two", |turn| {
        sent.push(turn);
        true
    })
    .expect("retry continues the adopted task");
    assert_eq!(
        sent,
        [
            DataToBrain::TextSubmit {
                turn: 1,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: None,
                text: "one".to_owned(),
            },
            DataToBrain::TextSubmit {
                turn: 2,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(11)),
                text: "two".to_owned(),
            },
        ]
    );
}

#[test]
fn only_a_newer_run_sets_the_next_turn_s_thread() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    let mut sent = Vec::new();
    for typed in ["one", "two"] {
        runs.select_thread(None);
        runs.submit(typed, |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
    }
    runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
    runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
    runs.apply_state(RunId(1), ThreadId(11), Phase::Done);
    runs.submit("third", |turn| {
        sent.push(turn);
        true
    })
    .expect("sent");
    assert_eq!(
        sent,
        [
            DataToBrain::TextSubmit {
                turn: 1,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: None,
                text: "one".to_owned(),
            },
            DataToBrain::TextSubmit {
                turn: 2,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: None,
                text: "two".to_owned(),
            },
            DataToBrain::TextSubmit {
                turn: 3,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(22)),
                text: "third".to_owned(),
            },
        ]
    );
}

#[test]
fn a_fenced_older_run_s_late_failure_keeps_the_thread() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.select_thread(None);
    submit(&mut runs, "one");
    runs.select_thread(None);
    submit(&mut runs, "two");
    runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
    runs.stop();
    runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
    runs.apply_state(RunId(1), ThreadId(11), Phase::Failed);
    let mut sent = None;
    runs.submit("third", |turn| {
        sent = Some(turn);
        true
    })
    .expect("sent");
    assert_eq!(
        sent,
        Some(DataToBrain::TextSubmit {
            turn: 3,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(ThreadId(22)),
            text: "third".to_owned(),
        })
    );
}

#[test]
fn submits_stay_under_the_brain_s_turn_cap() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    let sends = std::cell::Cell::new(0);
    let send = |_| {
        sends.set(sends.get() + 1);
        true
    };
    for _ in 0..MAX_PENDING_TURNS - 1 {
        runs.submit("again", send).expect("under the cap");
    }
    assert_eq!(runs.submit("again", send), Err(Refused::Busy));
    assert_eq!(sends.get(), 63, "a refused turn never reaches the sender");
    runs.apply_state(RunId(1), ThreadId(1), Phase::Thinking);
    assert_eq!(runs.submit("again", send), Ok(()));
    assert_eq!(sends.get(), 64);
}

#[test]
fn submit_numbers_advance_only_on_enqueue_and_reset_per_connection_without_wrapping() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    assert_eq!(runs.submit("unsent", |_| false), Err(Refused::Offline));
    let mut sent = Vec::new();
    for text in ["first", "second"] {
        assert_eq!(
            runs.submit(text, |message| {
                sent.push(message);
                true
            }),
            Ok(())
        );
    }
    assert_eq!(
        sent,
        [
            DataToBrain::TextSubmit {
                turn: 1,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(1)),
                text: "first".to_owned()
            },
            DataToBrain::TextSubmit {
                turn: 2,
                screen: kumochi_protocol::message::ScreenChoice::None,
                thread: Some(ThreadId(1)),
                text: "second".to_owned()
            },
        ]
    );
    runs.turns_sent = u32::MAX - 1;
    assert_eq!(
        runs.submit("last", |message| {
            assert_eq!(
                message,
                DataToBrain::TextSubmit {
                    turn: u32::MAX,
                    screen: kumochi_protocol::message::ScreenChoice::None,
                    thread: Some(ThreadId(1)),
                    text: "last".to_owned()
                }
            );
            true
        }),
        Ok(())
    );
    assert_eq!(
        runs.submit("overflow", |_| panic!("overflow cannot reach sender")),
        Err(Refused::Busy)
    );
    runs.disconnected();
    runs.connected(2);
    runs.settings(ThreadId(1));
    assert_eq!(
        typed(&mut runs, "reset"),
        Input::Turn {
            turn: 1,
            thread: Some(ThreadId(1)),
            text: "reset".to_owned()
        }
    );
}

#[test]
fn blank_long_offline_and_unsent_turns_are_refused_and_not_counted() {
    let mut runs = Runs::default();
    assert_eq!(runs.submit("hi", |_| true), Err(Refused::Offline));
    runs.connected(1);
    runs.settings(ThreadId(1));
    assert_eq!(runs.submit(" \n\t ", |_| true), Err(Refused::Blank));
    let long = "x".repeat(Pipe::Data.max_payload());
    assert_eq!(runs.submit(&long, |_| true), Err(Refused::TooLong));
    assert_eq!(runs.submit("hi", |_| false), Err(Refused::Offline));
    assert_eq!(runs.submitted.len(), 0);
    assert!(!runs.working());
}

#[test]
fn a_lost_brain_fences_what_it_knew_and_forgets_unanswered_turns() {
    let mut runs = answering(5);
    submit(&mut runs, "pending");
    data(&mut runs, text(6, "held"));
    runs.disconnected();
    assert!(!runs.working());
    assert_eq!(runs.submitted.len(), 0);
    assert_eq!(runs.submit("hi", |_| true), Err(Refused::Offline));
    data(&mut runs, text(5, "from before"));
    assert!(runs.shown().is_none());
    runs.connected(1);
    runs.settings(ThreadId(1));
    assert_eq!(
        state(&mut runs, 6, Phase::Answering).credit,
        0,
        "held data went with it"
    );
    assert!(runs.shown().is_none());
    assert!(runs.working());
}

#[test]
fn esc_stops_a_run_moving_during_recovery_before_it_clears() {
    let mut runs = Runs::default();
    runs.connected(1);
    runs.settings(ThreadId(1));
    let scene = SceneDescriptor {
        scene: SceneId(3),
        revision: 2,
        committed: 2,
    };
    runs.apply_scenes(&[scene]).expect("valid");
    let chunk = Chunk {
        scene: SceneId(3),
        revision: 2,
        steps: vec![Step {
            label: "restored".to_owned(),
        }],
        last: false,
        bytes: BYTES,
    };
    assert_eq!(runs.apply_snapshot(1, chunk), Ok(BYTES));
    submit(&mut runs, "new turn");
    state(&mut runs, 5, Phase::Answering);
    assert!(runs.drawing(), "the recovery is still receiving");
    assert_eq!(press(&mut runs), (Escape::Stop, Some(stop(5))));
    assert_eq!(
        press(&mut runs),
        (
            Escape::Clear,
            Some(ControlToBrain::ClearDrawing {
                scene_ids: vec![SceneId(3)]
            })
        )
    );
    assert!(!runs.drawing());
}

#[test]
fn the_view_keeps_the_end_of_a_long_answer() {
    let mut runs = answering(1);
    data(&mut runs, text(1, &"a".repeat(TEXT_KEPT)));
    data(&mut runs, text(1, "é!"));
    let expected = format!("{}é!", "a".repeat(TEXT_KEPT - 3));
    assert_eq!(shown_text(&runs), Some((RunId(1), expected.as_str())));
}

#[test]
fn the_view_cuts_long_answers_at_a_character_boundary() {
    let mut runs = answering(1);
    data(&mut runs, text(1, &"é".repeat(TEXT_KEPT / 2)));
    data(&mut runs, text(1, "!"));
    let expected = format!("{}!", "é".repeat(TEXT_KEPT / 2 - 1));
    assert_eq!(shown_text(&runs), Some((RunId(1), expected.as_str())));
}
