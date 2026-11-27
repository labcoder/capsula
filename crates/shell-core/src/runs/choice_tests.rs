//! The target's model choice, read by id and driven by [`Runs`]' own selection and connection:
//! focus, promotion, Kumo's id arriving, a delete and a reconnect each ask again, and no answer
//! asked under an older key, an earlier write or a lost one can win.

use std::time::Instant;

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ConnectionKind, ControlToBrain, ControlToShell, Effort, Incarnation,
    ModelChoice, ModelSlug, Phase, RunId, RunOrigin, SavedChoice, ThreadId, ThreadOp,
};

use super::{Choice, MODEL_READ_RETRY, MODEL_WRITE_TIME, Runs};

const KUMO: ThreadId = ThreadId(1);
const TASK: ThreadId = ThreadId(9);
const OTHER: ThreadId = ThreadId(12);

fn picked() -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new("picked-model").expect("valid"),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    }
}

fn found(choice: SavedChoice, revision: u64) -> ChoiceRead {
    ChoiceRead::Found {
        choice,
        revision: ChoiceRev(revision),
    }
}

fn read(request: u32, thread: ThreadId) -> ControlToBrain {
    ControlToBrain::ReadModel { request, thread }
}

fn answer(request: u32, thread: ThreadId, result: ChoiceRead) -> ControlToShell {
    ControlToShell::ThreadModelRead {
        request,
        thread,
        result,
    }
}

fn written(request: u32, thread: ThreadId, result: ChoiceRead) -> ControlToShell {
    ControlToShell::ThreadModelSet {
        request,
        thread,
        result,
    }
}

/// Connected, with Kumo's id known and nothing read yet.
fn connected() -> Runs {
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    runs.settings(KUMO);
    runs
}

/// Focused on `thread` with its first read answered `result`.
fn settled(thread: ThreadId, result: ChoiceRead) -> (Runs, Instant) {
    let now = Instant::now();
    let mut runs = connected();
    runs.select_thread(Some(thread));
    assert_eq!(runs.model_read(now), Some(read(1, thread)));
    runs.apply_model(&answer(1, thread, result), now);
    (runs, now)
}

#[test]
fn kumo_focus_promotion_and_reconnect_each_ask_again() {
    let now = Instant::now();
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    assert_eq!(
        runs.model_choice(),
        Choice::NoThread,
        "Kumo's id hasn't arrived"
    );
    assert_eq!(runs.model_read(now), None);
    runs.settings(KUMO);
    assert_eq!(
        runs.model_read(now),
        Some(read(1, KUMO)),
        "Kumo's chat is read like a task"
    );
    assert_eq!(runs.model_read(now), None, "one read per key");
    runs.apply_model(&answer(1, KUMO, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        }
    );

    runs.select_thread(Some(TASK));
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "Kumo's answer isn't the task's"
    );
    assert_eq!(runs.model_read(now), Some(read(2, TASK)));
    runs.apply_model(
        &answer(2, TASK, found(SavedChoice::Chosen(picked()), 4)),
        now,
    );

    runs.select_thread(None);
    assert_eq!(runs.model_choice(), Choice::NoThread);
    assert_eq!(runs.model_read(now), None);
    runs.submit("first words", |_| true).expect("sent");
    runs.apply_state(
        RunId(1),
        OTHER,
        Phase::Thinking,
        Some(RunOrigin::Typed { turn: 1 }),
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "promoted, not read yet"
    );
    assert_eq!(runs.model_read(now), Some(read(3, OTHER)));
    runs.apply_model(&answer(3, OTHER, found(SavedChoice::Inherit, 0)), now);

    runs.disconnected();
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "nothing is known offline"
    );
    assert_eq!(runs.model_read(now), None);
    runs.connected(2);
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "the earlier pair's answer is gone"
    );
    assert_eq!(
        runs.model_read(now),
        Some(read(1, OTHER)),
        "a new pair numbers from one"
    );
}

