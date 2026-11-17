//! Screen scenes through the presenter: what is shown, what is skipped and what clears, with
//! sources the tests script.

use std::{cell::RefCell, rc::Rc, time::Instant};

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, RunId, SceneHeader, SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{
    Frame, Record, SceneEvent, Step, parse_screen_draw_record, record::ScreenDisplay, screen,
    text::Bounded,
};

use super::super::{AdmitJob, Broken};
use super::{
    Effect, FRAME, GENERATION, LIVE, Presenter, RUN, Shaper, arrive, begin, commit_as, controls,
    credit, data, described, jobs, live, ms, only_admission, only_job, presenter, recovery, rect,
    scene_ids, snapshot,
};
use crate::input::TalkHeld;
use crate::platform::screen_watch::{Changed, Sources};

/// The newest run named when the keys came up; [`RUN`] is the hold's.
const AFTER: RunId = RunId(4);
const OTHER: RunId = RunId(6);
const MARKS: SceneId = SceneId(30);
const SECOND: SceneId = SceneId(31);
/// The display the marks' picture was taken of, which the overlay covers.
const DISPLAY: ScreenDisplay = ScreenDisplay {
    origin: [0, 0],
    size: [2560, 1440],
    dpi: 96,
};
/// The display beside it.
const ELSEWHERE: ScreenDisplay = ScreenDisplay {
    origin: [2560, 0],
    size: [2560, 1440],
    dpi: 96,
};

#[derive(Default)]
struct Log {
    starts: u32,
    stops: u32,
    bit: Option<Changed>,
    held: Option<TalkHeld>,
}

struct Scripted {
    log: Rc<RefCell<Log>>,
    starts: bool,
}

impl Sources for Scripted {
    fn start(&mut self, changed: Changed, held: TalkHeld) -> bool {
        let mut log = self.log.borrow_mut();
        log.starts += 1;
        log.bit = Some(changed);
        log.held = Some(held);
        self.starts
    }

    fn stop(&mut self) {
        self.log.borrow_mut().stops += 1;
    }
}

/// A live presenter whose sources start, and their log. Its overlay covers [`DISPLAY`].
fn watched() -> (Presenter, Instant, Rc<RefCell<Log>>) {
    let (mut presenter, now) = live();
    let effects = presenter.set_display(Some(DISPLAY), now);
    assert!(effects.expect("a display").is_empty());
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
    header_of(DISPLAY)
}

/// The header of a screen scene whose picture was taken of `display`.
fn header_of(display: ScreenDisplay) -> SceneHeader {
    let source = br#"{"begin":{"scene":"marks","surface":"screen","alt":""}}"#;
    let Ok(Record::Begin(mut begin)) = parse_screen_draw_record(source) else {
        panic!("a screen begin record");
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

/// A screen scene begins whose picture is of the display beside the overlay's.
fn begin_elsewhere(scene: SceneId) -> DataToShell {
    DataToShell::SceneBegin {
        scene,
        header: header_of(ELSEWHERE),
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
    assert!(release(&mut presenter, AFTER, now).is_empty());
    presenter.screen_run(RUN, now);
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

fn skipped(scene: SceneId) -> ControlToBrain {
    ControlToBrain::SceneSkipped { scene }
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
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    set(&log);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert!(jobs(effects).is_empty());
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 1));
    // The change stays: the run's next screen scene is skipped as well.
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    assert_eq!(scene_ids(&presenter), []);
    // The next release starts over.
    assert!(release(&mut presenter, RUN, now).is_empty());
    presenter.screen_run(OTHER, now);
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
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
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
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
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
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
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
    let effects = presenter.screen_changed(now).expect("a change");
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
    assert!(presenter.screen_changed(now).expect("a change").is_empty());

    let (mut presenter, now, log, _) = shown();
    begin(&mut presenter, LIVE, now);
    set(&log);
    let effects = commit_as(&mut presenter, RUN, LIVE, 0, rect("a"), now).expect("a step");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [LIVE]);
    assert_eq!(counts(&log), (1, 1));
    // The run's next screen scene isn't shown: the screen it marks has changed.
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    assert_eq!(presenter.marks_skipped(), None);
}

#[test]
fn one_screen_scene_is_up_at_a_time() {
    let (mut presenter, now, log, _) = shown();
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [SECOND]);
    let effects = marked(&mut presenter, OTHER, SceneId(32), now);
    assert_eq!(controls(&effects), [&skipped(SceneId(32))]);
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
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    presenter.run_ended(OTHER, now);
    assert_eq!(counts(&log), (1, 0));
    presenter.run_ended(RUN, now);
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);

    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    presenter.screen_dropped();
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn no_mark_is_shown_without_a_release_or_without_running_sources() {
    let (mut presenter, now, log) = watched();
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (0, 0));

    // No sources installed: what the product does until the shell supplies them.
    let (mut presenter, now) = live();
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.marks_skipped(), Some(RUN));

    let (mut presenter, now) = live();
    let log = Rc::<RefCell<Log>>::default();
    presenter.watch_sources(Box::new(Scripted {
        log: Rc::clone(&log),
        starts: false,
    }));
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_release_clears_the_marks_of_the_hold_before_it() {
    let (mut presenter, now, log, _) = shown();
    let effects = release(&mut presenter, RUN, now);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (2, 1));
    // A bit set before the release doesn't count against the new hold.
    set(&log);
    release(&mut presenter, RUN, now);
    presenter.screen_run(OTHER, now);
    assert_eq!(counts(&log), (3, 2));
    let effects = marked(&mut presenter, OTHER, SECOND, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SECOND]);
}

