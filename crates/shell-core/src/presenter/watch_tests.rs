use std::time::{Duration, Instant};

use kumochi_protocol::message::{RunId, SceneId};

use super::{MARKS_KEPT, State, UNNAMED_KEPT, Watch};

/// The newest run named when the keys came up.
const AFTER: RunId = RunId(4);
/// The hold's run.
const HOLD: RunId = RunId(5);
const OTHER: RunId = RunId(6);
const FIRST: SceneId = SceneId(20);
const SECOND: SceneId = SceneId(21);
const THIRD: SceneId = SceneId(22);

fn ms(now: Instant, millis: u64) -> Instant {
    now + Duration::from_millis(millis)
}

/// A watch released at `now`.
fn released(now: Instant) -> Watch {
    let mut watch = Watch::default();
    watch.released(AFTER, now);
    watch
}

fn showing() -> Watch {
    let now = Instant::now();
    let mut watch = released(now);
    watch.named(HOLD, now);
    assert!(watch.marked(FIRST, HOLD));
    watch
}

/// The state a release at `now` waits in while nothing has changed.
fn expecting(now: Instant, run: Option<RunId>) -> State {
    State::Expecting {
        after: AFTER,
        run,
        changed: false,
        until: ms(now, 30_000),
    }
}

/// The same, once something may have changed the screen.
fn changed(now: Instant, run: Option<RunId>) -> State {
    State::Expecting {
        after: AFTER,
        run,
        changed: true,
        until: ms(now, 30_000),
    }
}

#[test]
fn an_idle_watch_shows_nothing_and_listens_to_nothing() {
    let mut watch = Watch::default();
    assert!(!watch.listening());
    assert_eq!(watch.deadline(), None);
    assert!(!watch.marked(FIRST, HOLD));
    assert_eq!(watch.state, State::Idle);
    assert_eq!(watch.skipped(), None);
    assert!(!watch.changed());
    assert_eq!(watch.state, State::Idle);
}

#[test]
fn a_release_expects_the_first_mark_of_a_newer_run() {
    let now = Instant::now();
    let mut watch = released(now);
    assert_eq!(watch.state, expecting(now, None));
    assert!(watch.listening());
    for old in [AFTER, RunId(3)] {
        assert!(!watch.marked(FIRST, old));
    }
    assert_eq!(watch.skipped(), None);
    assert!(watch.listening());
    // A newer run's mark before any run is named is skipped, and the watch goes on.
    assert!(!watch.marked(THIRD, OTHER));
    assert_eq!(watch.skipped(), Some(OTHER));
    assert_eq!(watch.state, expecting(now, None));
    assert!(watch.listening());
    watch.named(HOLD, now);
    assert!(watch.marked(FIRST, HOLD));
    assert_eq!(
        watch.state,
        State::Showing {
            scene: FIRST,
            run: HOLD,
            ends: None,
        }
    );
    assert!(watch.shows(FIRST));
    assert!(!watch.shows(SECOND));
    assert!(watch.listening());
    assert_eq!(watch.deadline(), None);
}

#[test]
fn only_the_named_run_is_shown_and_only_the_first_newer_name_counts() {
    let now = Instant::now();
    let mut watch = released(now);
    watch.named(AFTER, now);
    assert_eq!(watch.state, expecting(now, None));
    watch.named(HOLD, now);
    watch.named(OTHER, now);
    assert_eq!(watch.state, expecting(now, Some(HOLD)));
    assert!(!watch.marked(FIRST, OTHER));
    assert_eq!(watch.state, expecting(now, Some(HOLD)));
    assert_eq!(watch.skipped(), None);
    assert!(watch.marked(FIRST, HOLD));
}

