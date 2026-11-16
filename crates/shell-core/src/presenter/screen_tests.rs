//! Screen scenes through the presenter: what is shown, what is skipped and what clears, with
//! sources the tests script.

use std::{cell::RefCell, rc::Rc, time::Instant};

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, RunId, SceneHeader, SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{
    Record, SceneEvent, Step, parse_screen_draw_record, record::ScreenDisplay, screen,
    text::Bounded,
};

use super::{
    Effect, FRAME, GENERATION, LIVE, Presenter, RUN, Shaper, arrive, begin, commit_as, controls,
    credit, data, described, jobs, live, ms, only_admission, only_job, presenter, recovery, rect,
    scene_ids, snapshot,
};
use crate::platform::screen_watch::{Changed, Sources};

/// The newest run named when the keys came up; [`RUN`] is the hold's.
const AFTER: RunId = RunId(4);
const OTHER: RunId = RunId(6);
const MARKS: SceneId = SceneId(30);
const SECOND: SceneId = SceneId(31);

#[derive(Default)]
struct Log {
    starts: u32,
    stops: u32,
    bit: Option<Changed>,
}

struct Scripted {
    log: Rc<RefCell<Log>>,
    starts: bool,
}

impl Sources for Scripted {
    fn start(&mut self, changed: Changed) -> bool {
        let mut log = self.log.borrow_mut();
        log.starts += 1;
        log.bit = Some(changed);
        self.starts
    }

    fn stop(&mut self) {
        self.log.borrow_mut().stops += 1;
    }
}

/// A live presenter whose sources start, and their log.
fn watched() -> (Presenter, Instant, Rc<RefCell<Log>>) {
    let (mut presenter, now) = live();
    let log = Rc::<RefCell<Log>>::default();
    presenter.watch_sources(Box::new(Scripted {
        log: Rc::clone(&log),
        starts: true,
    }));
    (presenter, now, log)
}

/// How often the sources were started and stopped.
fn counts(log: &RefCell<Log>) -> (u32, u32) {
    let log = log.borrow();
    (log.starts, log.stops)
}

/// A source sees one of its events.
fn set(log: &RefCell<Log>) {
    log.borrow().bit.as_ref().expect("started sources").set();
}

fn screen_header() -> SceneHeader {
    let source = br#"{"begin":{"scene":"marks","surface":"screen","alt":""}}"#;
    let Ok(Record::Begin(mut begin)) = parse_screen_draw_record(source) else {
        panic!("a screen begin record");
    };
    let display = ScreenDisplay {
        origin: [0, 0],
        size: [2560, 1440],
        dpi: 96,
    };
    screen::stamp(&mut begin, [1280, 720], display).expect("stamped");
    SceneHeader {
        begin,
        layout_version: super::super::LAYOUT_VERSION,
    }
}

fn mark() -> Step {
    let source =
        br#"{"step":{"id":"s","put":[{"highlight":{"id":"h","target":{"box":[1,2,3,4]}}}]}}"#;
    let Ok(Record::Step(step)) = parse_screen_draw_record(source) else {
        panic!("a screen step record");
    };
    step
}

fn begin_frame(scene: SceneId) -> DataToShell {
    DataToShell::SceneBegin {
        scene,
        header: screen_header(),
    }
}

fn mark_frame(run: RunId, scene: SceneId) -> DataToShell {
    DataToShell::SceneStepCommit {
        run,
        scene,
        index: 0,
        step: mark(),
    }
}

/// A screen scene begins and its first mark arrives, from `run`.
fn marked(presenter: &mut Presenter, run: RunId, scene: SceneId, now: Instant) -> Vec<Effect> {
    let effects = data(presenter, begin_frame(scene), now).expect("a screen scene");
    assert!(controls(&effects).is_empty());
    let effects = commit_as(presenter, run, scene, 0, mark(), now).expect("a first mark");
    assert_eq!(credit(&effects), FRAME);
    effects
}

/// A watch with its first mark shown, and the mark's preparation.
fn shown() -> (Presenter, Instant, Rc<RefCell<Log>>, Vec<Effect>) {
    let (mut presenter, now, log) = watched();
    assert!(presenter.screen_released(AFTER, now).is_empty());
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    (presenter, now, log, effects)
}

fn cleared(scenes: &[SceneId]) -> ControlToBrain {
    ControlToBrain::ClearDrawing {
        scene_ids: scenes.to_vec(),
    }
}

fn refused(scene: SceneId) -> ControlToBrain {
    ControlToBrain::SceneFailed { scene }
}