#[test]
fn the_setting_going_off_clears_the_marks_and_shows_none_after() {
    let (mut presenter, now, log, _) = shown();
    begin(&mut presenter, LIVE, now);
    let effects = presenter.screen_off(now).expect("the setting");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(scene_ids(&presenter), [LIVE]);
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    assert_eq!(presenter.marks_skipped(), None);

    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    assert!(presenter.screen_off(now).expect("the setting").is_empty());
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    // A run skipped for a change is forgotten with the setting.
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    set(&log);
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert!(presenter.screen_off(now).expect("the setting").is_empty());
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
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
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
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    presenter.frames.borrow_mut().shown = true;
    let frame = DataToShell::ScenesReplaced {
        scenes: vec![MARKS],
        successor: SECOND,
    };
    let effects = data(&mut presenter, frame, now).expect("replacement marker");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert!(
        !presenter
            .sample_at(ms(now, 10))
            .expect("a sample")
            .0
            .visible
    );
    // The run goes on, so its watch waits for the successor's first mark.
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn an_unnamed_release_stops_listening_thirty_seconds_later() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    assert_eq!(presenter.deadline(), Some(ms(now, 30_000)));
    assert!(presenter.tick(ms(now, 29_999)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 0));
    assert!(presenter.tick(ms(now, 30_000)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);
    // The watch stays ended: a name and a scene that come later change nothing.
    presenter.screen_run(RUN, now);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 30_001));
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_named_run_keeps_the_watch_past_thirty_seconds() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    assert_eq!(presenter.deadline(), None);
    assert!(presenter.tick(ms(now, 30_000)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 0));
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 45_000));
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert_eq!(counts(&log), (1, 0));
}

/// The keys of the next screen hold come up.
fn release(presenter: &mut Presenter, after: RunId, now: Instant) -> Vec<Effect> {
    presenter
        .screen_released(after, TalkHeld::default(), now)
        .expect("a release")
}

fn replaced_by(successor: SceneId) -> DataToShell {
    DataToShell::ScenesReplaced {
        scenes: vec![MARKS],
        successor,
    }
}

/// A second step for a screen scene.
fn second_mark(scene: SceneId) -> DataToShell {
    let source =
        br#"{"step":{"id":"t","put":[{"highlight":{"id":"g","target":{"box":[5,6,7,8]}}}]}}"#;
    let Ok(Record::Step(step)) = parse_screen_draw_record(source) else {
        panic!("a screen step record");
    };
    DataToShell::SceneStepCommit {
        run: RUN,
        scene,
        index: 1,
        step,
    }
}

/// An ordinary scene's step held behind the first mark's own admission, which is still out.
fn admitting() -> (Presenter, Instant, Rc<RefCell<Log>>, AdmitJob) {
    let (mut presenter, now, log) = watched();
    begin(&mut presenter, LIVE, now);
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    let effects = arrive(&mut presenter, begin_frame(MARKS), now).expect("a screen scene");
    assert_eq!(credit(&effects), FRAME);
    let effects = arrive(&mut presenter, mark_frame(RUN, MARKS), now).expect("a first mark");
    assert_eq!(credit(&effects), 0, "the mark's credit is withheld");
    let admission = only_admission(effects);
    let frame = DataToShell::SceneStepCommit {
        run: RUN,
        scene: LIVE,
        index: 0,
        step: rect("a"),
    };
    assert!(arrive(&mut presenter, frame, now).expect("held").is_empty());
    (presenter, now, log, admission)
}

