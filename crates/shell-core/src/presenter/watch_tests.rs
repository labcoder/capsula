use std::time::{Duration, Instant};

use kumochi_protocol::message::{RunId, SceneId};

use super::{MARKS_KEPT, State, Watch};

/// The newest run named when the keys came up.
const AFTER: RunId = RunId(4);
/// The hold's run.
const HOLD: RunId = RunId(5);
const OTHER: RunId = RunId(6);
const FIRST: SceneId = SceneId(20);
const SECOND: SceneId = SceneId(21);

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
fn no_run_s_end_ends_an_unnamed_watch_and_the_hold_s_run_is_still_named_and_shown() {
    let now = Instant::now();
    let mut watch = released(now);
    // Older runs, and a newer one that can be an earlier hold's: until a name says which run
    // is the hold's, none of their ends is the watch's.
    for run in [AFTER, RunId(3), HOLD] {
        watch.ended(run, ms(now, 10_000));
        assert_eq!(watch.state, expecting(now, None));
    }
    assert!(watch.listening());
    assert_eq!(watch.deadline(), Some(ms(now, 30_000)));
    // That newer run's mark is not shown, and the hold's own run is named and shown after it.
    assert!(!watch.marked(FIRST, HOLD));
    watch.named(OTHER, ms(now, 29_999));
    assert_eq!(watch.state, expecting(now, Some(OTHER)));
    assert!(watch.marked(SECOND, OTHER));

    // With no name the watch ends at the release's own limit, which that end didn't move.
    let mut watch = released(now);
    watch.ended(HOLD, ms(now, 10_000));
    assert!(!watch.expired(ms(now, 29_999)));
    assert_eq!(watch.state, expecting(now, None));
    assert!(!watch.expired(ms(now, 30_000)));
    assert_eq!(watch.state, State::Idle);
    watch.named(OTHER, ms(now, 30_000));
    assert_eq!(watch.state, State::Idle);
    assert!(!watch.listening());
    assert!(!watch.marked(SECOND, OTHER));
}

#[test]
fn a_replaced_scene_passes_its_place_to_its_run_s_successor() {
    let mut watch = showing();
    assert!(watch.replaced(&[FIRST], SECOND));
    assert_eq!(watch.deadline(), None);
    assert_eq!(watch.shown(), None);
    assert!(!watch.gone(FIRST));
    assert!(!watch.marked(SECOND, OTHER));
    assert!(
        watch.listening(),
        "Watch itself does not put the wrong run in the successor's place"
    );
    assert!(watch.marked(SECOND, HOLD));
    assert!(watch.shows(SECOND));
    assert_eq!(watch.skipped(), None);
}
