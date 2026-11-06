//! Putting a failed turn's text back: once, for its own run, only while the thread it was typed
//! to and the selection it was typed at are current. A later attempt, Esc, a selection change,
//! a delete of its thread, a disconnect or a new brain drops the text itself, not only the restore.

use kumochi_protocol::message::{
    Content, DataToShell, ErrorKey, Incarnation, Phase, RunId, ThreadId, UserError,
};

use super::{Applied, Refused, Runs};

/// The keys a sign-in failure explains a named run's failure with, written out here rather than
/// read from the code under test. `TurnRefused` has no run, so it isn't one.
const RECONNECT: [ErrorKey; 6] = [
    ErrorKey::SessionEnded,
    ErrorKey::AccountRefused,
    ErrorKey::SignInNeeded,
    ErrorKey::ClaudeCodeSignedOut,
    ErrorKey::AnthropicKeyRejected,
    ErrorKey::RenewalUnconfirmed,
];

const MARKER: &str = "kumochi-sentinel-7f3a";

/// Connected to brain 1, with main thread 1 as Kumo and no scene to recover.
fn ready() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(ThreadId(1));
    runs.apply_scenes(&[]).expect("valid");
    runs
}

fn submit(runs: &mut Runs, text: &str) {
    runs.submit(text, |_| true).expect("sent");
}

fn thinking(runs: &mut Runs, run: u64, thread: u64) {
    runs.apply_run_state(RunId(run), ThreadId(thread), Phase::Thinking, None);
}

fn ended(runs: &mut Runs, run: u64, thread: u64, phase: Phase, key: Option<ErrorKey>) -> Applied {
    let error = key.map(UserError::from);
    runs.apply_run_state(RunId(run), ThreadId(thread), phase, error)
        .0
}

/// The text a failure of `run` in `thread`, explained by `key`, gives back.
fn failed(runs: &mut Runs, run: u64, thread: u64, key: ErrorKey) -> Option<Content> {
    ended(runs, run, thread, Phase::Failed, Some(key)).restore
}

#[test]
fn each_reconnect_key_restores_the_exact_text_once() {
    for key in RECONNECT {
        let mut runs = ready();
        submit(&mut runs, "résumé ✓");
        runs.apply_state(RunId(1), ThreadId(1), Phase::Thinking);
        assert_eq!(runs.kept(), vec![12], "{key:?}");
        assert_eq!(
            failed(&mut runs, 1, 1, key),
            Some(Content::from("résumé ✓")),
            "{key:?}"
        );
        assert_eq!(runs.kept(), Vec::<usize>::new(), "{key:?}");
        assert_eq!(failed(&mut runs, 1, 1, key), None, "{key:?} restores once");

        // A run named by its failure restores the same way.
        let mut runs = ready();
        submit(&mut runs, "first state");
        assert_eq!(
            failed(&mut runs, 1, 1, key),
            Some(Content::from("first state")),
            "{key:?}"
        );
    }
}

#[test]
fn reconnect_after_answering_never_restores_an_already_started_turn() {
    for key in RECONNECT {
        let mut runs = ready();
        submit(&mut runs, "already started");
        runs.apply_state(RunId(1), ThreadId(1), Phase::Answering);
        assert_eq!(failed(&mut runs, 1, 1, key), None, "{key:?}");
        assert_eq!(runs.kept(), Vec::<usize>::new());
        assert_eq!(failed(&mut runs, 1, 1, key), None);
    }
}

#[test]
fn a_new_target_restores_into_the_thread_its_run_names() {
    let mut runs = ready();
    assert!(runs.select_thread(None));
    submit(&mut runs, "new task");
    assert_eq!(
        failed(&mut runs, 1, 7, ErrorKey::SessionEnded),
        Some(Content::from("new task"))
    );
}