#[test]
fn a_clear_cancels_the_marks_own_admission_and_drops_its_late_answer() {
    type Clear = fn(&mut Presenter, Instant) -> Vec<Effect>;
    let clears: [(Clear, (u32, u32)); 4] = [
        (|p, now| p.screen_changed(now).expect("a change"), (1, 1)),
        (
            |p, now| {
                p.changed.set();
                p.source_woke(now).expect("a wake")
            },
            (1, 1),
        ),
        (|p, now| p.screen_off(now).expect("the setting"), (1, 1)),
        (|p, now| release(p, RUN, now), (2, 1)),
    ];
    for (clear, sources) in clears {
        let (mut presenter, now, log, admission) = admitting();
        let effects = clear(&mut presenter, now);
        assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
        assert_eq!(credit(&effects), FRAME, "the cancelled mark's credit, once");
        assert_eq!(counts(&log), sources);
        // The ordinary step held behind it goes on.
        let live = only_admission(effects);
        assert_eq!(scene_ids(&presenter), [LIVE]);
        // The cleared mark's answer comes back in time: it is dropped, not a broken stream.
        let late = presenter
            .completed(admission.run(), now)
            .expect("a late answer");
        assert!(late.is_empty());
        let effects = presenter.completed(live.run(), now).expect("admitted");
        assert!(controls(&effects).is_empty());
        assert_eq!(credit(&effects), FRAME);
        assert_eq!(only_job(effects).scene(), LIVE);
        assert_eq!(scene_ids(&presenter), [LIVE]);
    }
}

#[test]
fn a_clear_drops_the_marks_admission_that_still_waits_for_a_worker_slot() {
    let (mut presenter, now, mut zombies) = super::exhausted();
    presenter.watch_sources(Box::new(Scripted {
        log: Rc::default(),
        starts: true,
    }));
    presenter
        .set_display(Some(DISPLAY), now)
        .expect("a display");
    let marks = SceneId(40);
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    arrive(&mut presenter, begin_frame(marks), now).expect("a screen scene");
    let effects = arrive(&mut presenter, mark_frame(RUN, marks), now).expect("a first mark");
    assert!(effects.is_empty(), "no slot and no credit yet");
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let effects = presenter.screen_changed(now).expect("a change");
    assert_eq!(controls(&effects), [&cleared(&[marks])]);
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(presenter.deadline(), None);
    // A slot frees: no lane is left to send for a scene that is gone.
    let zombie = zombies.pop().expect("a held slot");
    let effects = presenter
        .completed(zombie.run(&mut Shaper), now)
        .expect("a stale answer");
    assert!(effects.is_empty());
    assert!(!presenter.holds_scenes());
}