/// A→B→A with A's first read outstanding: that answer belongs to the old selection and is
/// dropped; the next read waits for it, then asks for the current A.
#[test]
fn an_answer_asked_under_an_earlier_selection_is_dropped() {
    let now = Instant::now();
    let mut runs = connected();
    runs.select_thread(Some(TASK));
    assert_eq!(runs.model_read(now), Some(read(1, TASK)));
    runs.select_thread(Some(OTHER));
    runs.select_thread(Some(TASK));
    assert_eq!(
        runs.model_read(now),
        None,
        "the old read still holds its slot"
    );
    runs.apply_model(
        &answer(1, TASK, found(SavedChoice::Chosen(picked()), 2)),
        now,
    );
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(runs.model_read(now), Some(read(2, TASK)));
    runs.apply_model(&answer(2, OTHER, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "another thread's answer"
    );
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));
    runs.apply_model(&written(3, TASK, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "a write answer for a read's number"
    );
    runs.apply_model(&answer(3, TASK, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        }
    );
}

/// The committed answer is used, never the requested value, and a Clear's answer replaces a
/// pinned one.
#[test]
fn set_and_clear_use_the_committed_answer() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    let set = runs.set_model(Some(picked()), now).expect("a write");
    assert_eq!(
        set,
        ControlToBrain::ThreadOp(ThreadOp::SetModel {
            request: 2,
            thread: TASK,
            choice: Some(picked()),
        })
    );
    assert_eq!(runs.model_choice(), Choice::Unknown, "unanswered");
    assert_eq!(runs.set_model(None, now), None, "one write at a time");
    let other = ModelChoice {
        model: ModelSlug::new("committed-model").expect("valid"),
        ..picked()
    };
    runs.apply_model(
        &written(2, TASK, found(SavedChoice::Chosen(other.clone()), 1)),
        now,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Chosen(other),
            revision: ChoiceRev(1)
        }
    );
    assert_eq!(
        runs.set_model(None, now),
        Some(ControlToBrain::ThreadOp(ThreadOp::SetModel {
            request: 3,
            thread: TASK,
            choice: None,
        }))
    );
    runs.apply_model(&written(3, TASK, found(SavedChoice::Inherit, 2)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(2)
        }
    );
}

#[test]
fn no_write_goes_before_the_chat_is_known_to_exist() {
    let now = Instant::now();
    let mut runs = connected();
    runs.select_thread(Some(TASK));
    assert_eq!(runs.set_model(Some(picked()), now), None, "unread");
    assert_eq!(runs.model_read(now), Some(read(1, TASK)));
    assert_eq!(runs.set_model(None, now), None, "the read is unanswered");
    runs.apply_model(&answer(1, TASK, ChoiceRead::Failed), now);
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(
        runs.set_model(None, now),
        None,
        "a failed read proves nothing"
    );
    let (mut runs, now) = settled(TASK, ChoiceRead::Missing);
    assert_eq!(runs.model_choice(), Choice::Missing);
    assert_eq!(runs.set_model(None, now), None, "a missing chat");
}

/// A write whose answer is late, refused or failed leaves the choice unknown until a read sent
/// after it answers; the late answer itself never lands.
#[test]
fn a_lost_write_is_settled_only_by_a_later_read() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    runs.set_model(Some(picked()), now).expect("a write");
    assert_eq!(
        runs.model_read(now),
        None,
        "nothing is owed while it's in time"
    );
    let late = now + MODEL_WRITE_TIME;
    assert_eq!(runs.model_read(late), Some(read(3, TASK)));
    runs.apply_model(
        &written(2, TASK, found(SavedChoice::Chosen(picked()), 1)),
        now,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "the late answer isn't used"
    );
    runs.apply_model(
        &answer(3, TASK, found(SavedChoice::Chosen(picked()), 1)),
        now,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Chosen(picked()),
            revision: ChoiceRev(1)
        }
    );

    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    let set = runs.set_model(None, now).expect("a write");
    runs.model_unsent(&set);
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));

    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    runs.set_model(Some(picked()), now).expect("a write");
    runs.apply_model(&written(2, TASK, ChoiceRead::Failed), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "never an assumed rollback"
    );
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));
    runs.apply_model(&answer(3, TASK, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        }
    );
}