#[test]
fn a_first_mark_is_shown_while_nothing_changed_since_the_release() {
    let (presenter, _, log, effects) = shown();
    assert_eq!(only_job(effects).scene(), MARKS);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_change_before_the_first_mark_skips_the_scene_and_names_its_run() {
    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    set(&log);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert!(jobs(effects).is_empty());
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 1));
    // The change stays: the run's next screen scene is skipped as well.
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&refused(SECOND)]);
    assert_eq!(scene_ids(&presenter), []);
    // The next release starts over.
    assert!(presenter.screen_released(RUN, now).is_empty());
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (2, 1));
    let effects = marked(&mut presenter, OTHER, SceneId(32), now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SceneId(32)]);
}

#[test]
fn a_first_mark_is_judged_as_it_arrives_while_its_frames_are_held() {
    let (mut presenter, now, log) = watched();
    begin(&mut presenter, LIVE, now);
    presenter.screen_released(AFTER, now);
    let frame = DataToShell::SceneStepCommit {
        run: RUN,
        scene: LIVE,
        index: 0,
        step: rect("a"),
    };
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    assert!(
        arrive(&mut presenter, begin_frame(MARKS), now)
            .expect("held")
            .is_empty()
    );
    set(&log);
    let effects = arrive(&mut presenter, mark_frame(RUN, MARKS), now).expect("held");
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    let effects = presenter.completed(admission.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(credit(&effects), 3 * FRAME);
    assert_eq!(scene_ids(&presenter), [LIVE]);
}

#[test]
fn a_change_clears_a_shown_scene_whose_frames_are_still_held() {
    let (mut presenter, now, log) = watched();
    begin(&mut presenter, LIVE, now);
    presenter.screen_released(AFTER, now);
    let frame = DataToShell::SceneStepCommit {
        run: RUN,
        scene: LIVE,
        index: 0,
        step: rect("a"),
    };
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    assert!(
        arrive(&mut presenter, begin_frame(MARKS), now)
            .expect("held")
            .is_empty()
    );
    assert!(
        arrive(&mut presenter, mark_frame(RUN, MARKS), now)
            .expect("held")
            .is_empty()
    );
    assert_eq!(counts(&log), (1, 0));
    let effects = presenter.screen_changed();
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.marks_skipped(), None);
    let effects = presenter.completed(admission.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [LIVE]);
}

#[test]
fn a_set_bit_clears_the_shown_scene_at_the_next_tick_or_frame() {
    let (mut presenter, now, log, _) = shown();
    set(&log);
    let effects = presenter.tick(now).expect("a tick");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.marks_skipped(), None);
    assert!(presenter.screen_changed().is_empty());

    let (mut presenter, now, log, _) = shown();
    begin(&mut presenter, LIVE, now);
    set(&log);
    let effects = commit_as(&mut presenter, RUN, LIVE, 0, rect("a"), now).expect("a step");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [LIVE]);
    assert_eq!(counts(&log), (1, 1));
    // The run's next screen scene isn't shown: the screen it marks has changed.
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&refused(SECOND)]);
    assert_eq!(presenter.marks_skipped(), None);
}

#[test]
fn one_screen_scene_is_up_at_a_time() {
    let (mut presenter, now, log, _) = shown();
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [SECOND]);
    let effects = marked(&mut presenter, OTHER, SceneId(32), now);
    assert_eq!(controls(&effects), [&refused(SceneId(32))]);
    assert_eq!(scene_ids(&presenter), [SECOND]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn the_marks_go_sixty_seconds_after_their_run_ends() {
    let (mut presenter, now, log, effects) = shown();
    let effects = presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    assert!(controls(&effects).is_empty());
    presenter.run_ended(OTHER, now);
    assert_eq!(presenter.deadline(), None);
    presenter.run_ended(RUN, now);
    presenter.run_ended(RUN, ms(now, 10_000));
    assert_eq!(presenter.deadline(), Some(ms(now, 60_000)));
    assert!(presenter.tick(ms(now, 59_999)).expect("a tick").is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert_eq!(counts(&log), (1, 0));
    let effects = presenter.tick(ms(now, 60_000)).expect("a tick");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.deadline(), None);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_hold_that_ends_without_a_screen_scene_stops_the_watch() {
    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    presenter.screen_run(RUN);
    presenter.run_ended(OTHER, now);
    assert_eq!(counts(&log), (1, 0));
    presenter.run_ended(RUN, now);
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);

    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    presenter.screen_dropped();
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn no_mark_is_shown_without_a_release_or_without_running_sources() {
    let (mut presenter, now, log) = watched();
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (0, 0));

    // No sources installed: what the product does until the shell supplies them.
    let (mut presenter, now) = live();
    presenter.screen_released(AFTER, now);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.marks_skipped(), Some(RUN));

    let (mut presenter, now) = live();
    let log = Rc::<RefCell<Log>>::default();
    presenter.watch_sources(Box::new(Scripted {
        log: Rc::clone(&log),
        starts: false,
    }));
    presenter.screen_released(AFTER, now);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_release_clears_the_marks_of_the_hold_before_it() {
    let (mut presenter, now, log, _) = shown();
    let effects = presenter.screen_released(RUN, now);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (2, 1));
    // A bit set before the release doesn't count against the new hold.
    set(&log);
    presenter.screen_released(RUN, now);
    assert_eq!(counts(&log), (2, 1));
    let effects = marked(&mut presenter, OTHER, SECOND, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SECOND]);
}