#[test]
fn the_brain_s_replacement_passes_the_watch_to_the_run_s_next_screen_scene() {
    let (mut presenter, now, log, _) = shown();
    let effects = data(&mut presenter, replaced_by(SECOND), now).expect("the marker");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (1, 0), "the run's watch goes on");
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(only_job(effects).scene(), SECOND);
    assert_eq!(scene_ids(&presenter), [SECOND]);
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert_eq!(counts(&log), (1, 0));
    // The successor is watched as the first scene was, with the same bit.
    set(&log);
    let effects = presenter.source_woke(now).expect("a wake");
    assert_eq!(controls(&effects), [&cleared(&[SECOND])]);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_replacement_held_behind_the_marks_own_admission_still_passes_the_watch() {
    let (mut presenter, now, log, _) = shown();
    let admission =
        only_admission(arrive(&mut presenter, second_mark(MARKS), now).expect("a second step"));
    for held in [replaced_by(SECOND), begin_frame(SECOND)] {
        assert!(arrive(&mut presenter, held, now).expect("held").is_empty());
    }
    let effects = arrive(&mut presenter, mark_frame(RUN, SECOND), now).expect("a first mark");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(
        credit(&effects),
        3 * FRAME,
        "the cancelled step, the marker and the begin"
    );
    let successor = only_admission(effects);
    assert_eq!(scene_ids(&presenter), [SECOND]);
    assert_eq!(counts(&log), (1, 0));
    let late = presenter
        .completed(admission.run(), now)
        .expect("a late answer");
    assert!(late.is_empty());
    let effects = presenter.completed(successor.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(only_job(effects).scene(), SECOND);
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_handoff_that_fails_or_never_comes_ends_the_watch() {
    /// The shown scene replaced, its successor not yet begun.
    fn passing() -> (Presenter, Instant, Rc<RefCell<Log>>) {
        let (mut presenter, now, log, _) = shown();
        data(&mut presenter, replaced_by(SECOND), now).expect("the marker");
        assert_eq!(counts(&log), (1, 0));
        (presenter, now, log)
    }
    // The successor's first mark is another run's.
    let (mut presenter, now, log) = passing();
    let effects = marked(&mut presenter, OTHER, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    assert_eq!(counts(&log), (1, 1));
    // The successor is no screen scene.
    let (mut presenter, now, log) = passing();
    begin(&mut presenter, SECOND, now);
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(scene_ids(&presenter), [SECOND]);
    // The run ends with no successor shown; a late one is skipped.
    let (mut presenter, now, log) = passing();
    presenter.run_ended(OTHER, now);
    assert_eq!(counts(&log), (1, 0));
    presenter.run_ended(RUN, now);
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
    // The user clears before the successor begins.
    let (mut presenter, now, log) = passing();
    let effects = presenter.clear(now).expect("the user's clear");
    assert_eq!(controls(&effects), [&cleared(&[SECOND])]);
    assert_eq!(counts(&log), (1, 1));
    // Another screen scene of the run is not the declared successor.
    let (mut presenter, now, log) = passing();
    let effects = marked(&mut presenter, RUN, SceneId(32), now);
    assert_eq!(controls(&effects), [&skipped(SceneId(32))]);
    assert_eq!(counts(&log), (1, 0));
    // A scene whose run has ended passes nothing on.
    let (mut presenter, now, log, _) = shown();
    presenter.run_ended(RUN, now);
    data(&mut presenter, replaced_by(SECOND), now).expect("the marker");
    assert_eq!(counts(&log), (1, 1));
    let effects = marked(&mut presenter, RUN, SECOND, now);
    assert_eq!(controls(&effects), [&skipped(SECOND)]);
}

#[test]
fn a_first_mark_or_a_name_at_the_unnamed_limit_is_late_before_the_watchdog_runs() {
    // Before the limit an unnamed release skips a first mark and goes on listening.
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 29_999));
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 0));

    // No tick comes first: the frame itself finds the limit passed.
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 30_000));
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(presenter.marks_skipped(), None);
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);

    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, ms(now, 30_000));
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 30_000));
    assert_eq!(controls(&effects), [&skipped(MARKS)]);

    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, ms(now, 29_999));
    assert_eq!(presenter.deadline(), None);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 45_000));
    assert!(controls(&effects).is_empty());
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_first_mark_before_the_run_is_named_is_skipped_and_the_named_run_s_still_shows() {
    // An earlier hold's scene, queued before either hold's first state, comes under the new
    // release: its run is newer than the release and is not the hold's.
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(
        counts(&log),
        (1, 0),
        "the watch still expects the hold's own"
    );
    presenter.screen_run(OTHER, now);
    let effects = marked(&mut presenter, OTHER, SECOND, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SECOND]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_frame_at_the_time_limit_clears_the_marks_before_the_watchdog_runs() {
    let (mut presenter, now, log, _) = shown();
    presenter.run_ended(RUN, now);
    let effects = data(&mut presenter, begin_frame(SECOND), ms(now, 59_999)).expect("a begin");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS, SECOND]);
    // The run's next first mark comes at the limit: the shown marks go and it isn't shown.
    let effects =
        commit_as(&mut presenter, RUN, SECOND, 0, mark(), ms(now, 60_000)).expect("a first mark");
    assert_eq!(controls(&effects), [&cleared(&[MARKS, SECOND])]);
    assert_eq!(scene_ids(&presenter), []);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_run_s_end_before_any_name_leaves_the_unnamed_watch_to_its_own_limit() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    for run in [AFTER, RUN] {
        presenter.run_ended(run, ms(now, 10_000));
        assert_eq!(counts(&log), (1, 0));
        assert_eq!(presenter.deadline(), Some(ms(now, 30_000)));
    }
    // With no name the release's own limit ends the watch, not one counted from that end.
    assert!(presenter.tick(ms(now, 29_999)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 0));
    assert!(presenter.tick(ms(now, 30_000)).expect("a tick").is_empty());
    assert_eq!(counts(&log), (1, 1));
    assert_eq!(presenter.deadline(), None);
    // A late name brings nothing back.
    presenter.screen_run(RUN, ms(now, 30_000));
    assert_eq!(presenter.deadline(), None);
    let effects = marked(&mut presenter, RUN, MARKS, ms(now, 30_000));
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(counts(&log), (1, 1));
}