#[test]
fn a_change_before_the_first_mark_stays_and_names_the_skipped_run() {
    let now = Instant::now();
    let mut watch = released(now);
    assert!(!watch.changed());
    assert_eq!(watch.state, changed(now, None));
    assert!(!watch.listening());
    assert_eq!(watch.skipped(), None);
    assert!(!watch.marked(FIRST, HOLD));
    assert_eq!(watch.skipped(), Some(HOLD));
    // Nothing later takes the change back.
    watch.named(HOLD, now);
    assert!(!watch.changed());
    assert_eq!(watch.state, changed(now, Some(HOLD)));
    assert!(!watch.marked(SECOND, HOLD));
    assert_eq!(watch.state, changed(now, Some(HOLD)));
    watch.released(HOLD, now);
    assert_eq!(watch.skipped(), None);
    assert!(watch.listening());
    watch.named(OTHER, now);
    assert!(watch.marked(FIRST, OTHER));
}

#[test]
fn a_change_clears_the_shown_scene_and_a_later_one_is_not_shown() {
    let mut watch = showing();
    assert!(watch.changed());
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.listening());
    assert!(!watch.shows(FIRST));
    assert!(!watch.marked(SECOND, HOLD));
    assert_eq!(watch.skipped(), None);
}

#[test]
fn a_second_scene_of_the_shown_run_takes_its_place_and_another_run_is_skipped() {
    let mut watch = showing();
    let now = Instant::now();
    watch.ended(HOLD, now);
    assert!(watch.marked(SECOND, HOLD));
    assert_eq!(
        watch.state,
        State::Showing {
            scene: SECOND,
            run: HOLD,
            ends: Some(ms(now, 60_000)),
        }
    );
    assert!(!watch.marked(FIRST, OTHER));
    assert!(watch.shows(SECOND));
    assert_eq!(watch.skipped(), None);
}

#[test]
fn the_limit_runs_sixty_seconds_from_the_first_end_of_the_shown_run() {
    assert_eq!(MARKS_KEPT, Duration::from_secs(60));
    let mut watch = showing();
    let now = Instant::now();
    watch.ended(OTHER, now);
    assert_eq!(watch.deadline(), None);
    assert!(!watch.expired(ms(now, 3_600_000)));
    watch.ended(HOLD, now);
    watch.ended(HOLD, ms(now, 10_000));
    assert_eq!(watch.deadline(), Some(ms(now, 60_000)));
    assert!(!watch.expired(ms(now, 59_999)));
    assert!(watch.shows(FIRST));
    assert!(watch.listening());
    assert!(watch.expired(ms(now, 60_000)));
    assert_eq!(watch.state, State::Idle);
    assert_eq!(watch.deadline(), None);
    assert!(!watch.expired(ms(now, 60_000)));
}

#[test]
fn an_unnamed_release_ends_thirty_seconds_later_and_stays_ended() {
    assert_eq!(UNNAMED_KEPT, Duration::from_secs(30));
    let now = Instant::now();
    let mut watch = released(now);
    assert_eq!(watch.deadline(), Some(ms(now, 30_000)));
    assert!(!watch.expired(ms(now, 29_999)));
    assert_eq!(watch.state, expecting(now, None));
    assert!(watch.listening());
    // Nothing is shown, so nothing is cleared; the watch just ends.
    assert!(!watch.expired(ms(now, 30_000)));
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.listening());
    assert_eq!(watch.deadline(), None);
    watch.named(HOLD, now);
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.marked(FIRST, HOLD));
    assert_eq!(watch.skipped(), None);
}

#[test]
fn a_named_run_is_not_cut_off_thirty_seconds_after_the_release() {
    let now = Instant::now();
    let mut watch = released(now);
    watch.named(HOLD, now);
    assert_eq!(watch.deadline(), None);
    assert!(!watch.expired(ms(now, 30_000)));
    assert!(!watch.expired(ms(now, 3_600_000)));
    assert_eq!(watch.state, expecting(now, Some(HOLD)));
    assert!(watch.listening());
    assert!(watch.marked(FIRST, HOLD));
}

#[test]
fn the_named_run_ending_without_a_scene_ends_the_watch() {
    let now = Instant::now();
    let mut watch = released(now);
    watch.named(HOLD, now);
    watch.ended(OTHER, now);
    assert!(watch.listening());
    watch.ended(HOLD, now);
    assert_eq!(watch.state, State::Idle);
    assert_eq!(watch.deadline(), None);
    assert!(!watch.marked(FIRST, HOLD));
    assert_eq!(watch.skipped(), None);
}