#[test]
fn any_other_ending_uses_the_text_up_and_a_later_valid_failure_cannot_revive_it() {
    let others = [
        Some(ErrorKey::UsageLimit),
        Some(ErrorKey::RequestRefused),
        Some(ErrorKey::AccountOffline),
        None,
    ];
    for key in others {
        let mut runs = ready();
        submit(&mut runs, "x");
        thinking(&mut runs, 1, 1);
        assert_eq!(
            ended(&mut runs, 1, 1, Phase::Failed, key).restore,
            None,
            "{key:?}"
        );
        assert_eq!(runs.kept(), Vec::<usize>::new(), "{key:?}");
        assert_eq!(
            failed(&mut runs, 1, 1, ErrorKey::SessionEnded),
            None,
            "{key:?}"
        );
    }
    for phase in [Phase::Done, Phase::Interrupted] {
        let mut runs = ready();
        submit(&mut runs, "x");
        thinking(&mut runs, 1, 1);
        let applied = ended(&mut runs, 1, 1, phase, Some(ErrorKey::SessionEnded));
        assert_eq!(applied.restore, None, "{phase:?}");
        assert_eq!(runs.kept(), Vec::<usize>::new(), "{phase:?}");
        assert_eq!(
            failed(&mut runs, 1, 1, ErrorKey::SessionEnded),
            None,
            "{phase:?}"
        );
    }
    // A turn the brain refused before naming a run takes its text with it.
    let mut runs = ready();
    submit(&mut runs, "x");
    runs.refused();
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);
}

#[test]
fn queued_turns_map_to_their_own_runs_and_any_later_attempt_drops_earlier_texts() {
    let mut runs = ready();
    submit(&mut runs, "a");
    assert_eq!(runs.kept(), vec![1]);
    submit(&mut runs, "bb");
    assert_eq!(runs.kept(), vec![2], "b's attempt dropped a's text");
    assert_eq!(
        failed(&mut runs, 1, 1, ErrorKey::SessionEnded),
        None,
        "run 1 is a's"
    );
    assert_eq!(
        failed(&mut runs, 2, 1, ErrorKey::SessionEnded),
        Some(Content::from("bb"))
    );

    let mut runs = ready();
    submit(&mut runs, "c");
    assert_eq!(
        runs.submit("  ", |_| panic!("blank isn't sent")),
        Err(Refused::Blank)
    );
    assert_eq!(runs.kept(), Vec::<usize>::new(), "a refused attempt counts");
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);
}

#[test]
fn a_selection_change_drops_the_text_even_after_coming_back() {
    let mut runs = ready();
    submit(&mut runs, "x");
    thinking(&mut runs, 1, 1);
    assert!(runs.select_thread(Some(ThreadId(2))));
    assert_eq!(runs.kept(), Vec::<usize>::new());
    runs.back_to_kumo();
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);

    let mut runs = ready();
    assert!(runs.select_thread(Some(ThreadId(2))));
    submit(&mut runs, "y");
    thinking(&mut runs, 1, 2);
    runs.back_to_kumo();
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert!(runs.select_thread(Some(ThreadId(2))));
    assert_eq!(failed(&mut runs, 1, 2, ErrorKey::SessionEnded), None);
}

/// H1: the text belongs to the thread it was typed to, so a run of another thread never takes
/// it, and a target that now resolves elsewhere never gets it, even with no new selection.
#[test]
fn a_run_or_target_of_another_thread_never_restores() {
    // Typed while Kumo was thread 1; new settings moved Kumo to 3 before the run was named.
    let mut runs = ready();
    submit(&mut runs, "for one");
    runs.settings(ThreadId(3));
    thinking(&mut runs, 1, 3);
    assert_eq!(
        runs.kept(),
        Vec::<usize>::new(),
        "run 1 continues 3, not the typed 1"
    );
    assert_eq!(failed(&mut runs, 1, 3, ErrorKey::SessionEnded), None);

    // Typed to 1 and answered in 1, but input now goes to 3.
    let mut runs = ready();
    submit(&mut runs, "for one");
    thinking(&mut runs, 1, 1);
    runs.settings(ThreadId(3));
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);
}