#[test]
fn a_wake_queued_before_a_release_changes_nothing_after_it() {
    let (mut presenter, now, log, _) = shown();
    // A source sets the bit and queues its wake; the next hold's release comes first.
    set(&log);
    let effects = release(&mut presenter, RUN, now);
    presenter.screen_run(OTHER, now);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert!(presenter.source_woke(now).expect("the old wake").is_empty());
    let effects = marked(&mut presenter, OTHER, SECOND, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SECOND]);

    // A frame consumed the bit before its wake was handled; then the next hold was released.
    set(&log);
    let effects = presenter.tick(now).expect("a tick");
    assert_eq!(controls(&effects), [&cleared(&[SECOND])]);
    release(&mut presenter, OTHER, now);
    presenter.screen_run(RunId(7), now);
    assert!(presenter.source_woke(now).expect("the old wake").is_empty());
    let effects = marked(&mut presenter, RunId(7), SceneId(32), now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SceneId(32)]);

    // The shell's own event is the one that sets the bit.
    let effects = presenter.screen_changed(now).expect("the next hold");
    assert_eq!(controls(&effects), [&cleared(&[SceneId(32)])]);
}

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What `road` logs.
fn logged(road: impl FnOnce()) -> String {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let logging = tracing::subscriber::set_default(subscriber);
    road();
    drop(logging);
    String::from_utf8(captured.0.lock().expect("the log").clone()).expect("text")
}

/// The reason words of the "screen marks cleared" lines `road` logs after its marks are shown.
fn reasons(road: impl FnOnce(&mut Presenter, Instant)) -> Vec<String> {
    let (mut presenter, now, _, _) = shown();
    let log = logged(|| road(&mut presenter, now));
    log.lines()
        .filter_map(|line| line.split_once("screen marks cleared reason="))
        .map(|(_, reason)| reason.trim().trim_matches('"').to_owned())
        .collect()
}

#[test]
fn every_road_that_takes_shown_marks_down_logs_one_fixed_word() {
    type Road = fn(&mut Presenter, Instant);
    let roads: [(Road, &str); 11] = [
        (|p, now| drop(p.screen_changed(now)), "changed"),
        (|p, now| drop(p.set_display(None, now)), "display"),
        (|p, now| drop(p.screen_off(now)), "off"),
        (|p, now| drop(release(p, RUN, now)), "release"),
        (
            |p, now| {
                p.run_ended(RUN, now);
                drop(p.tick(ms(now, 60_000)));
            },
            "limit",
        ),
        (|p, now| drop(marked(p, RUN, SECOND, now)), "left"),
        (|p, now| drop(data(p, replaced_by(SECOND), now)), "left"),
        (
            |p, now| {
                p.run_ended(RUN, now);
                drop(data(p, replaced_by(SECOND), now));
            },
            "left",
        ),
        (|p, now| drop(p.clear(now)), "left"),
        (
            |p, now| {
                let scenes = vec![MARKS];
                drop(data(p, DataToShell::ScenesRemoved { scenes }, now));
            },
            "left",
        ),
        (|p, _| p.disconnected(), "left"),
    ];
    for (at, (road, word)) in roads.into_iter().enumerate() {
        assert_eq!(reasons(road), [word], "road {at}");
    }
    // Marks that were never shown leave no line.
    let (mut presenter, now, _) = watched();
    release(&mut presenter, AFTER, now);
    data(&mut presenter, begin_frame(MARKS), now).expect("a screen scene");
    presenter.disconnected();
}

#[test]
fn set_says_whether_it_changed_the_bit() {
    let bit = Changed::default();
    assert!(bit.set());
    assert!(
        !bit.set(),
        "a second event of the same change wakes nothing"
    );
    assert!(bit.take());
    assert!(!bit.take());
    assert!(bit.set());
}

#[test]
fn every_release_starts_the_sources_again_on_a_bit_of_its_own() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    let old = log.borrow().bit.clone().expect("started sources");
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    assert_eq!(counts(&log), (2, 1));
    // A handler of the watch before is still running: the bit it sets is nobody's.
    assert!(old.set());
    assert!(presenter.source_woke(now).expect("its wake").is_empty());
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert_eq!(presenter.marks_skipped(), None);
    // This watch's own bit still counts.
    set(&log);
    let effects = presenter.source_woke(now).expect("a wake");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(counts(&log), (2, 2));
}

#[test]
fn each_release_hands_the_sources_the_talk_keys_still_down() {
    let (mut presenter, now, log) = watched();
    for (after, held) in [
        (
            AFTER,
            TalkHeld {
                control: true,
                alt: false,
            },
        ),
        (
            RUN,
            TalkHeld {
                control: false,
                alt: true,
            },
        ),
        (OTHER, TalkHeld::default()),
    ] {
        presenter
            .screen_released(after, held, now)
            .expect("a release");
        assert_eq!(log.borrow().held, Some(held));
    }
    assert_eq!(counts(&log), (3, 2));
}