#[test]
fn a_dropped_hold_ends_an_expecting_watch_only() {
    let mut watch = released(Instant::now());
    watch.dropped();
    assert_eq!(watch.state, State::Idle);
    let mut watch = showing();
    watch.dropped();
    assert!(watch.shows(FIRST));
}

#[test]
fn the_shown_scene_leaving_ends_the_watch() {
    let mut watch = showing();
    assert!(!watch.gone(SECOND));
    assert!(watch.shows(FIRST));
    assert!(watch.gone(FIRST));
    assert_eq!(watch.state, State::Idle);
    let mut watch = released(Instant::now());
    assert!(!watch.gone(FIRST));
    assert!(watch.listening());
}

#[test]
fn a_newer_run_that_ends_before_any_name_ends_the_watch_and_a_late_name_names_nothing() {
    let now = Instant::now();
    let mut watch = released(now);
    for old in [AFTER, RunId(3)] {
        watch.ended(old, now);
        assert_eq!(watch.state, expecting(now, None));
    }
    watch.ended(HOLD, now);
    assert_eq!(watch.state, State::Idle);
    // No second end comes: the late name must not bring the watch back.
    watch.named(HOLD, now);
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.listening());
    assert_eq!(watch.deadline(), None);
    assert!(!watch.marked(FIRST, HOLD));
}

#[test]
fn a_name_at_the_unnamed_limit_ends_the_watch_instead_of_lifting_the_limit() {
    let now = Instant::now();
    let mut watch = released(now);
    watch.named(HOLD, ms(now, 29_999));
    assert_eq!(watch.state, expecting(now, Some(HOLD)));
    assert_eq!(watch.deadline(), None);
    let mut watch = released(now);
    watch.named(HOLD, ms(now, 30_000));
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.listening());
    assert!(!watch.marked(FIRST, HOLD));
}

/// The shown scene replaced while its run goes on.
fn passing() -> Watch {
    let mut watch = showing();
    assert!(watch.replaced(&[SECOND, FIRST], THIRD));
    assert_eq!(
        watch.state,
        State::Passing {
            successor: THIRD,
            run: HOLD,
        }
    );
    watch
}

#[test]
fn a_replaced_scene_passes_its_place_to_its_run_s_successor() {
    let mut watch = showing();
    assert!(!watch.replaced(&[SECOND], THIRD));
    assert!(watch.shows(FIRST));
    let mut watch = passing();
    assert!(watch.listening());
    assert_eq!(watch.deadline(), None);
    assert_eq!(watch.shown(), None);
    // The replaced scene leaving the mirror ends nothing.
    assert!(!watch.gone(FIRST));
    // Another scene of the run, or the successor from another run, takes nothing.
    assert!(!watch.marked(SECOND, HOLD));
    assert!(!watch.marked(THIRD, OTHER));
    assert!(watch.listening());
    assert!(watch.marked(THIRD, HOLD));
    assert_eq!(
        watch.state,
        State::Showing {
            scene: THIRD,
            run: HOLD,
            ends: None,
        }
    );
    assert_eq!(watch.skipped(), None);
}

#[test]
fn a_successor_that_never_shows_a_mark_ends_the_watch() {
    let now = Instant::now();
    let mut watch = passing();
    assert!(!watch.gone(THIRD));
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.marked(THIRD, HOLD));

    let mut watch = passing();
    watch.ended(OTHER, now);
    assert!(watch.listening());
    watch.ended(HOLD, now);
    assert_eq!(watch.state, State::Idle);

    let mut watch = passing();
    assert!(!watch.changed());
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.marked(THIRD, HOLD));
    assert_eq!(watch.skipped(), None);

    // A scene whose run has ended has no successor: it leaves as any removed scene.
    let mut watch = showing();
    watch.ended(HOLD, now);
    assert!(!watch.replaced(&[FIRST], THIRD));
    assert!(watch.shows(FIRST));
    assert!(watch.gone(FIRST));
    assert_eq!(watch.state, State::Idle);
}