#[test]
fn the_setting_going_off_clears_the_marks_and_shows_none_after() {
    let (mut presenter, now, log, _) = shown();
    begin(&mut presenter, LIVE, now);
    let effects = presenter.screen_off();
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [LIVE]);
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&refused(SECOND)]);
    assert_eq!(presenter.marks_skipped(), None);

    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    assert!(presenter.screen_off().is_empty());
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    // A run skipped for a change is forgotten with the setting.
    presenter.screen_released(AFTER, now);
    set(&log);
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&refused(SECOND)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert!(presenter.screen_off().is_empty());
    assert_eq!(presenter.marks_skipped(), None);
}

#[test]
fn any_other_clear_of_the_shown_scene_stops_the_sources() {
    let (mut presenter, now, log, _) = shown();
    let effects = presenter.clear(now).expect("the user's clear");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(counts(&log), (1, 1));

    let (mut presenter, now, log, _) = shown();
    let frame = DataToShell::ScenesRemoved {
        scenes: vec![MARKS],
    };
    let effects = data(&mut presenter, frame, now).expect("the brain's removal");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_lost_session_drops_the_marks_and_ends_the_watch() {
    let (mut presenter, now, log, _) = shown();
    presenter.disconnected();
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);
    presenter.connected(GENERATION);
    assert!(described(&mut presenter, &[], now).is_empty());
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
}

fn screen_snapshot(scene: SceneId) -> DataToShell {
    let events = vec![SceneEvent::Step {
        revision: 1,
        step: std::sync::Arc::new(mark()),
    }];
    DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene,
            revision: 1,
            ordinal: 0,
            header: Some(screen_header()),
            events: Bounded::new(events).expect("one event"),
            last: true,
        },
    }
}

#[test]
fn recovery_forgets_a_screen_scene_and_restores_the_rest() {
    let mut presenter = presenter();
    let now = Instant::now();
    assert!(described(&mut presenter, &[LIVE, MARKS], now).is_empty());
    let effects = data(&mut presenter, snapshot(LIVE, rect("a")), now).expect("a chunk");
    assert!(controls(&effects).is_empty());
    let effects = data(&mut presenter, screen_snapshot(MARKS), now).expect("a chunk");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    let job = recovery(effects);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovery");
    assert_eq!(scene_ids(&presenter), [LIVE]);

    let mut presenter = super::presenter();
    assert!(described(&mut presenter, &[MARKS], now).is_empty());
    let effects = data(&mut presenter, screen_snapshot(MARKS), now).expect("a chunk");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 2);
    assert_eq!(scene_ids(&presenter), []);
    assert!(presenter.membership_ready());
}

#[test]
fn a_replaced_screen_scene_leaves_no_picture_for_the_bridge() {
    let (mut presenter, now, log, effects) = shown();
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).0.visible);
    presenter.frames.borrow_mut().shown = true;
    let frame = DataToShell::ScenesReplaced {
        scenes: vec![MARKS],
        successor: SECOND,
    };
    let effects = data(&mut presenter, frame, now).expect("replacement marker");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert!(!presenter.sample_at(ms(now, 10)).0.visible);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn an_unnamed_release_stops_listening_thirty_seconds_later() {
    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    assert_eq!(presenter.deadline(), Some(ms(now, 30_000)));
    assert!(presenter.tick(ms(now, 29_999)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 0));
    assert!(presenter.tick(ms(now, 30_000)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);
    // The watch stays ended: a name and a scene that come later change nothing.
    presenter.screen_run(RUN);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 30_001));
    assert_eq!(controls(&effects), [&refused(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_named_run_keeps_the_watch_past_thirty_seconds() {
    let (mut presenter, now, log) = watched();
    presenter.screen_released(AFTER, now);
    presenter.screen_run(RUN);
    assert_eq!(presenter.deadline(), None);
    assert!(presenter.tick(ms(now, 30_000)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 0));
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 45_000));
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert_eq!(counts(&log), (1, 0));
}