/// A watch with its first mark shown and that mark prepared, ready to be activated.
fn prepared() -> (Presenter, Instant, Rc<RefCell<Log>>) {
    let (mut presenter, now, log, effects) = shown();
    let effects = presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    assert!(effects.is_empty());
    (presenter, now, log)
}

/// The frame after a clear: nothing is activated and nothing is visible.
fn nothing_shows(presenter: &mut Presenter, now: Instant) {
    assert!(
        presenter
            .advance(GENERATION, now)
            .expect("a frame")
            .is_empty()
    );
    let (sample, effects) = presenter.sample_at(now).expect("a sample");
    assert!(!sample.visible);
    assert!(effects.is_empty());
    assert_eq!(scene_ids(presenter), []);
}

#[test]
fn a_due_activation_finds_the_marks_cleared_by_a_pending_wake_or_a_due_limit() {
    // Unchanged and inside the limit, the prepared mark is activated and seen.
    let (mut presenter, now, log) = prepared();
    presenter.run_ended(RUN, now);
    let at = ms(now, 59_999);
    assert!(
        presenter
            .advance(GENERATION, at)
            .expect("a frame")
            .is_empty()
    );
    assert!(presenter.sample_at(at).expect("a sample").0.visible);
    assert_eq!(counts(&log), (1, 0));

    // A source set the bit and its wake is still queued behind the frame.
    let (mut presenter, now, log) = prepared();
    set(&log);
    let effects = presenter.advance(GENERATION, now).expect("a frame");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, now);
    assert!(presenter.source_woke(now).expect("the wake").is_empty());

    // The limit is due and the watchdog hasn't run.
    let (mut presenter, now, log) = prepared();
    presenter.run_ended(RUN, now);
    let at = ms(now, 60_000);
    let effects = presenter.advance(GENERATION, at).expect("a frame");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, at);
    assert!(presenter.tick(at).expect("the watchdog").is_empty());
}

#[test]
fn a_frame_is_not_sampled_with_marks_a_pending_wake_or_a_due_limit_takes() {
    let (mut presenter, now, log) = prepared();
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    set(&log);
    let (sample, effects) = presenter.sample_at(now).expect("a sample");
    assert!(!sample.visible);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, now);

    let (mut presenter, now, log) = prepared();
    presenter.advance(GENERATION, now).expect("activation");
    presenter.run_ended(RUN, now);
    assert!(
        presenter
            .sample_at(ms(now, 59_999))
            .expect("a sample")
            .0
            .visible
    );
    let at = ms(now, 60_000);
    let (sample, effects) = presenter.sample_at(at).expect("a sample");
    assert!(!sample.visible);
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, at);
}

#[test]
fn a_queued_answer_cannot_publish_marks_a_pending_wake_or_a_due_limit_takes() {
    // The mark's preparation is out when a source sets the bit; its answer comes first.
    let (mut presenter, now, log, effects) = shown();
    let job = only_job(effects);
    set(&log);
    let effects = presenter
        .completed(job.run(&mut Shaper), now)
        .expect("a stale answer");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, now);
    assert!(presenter.source_woke(now).expect("the wake").is_empty());

    // The same answer at the due limit, before the watchdog.
    let (mut presenter, now, log, effects) = shown();
    let job = only_job(effects);
    presenter.run_ended(RUN, now);
    let at = ms(now, 60_000);
    let effects = presenter
        .completed(job.run(&mut Shaper), at)
        .expect("a stale answer");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(effects.len(), 1);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, at);

    // The marks' own admission is out, with an ordinary step held behind it.
    let (mut presenter, now, log, admission) = admitting();
    set(&log);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("a stale answer");
    assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
    assert_eq!(credit(&effects), FRAME, "the cancelled mark's credit, once");
    assert_eq!(counts(&log), (1, 1));
    // The held step goes on, and the marks' answer admitted nothing.
    let live = only_admission(effects);
    assert_eq!(scene_ids(&presenter), [LIVE]);
    let effects = presenter.completed(live.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(only_job(effects).scene(), LIVE);
}