/// No answer lowers the newest revision seen, a late failure never erases a found one, and a
/// deleted chat stays missing.
#[test]
fn an_older_revision_or_a_late_failure_never_wins() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    runs.set_model(Some(picked()), now).expect("a write");
    runs.apply_model(
        &written(2, TASK, found(SavedChoice::Chosen(picked()), 5)),
        now,
    );
    runs.set_model(None, now).expect("a write");
    runs.apply_model(&written(3, TASK, ChoiceRead::Failed), now);
    assert_eq!(runs.model_read(now), Some(read(4, TASK)));
    runs.apply_model(&answer(4, TASK, found(SavedChoice::Inherit, 4)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Unknown,
        "revision 4 is older than 5"
    );

    let (mut runs, now) = settled(TASK, found(SavedChoice::Chosen(picked()), 3));
    let set = runs.set_model(None, now).expect("a write");
    runs.model_unsent(&set);
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));
    runs.apply_model(&answer(3, TASK, ChoiceRead::Failed), now);
    assert_eq!(runs.model_choice(), Choice::Unknown);

    let (mut runs, now) = settled(TASK, ChoiceRead::Missing);
    runs.select_thread(Some(TASK));
    assert_eq!(runs.model_read(now), Some(read(2, TASK)));
    runs.apply_model(&answer(2, TASK, ChoiceRead::Missing), now);
    runs.select_thread(Some(TASK));
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));
    runs.apply_model(&answer(3, TASK, found(SavedChoice::Inherit, 0)), now);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        },
        "a new selection asks afresh"
    );
}

#[test]
fn a_confirmed_delete_moves_the_choice_back_to_kumo() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Chosen(picked()), 1));
    assert!(runs.confirm_delete(TASK));
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(runs.model_read(now), Some(read(2, KUMO)));
    runs.apply_model(&answer(2, KUMO, found(SavedChoice::Inherit, 0)), now);
    runs.apply_model(
        &answer(1, TASK, found(SavedChoice::Chosen(picked()), 1)),
        now,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        },
        "a late answer for the deleted chat doesn't restore it"
    );
}

/// A failed read asks again for the same chat after a pause, never at once, and its answer
/// settles the choice without a refocus.
#[test]
fn a_failed_read_is_retried_after_a_pause() {
    let (mut runs, now) = settled(TASK, ChoiceRead::Failed);
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(runs.model_read(now), None, "no immediate retry");
    let later = now + MODEL_READ_RETRY;
    assert_eq!(runs.model_read(later), Some(read(2, TASK)));
    assert_eq!(runs.model_read(later), None, "one read at a time");
    runs.apply_model(
        &answer(2, TASK, found(SavedChoice::Chosen(picked()), 1)),
        later,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Chosen(picked()),
            revision: ChoiceRev(1)
        }
    );

    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    let set = runs.set_model(None, now).expect("a write");
    runs.model_unsent(&set);
    assert_eq!(runs.model_read(now), Some(read(3, TASK)));
    runs.apply_model(&answer(3, TASK, ChoiceRead::Failed), now);
    let later = now + MODEL_READ_RETRY;
    assert_eq!(
        runs.model_read(later),
        Some(read(4, TASK)),
        "the reread after a lost write is retried too"
    );
    runs.apply_model(&answer(4, TASK, found(SavedChoice::Inherit, 0)), later);
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Inherit,
            revision: ChoiceRev(0)
        }
    );
}

/// A write answer for another thread neither settles the write nor frees it for another.
#[test]
fn a_write_answer_for_another_thread_is_ignored() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    runs.set_model(Some(picked()), now).expect("a write");
    runs.apply_model(&written(2, OTHER, found(SavedChoice::Inherit, 7)), now);
    assert_eq!(runs.model_choice(), Choice::Unknown, "still unanswered");
    assert_eq!(
        runs.set_model(None, now),
        None,
        "the write is still outstanding"
    );
    runs.apply_model(
        &written(2, TASK, found(SavedChoice::Chosen(picked()), 1)),
        now,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Chosen(picked()),
            revision: ChoiceRev(1)
        }
    );
}

/// An answer that arrives after the write's deadline, with no pump in between, is not used:
/// only a read sent afterwards settles the choice.
#[test]
fn a_write_answer_past_its_deadline_is_not_used() {
    let (mut runs, now) = settled(TASK, found(SavedChoice::Inherit, 0));
    runs.set_model(Some(picked()), now).expect("a write");
    let late = now + MODEL_WRITE_TIME;
    runs.apply_model(
        &written(2, TASK, found(SavedChoice::Chosen(picked()), 1)),
        late,
    );
    assert_eq!(runs.model_choice(), Choice::Unknown);
    assert_eq!(runs.model_read(late), Some(read(3, TASK)));
    runs.apply_model(
        &answer(3, TASK, found(SavedChoice::Chosen(picked()), 1)),
        late,
    );
    assert_eq!(
        runs.model_choice(),
        Choice::Found {
            choice: SavedChoice::Chosen(picked()),
            revision: ChoiceRev(1)
        }
    );
}