#[test]
fn esc_drops_the_text_whether_it_stops_cancels_or_clears() {
    let mut runs = ready();
    submit(&mut runs, "x");
    thinking(&mut runs, 1, 1);
    assert!(runs.stop().is_some());
    assert_eq!(runs.kept(), Vec::<usize>::new());
    let (applied, notice) = runs.apply_run_state(
        RunId(1),
        ThreadId(1),
        Phase::Failed,
        Some(ErrorKey::SessionEnded.into()),
    );
    assert_eq!((applied.restore, notice), (None, None));

    let mut runs = ready();
    submit(&mut runs, "x");
    assert_eq!(
        runs.stop(),
        None,
        "the turn is cancelled before its run is named"
    );
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);

    let mut runs = ready();
    submit(&mut runs, "x");
    thinking(&mut runs, 1, 1);
    runs.clear();
    assert_eq!(runs.kept(), Vec::<usize>::new());
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);
}

#[test]
fn a_confirmed_delete_drops_its_thread_s_text_even_when_another_is_the_target() {
    // Kumo was thread 1 when typed and is 5 when 1 is deleted, so the target doesn't change.
    let mut runs = ready();
    submit(&mut runs, "pending");
    runs.settings(ThreadId(5));
    assert!(runs.confirm_delete(ThreadId(1)));
    assert_eq!(runs.kept(), Vec::<usize>::new());

    let mut runs = ready();
    submit(&mut runs, "named");
    thinking(&mut runs, 1, 1);
    runs.settings(ThreadId(5));
    assert!(runs.confirm_delete(ThreadId(1)));
    assert_eq!(runs.kept(), Vec::<usize>::new());
    runs.settings(ThreadId(1));
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);

    // Another thread's delete leaves it alone.
    let mut runs = ready();
    submit(&mut runs, "kept");
    assert!(runs.confirm_delete(ThreadId(9)));
    assert_eq!(runs.kept(), vec![4]);
}

#[test]
fn a_disconnect_or_a_new_brain_drops_the_text() {
    let mut runs = ready();
    submit(&mut runs, "x");
    thinking(&mut runs, 1, 1);
    runs.disconnected();
    assert_eq!(runs.kept(), Vec::<usize>::new());
    // A new brain numbers runs afresh, so its run 1 isn't the old one.
    runs.met(Incarnation(2));
    runs.connected(2);
    runs.settings(ThreadId(1));
    assert_eq!(failed(&mut runs, 1, 1, ErrorKey::SessionEnded), None);
}

#[test]
fn a_new_brain_drops_held_texts_even_without_a_disconnect() {
    let mut runs = ready();
    submit(&mut runs, "x");
    thinking(&mut runs, 1, 1);
    runs.met(Incarnation(1));
    assert_eq!(runs.kept(), vec![1], "the same brain keeps it");
    runs.met(Incarnation(2));
    assert_eq!(runs.kept(), Vec::<usize>::new());
}

fn assert_redacted(debug: &str) {
    assert!(!debug.contains(MARKER), "{debug}");
    let bytes = format!("{:?}", MARKER.as_bytes());
    assert!(!debug.contains(bytes.trim_matches(['[', ']'])), "{debug}");
}

#[test]
fn debug_prints_lengths_never_typed_or_answer_text() {
    let mut runs = ready();
    submit(&mut runs, MARKER);
    runs.apply_data(
        DataToShell::AssistantText {
            run: RunId(1),
            delta: MARKER.into(),
        },
        10,
    );
    let pending = format!("{runs:?}");
    assert_redacted(&pending);
    assert!(pending.contains("held: 1"), "{pending}");

    thinking(&mut runs, 1, 1);
    let shown = runs.shown().expect("the answer shows");
    assert_eq!(shown.text, MARKER);
    let debug = format!("{shown:?}");
    assert_redacted(&debug);
    assert!(
        debug.contains(&format!("text_len: {}", MARKER.len())),
        "{debug}"
    );
    let named = format!("{runs:?}");
    assert_redacted(&named);
    assert!(
        named.contains(&format!("text_len: Some({})", MARKER.len())),
        "{named}"
    );

    let applied = ended(&mut runs, 1, 1, Phase::Failed, Some(ErrorKey::SessionEnded));
    assert_eq!(applied.restore.as_deref(), Some(MARKER));
    let debug = format!("{applied:?}");
    assert_redacted(&debug);
    assert!(
        debug.contains(&format!("restore_len: Some({})", MARKER.len())),
        "{debug}"
    );
}