#[test]
fn a_newer_run_that_is_not_the_hold_s_ending_before_the_name_leaves_the_watch_to_the_hold() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    // Another run, newer than the release, marks and ends before the hold's run is named.
    let effects = marked(&mut presenter, RUN, MARKS, now);
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    let later = ms(now, 10_000);
    presenter.run_ended(RUN, later);
    assert_eq!(counts(&log), (1, 0), "the sources listen on, started once");
    assert_eq!(presenter.deadline(), Some(ms(now, 30_000)));
    nothing_shows(&mut presenter, later);
    // The hold's run is named before the release's limit, and its mark shows.
    presenter.screen_run(OTHER, later);
    let effects = marked(&mut presenter, OTHER, SECOND, later);
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [SECOND]);
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_begun_screen_scene_with_no_mark_shows_nothing() {
    let (mut presenter, now, _) = watched();
    release(&mut presenter, AFTER, now);
    let effects = data(&mut presenter, begin_frame(MARKS), now).expect("a screen scene");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert!(
        presenter
            .advance(GENERATION, now)
            .expect("a frame")
            .is_empty()
    );
    let (sample, effects) = presenter.sample_at(ms(now, 500)).expect("a sample");
    assert!(!sample.visible);
    assert!(effects.is_empty());
}

#[test]
fn a_sample_that_clears_the_marks_reports_a_held_frame_the_mirror_rejects() {
    let (mut presenter, now, log) = watched();
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    arrive(&mut presenter, begin_frame(MARKS), now).expect("a screen scene");
    let effects = arrive(&mut presenter, mark_frame(RUN, MARKS), now).expect("a first mark");
    let _admission = only_admission(effects);
    // A step of a scene that never began waits behind the marks' admission.
    let orphan = DataToShell::SceneStepCommit {
        run: RUN,
        scene: SECOND,
        index: 0,
        step: rect("a"),
    };
    assert!(
        arrive(&mut presenter, orphan, now)
            .expect("held")
            .is_empty()
    );
    set(&log);
    assert_eq!(presenter.sample_at(now).err(), Some(Broken::Order));
}

#[test]
fn a_first_mark_whose_view_cannot_be_placed_is_skipped_once_and_shows_nothing() {
    for display in [None, Some(ELSEWHERE)] {
        let (mut presenter, now, log) = watched();
        assert!(
            presenter
                .set_display(display, now)
                .expect("a display")
                .is_empty()
        );
        release(&mut presenter, AFTER, now);
        let mut effects = Vec::new();
        let lines = logged(|| effects = marked(&mut presenter, RUN, MARKS, now));
        assert_eq!(controls(&effects), [&skipped(MARKS)]);
        assert_eq!(
            lines
                .matches("screen marks skipped reason=\"display\"")
                .count(),
            1
        );
        assert_eq!(lines.matches("screen marks").count(), 1);
        assert_eq!(presenter.marks_skipped(), Some(RUN));
        assert_eq!(counts(&log), (1, 1));
        nothing_shows(&mut presenter, now);
        // The display coming back undoes nothing: the run's next screen scene isn't shown.
        assert!(
            presenter
                .set_display(Some(DISPLAY), now)
                .expect("a display")
                .is_empty()
        );
        let effects = marked(&mut presenter, RUN, SECOND, now);
        assert_eq!(controls(&effects), [&skipped(SECOND)]);
    }

    // The same while the scene's begin is still held behind another scene's admission.
    let (mut presenter, now, log) = watched();
    presenter
        .set_display(Some(ELSEWHERE), now)
        .expect("a display");
    begin(&mut presenter, LIVE, now);
    release(&mut presenter, AFTER, now);
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
    let effects = arrive(&mut presenter, mark_frame(RUN, MARKS), now).expect("held");
    assert_eq!(controls(&effects), [&skipped(MARKS)]);
    assert_eq!(presenter.marks_skipped(), Some(RUN));
    assert_eq!(counts(&log), (1, 1));
    let effects = presenter.completed(admission.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [LIVE]);
}

#[test]
fn an_unplaceable_first_mark_is_skipped_while_the_marks_that_were_up_are_cleared() {
    // The older scene is up; the next one's picture is of the display beside the overlay's.
    let (mut presenter, now, log, _) = shown();
    let effects = data(&mut presenter, begin_elsewhere(SECOND), now).expect("a screen scene");
    assert!(controls(&effects).is_empty());
    let mut effects = Vec::new();
    let lines = logged(|| {
        effects = commit_as(&mut presenter, RUN, SECOND, 0, mark(), now).expect("a first mark");
    });
    // The brain hears a skip for the scene that never showed, and a clear only for the other.
    assert_eq!(controls(&effects), [&skipped(SECOND), &cleared(&[MARKS])]);
    assert_eq!(
        lines
            .matches("screen marks skipped reason=\"display\"")
            .count(),
        1
    );
    assert_eq!(
        lines
            .matches("screen marks cleared reason=\"display\"")
            .count(),
        1
    );
    assert_eq!(lines.matches("screen marks").count(), 2);
    assert_eq!(counts(&log), (1, 1));
    nothing_shows(&mut presenter, now);

    // The same while the scene's begin is still held behind another scene's admission.
    let (mut presenter, now, log, _) = shown();
    begin(&mut presenter, LIVE, now);
    let frame = DataToShell::SceneStepCommit {
        run: RUN,
        scene: LIVE,
        index: 0,
        step: rect("a"),
    };
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    assert!(
        arrive(&mut presenter, begin_elsewhere(SECOND), now)
            .expect("held")
            .is_empty()
    );
    let effects = arrive(&mut presenter, mark_frame(RUN, SECOND), now).expect("held");
    assert_eq!(controls(&effects), [&skipped(SECOND), &cleared(&[MARKS])]);
    assert_eq!(counts(&log), (1, 1));
    let effects = presenter.completed(admission.run(), now).expect("admitted");
    assert!(controls(&effects).is_empty());
    assert_eq!(scene_ids(&presenter), [LIVE]);
}

#[test]
fn a_shown_scene_whose_display_changes_is_cleared_under_the_word_display() {
    for display in [None, Some(ELSEWHERE)] {
        let (mut presenter, now, log, _) = shown();
        let mut effects = Vec::new();
        let lines = logged(|| effects = presenter.set_display(display, now).expect("a display"));
        assert_eq!(controls(&effects), [&cleared(&[MARKS])]);
        assert_eq!(effects.len(), 1);
        assert_eq!(
            lines
                .matches("screen marks cleared reason=\"display\"")
                .count(),
            1
        );
        assert_eq!(lines.matches("screen marks").count(), 1);
        assert_eq!(counts(&log), (1, 1));
        assert!(
            presenter
                .set_display(display, now)
                .expect("the same display")
                .is_empty()
        );
        nothing_shows(&mut presenter, now);
    }

    // The display the marks were placed on, given again, changes nothing.
    let (mut presenter, now, log, _) = shown();
    assert!(
        presenter
            .set_display(Some(DISPLAY), now)
            .expect("the same display")
            .is_empty()
    );
    assert_eq!(scene_ids(&presenter), [MARKS]);
    assert_eq!(counts(&log), (1, 0));
}

#[test]
fn a_display_known_after_the_begin_places_the_scene_before_its_first_mark() {
    let (mut presenter, now, log) = watched();
    presenter.set_display(None, now).expect("no display");
    release(&mut presenter, AFTER, now);
    presenter.screen_run(RUN, now);
    data(&mut presenter, begin_frame(MARKS), now).expect("a screen scene");
    assert!(
        presenter
            .set_display(Some(DISPLAY), now)
            .expect("a display")
            .is_empty()
    );
    let effects = commit_as(&mut presenter, RUN, MARKS, 0, mark(), now).expect("a first mark");
    assert!(controls(&effects).is_empty());
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    let placed = scales(&presenter, MARKS);
    assert!(!placed.is_empty());
    assert_eq!(placed, vec![TWICE; placed.len()]);
    assert_eq!(counts(&log), (1, 0));
}

/// The scale each painted mark of `scene` is placed at, as bits.
fn scales(presenter: &Presenter, scene: SceneId) -> Vec<[u32; 2]> {
    let mut frame = Frame::new();
    presenter.scenes[&scene]
        .view
        .frame(60_000, &mut frame)
        .expect("frame");
    frame
        .items()
        .iter()
        .map(|item| [item.transform.0[0].to_bits(), item.transform.0[3].to_bits()])
        .collect()
}

/// 1,280 picture pixels across a 2,560-pixel display at 100%: two logical pixels each.
const TWICE: [u32; 2] = [2.0_f32.to_bits(); 2];

#[test]
fn shown_marks_are_placed_on_the_overlay_s_display() {
    let (mut presenter, now, _) = prepared();
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    let placed = scales(&presenter, MARKS);
    assert!(!placed.is_empty());
    assert_eq!(placed, vec![TWICE; placed.len()]);
}

#[test]
fn the_frame_checks_allocate_nothing_while_nothing_changed() {
    let (mut presenter, now, _) = prepared();
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    assert!(presenter.listening);
    let at = ms(now, 7);
    let (shown, [allocations, _], _) = crate::allocation::usage(|| {
        let effects = presenter.advance(GENERATION, at).expect("a frame");
        let (sample, sampled) = presenter.sample_at(at).expect("a sample");
        effects.is_empty() && sampled.is_empty() && sample.visible
    });
    assert!(shown);
    assert_eq!(allocations, 0);
}
