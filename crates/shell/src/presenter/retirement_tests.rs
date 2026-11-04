//! Each presenter retirement site keeps what it discards off the calling thread. Where an Arc
//! can see the discarded value, its count must not fall while the dropper is held; where only a
//! container or owned ids remain, the caller's frees are measured against a plain-drop control.

use std::collections::VecDeque;

use kumochi_scene::{Admission, GlobalBudget, SceneState, layout::LayoutState};

use super::*;

/// Live frames held before recovery, enough that their queue is the largest buffer in play.
const HELD: usize = 256;
/// Rectangles in one step, small enough for one record with the longest ids.
const OBJECTS: usize = 64;
/// Extra id bytes per object, keeping the longest id within the 40-character cap.
const PAD: usize = 37;

/// Waits until the dropper has destroyed everything queued before this call.
fn settled() {
    let marker = Arc::new(());
    drop(Deferred::new(Arc::clone(&marker), 0));
    assert!(drained(&marker, 1), "the dropper reaches the marker");
}

/// One step of rectangles whose ids end in the padding.
fn rects(pad: &str) -> Step {
    let puts: Vec<_> = (0..OBJECTS)
        .map(|i| format!(r#"{{"shape":{{"id":"s{i}{pad}","kind":"rect","box":[0,0,10,10]}}}}"#))
        .collect();
    step(&format!(
        r#"{{"step":{{"id":"many","put":[{}]}}}}"#,
        puts.join(",")
    ))
}

/// A one-step snapshot stored under a layout version this shell does not prepare.
fn other_layout(scene: SceneId) -> DataToShell {
    let mut frame = snapshot(scene, rect("r"));
    let DataToShell::SceneSnapshotV1 { chunk } = &mut frame else {
        panic!("a snapshot frame");
    };
    let stored = chunk.header.as_mut().expect("a first chunk");
    stored.layout_version = super::super::LAYOUT_VERSION + 1;
    frame
}

/// Holds live frames for the scene and returns the size of their queue's buffer, after checking
/// that a plainly dropped queue of that capacity frees exactly that on the caller.
fn held(presenter: &mut Presenter, scene: SceneId, now: Instant) -> usize {
    for _ in 0..HELD {
        let frame = DataToShell::SceneBegin {
            scene,
            header: header(),
        };
        let effects = presenter
            .data(GENERATION, Deferred::new(frame, 0), 0, now)
            .expect("a held frame");
        assert!(effects.is_empty(), "a held frame earns nothing yet");
    }
    assert_eq!(presenter.held.len(), HELD);
    let capacity = presenter.held.capacity();
    let bytes = capacity * size_of::<(Deferred<DataToShell>, u32)>();
    let control: VecDeque<(Deferred<DataToShell>, u32)> = VecDeque::with_capacity(capacity);
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(control));
    assert_eq!(
        released, bytes,
        "calibration: a plainly dropped queue frees its buffer on the caller"
    );
    bytes
}

#[test]
fn disconnecting_retires_the_held_frame_queue_off_the_caller() {
    let mut presenter = presenter();
    let bytes = held(&mut presenter, LIVE, Instant::now());
    let release = hold_dropper();
    crate::allocation::usage(|| presenter.disconnected());
    let largest = crate::allocation::largest_release();
    assert!(
        presenter.held.is_empty(),
        "disconnecting discards every held frame"
    );
    assert!(
        largest < bytes,
        "the held queue's {bytes}-byte buffer waits for the dropper, largest caller free {largest}"
    );
    drop(presenter);
    drop(release);
}

#[test]
fn going_live_retires_the_held_frame_queue_off_the_caller() {
    let scene = SceneId(10);
    let mut presenter = presenter();
    let now = Instant::now();
    described(&mut presenter, &[scene], now);
    let bytes = held(&mut presenter, scene, now);
    let release = hold_dropper();
    let (effects, _, _) =
        crate::allocation::usage(|| data(&mut presenter, other_layout(scene), now));
    let largest = crate::allocation::largest_release();
    let effects = effects.expect("an unpreparable batch goes live at once");
    assert_eq!(controls(&effects), [&clear_drawing(vec![scene])]);
    assert!(
        presenter.held.is_empty(),
        "going live applies every held frame"
    );
    assert!(
        largest < bytes,
        "the held queue's {bytes}-byte buffer waits for the dropper, largest caller free {largest}"
    );
    drop(presenter);
    drop(release);
}

#[test]
fn a_source_for_another_layout_version_drops_on_the_dropper() {
    let scene = SceneId(10);
    let mut presenter = presenter();
    let now = Instant::now();
    described(&mut presenter, &[scene], now);
    let frame = other_layout(scene);
    let probe = first_step(&frame);
    let owners = Arc::strong_count(&probe);
    let release = hold_dropper();
    let effects = data(&mut presenter, frame, now).expect("an unpreparable batch");
    assert_eq!(controls(&effects), [&clear_drawing(vec![scene])]);
    assert_eq!(
        Arc::strong_count(&probe),
        owners + 1,
        "the discarded chunk and the partitioned source's log both wait for the dropper"
    );
    drop(release);
    assert!(drained(&probe, 1), "the dropper destroys both");
}

#[test]
fn a_recovered_scene_removed_before_its_answer_drops_on_the_dropper() {
    let scene = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(scene, rect("r"))]);
    let done = job.run(&mut Shaper);
    let Completion::Recovered { scenes, .. } = &done else {
        panic!("a recovery result");
    };
    let (state, _) = scenes[0].1.as_ref().expect("recovered scene");
    let SceneEvent::Step { step, .. } = &state.recovery_log()[0] else {
        panic!("the recovered original step");
    };
    let probe = Arc::clone(step);
    settled();
    let owners = Arc::strong_count(&probe);
    let release = hold_dropper();
    let removal = DataToShell::ScenesRemoved {
        scenes: vec![scene],
    };
    data(&mut presenter, removal, now).expect("a removal during recovery");
    let effects = presenter
        .completed(done, ms(now, 20))
        .expect("the current batch");
    assert!(
        controls(&effects).is_empty(),
        "a removed scene is not cleared again"
    );
    assert!(
        !presenter.scenes.contains_key(&scene),
        "a removed scene never installs"
    );
    assert_eq!(
        Arc::strong_count(&probe),
        owners,
        "the rejected recovered state waits for the dropper"
    );
    drop(release);
    assert!(drained(&probe, 1), "the dropper destroys it");
}

#[test]
fn a_removed_scene_drops_on_the_dropper() {
    let scene = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(scene, rect("r"))]);
    presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let SceneEvent::Step { step, .. } = &presenter.scenes[&scene].state.recovery_log()[0] else {
        panic!("the installed original step");
    };
    let probe = Arc::clone(step);
    settled();
    let owners = Arc::strong_count(&probe);
    let release = hold_dropper();
    let removal = DataToShell::ScenesRemoved {
        scenes: vec![scene],
    };
    data(&mut presenter, removal, ms(now, 30)).expect("a removal");
    assert!(
        !presenter.scenes.contains_key(&scene),
        "the removed scene is released"
    );
    assert_eq!(
        Arc::strong_count(&probe),
        owners,
        "the released scene waits for the dropper"
    );
    drop(release);
    assert!(
        drained(&probe, owners - 1),
        "the dropper destroys the scene and its log"
    );
}

/// What the caller frees while a recovered scene of rectangles applies its kept cut, then while
/// its kept rest is dropped in place as a control.
fn kept_cut_released(pad: &str) -> (usize, usize) {
    let scene = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(scene, rects(pad))]);
    presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let stopped = presenter.stop(RUN, ms(now, 30)).expect("a stop");
    assert_eq!(controls(&stopped).len(), 1, "the recovered scene is cut");
    let cut = presenter.scenes[&scene].cut_now();
    let revision = presenter.scenes[&scene].state.revision();
    let release = hold_dropper();
    let (applied, _, [_, applying]) =
        crate::allocation::usage(|| acknowledge(&mut presenter, scene, revision, cut, ms(now, 40)));
    applied.expect("the kept cut");
    let kept = presenter
        .scenes
        .get_mut(&scene)
        .expect("a kept cut keeps its scene");
    let empty = LayoutState::new(&kept.state);
    let rest = std::mem::replace(&mut kept.rest, empty);
    let ((), _, [_, plain]) = crate::allocation::usage(|| drop(rest));
    drop(presenter);
    drop(release);
    (applying, plain)
}

#[test]
fn a_kept_cut_replaces_its_rest_off_the_caller() {
    let pad = "x".repeat(PAD);
    let (short, short_plain) = kept_cut_released("");
    let (long, long_plain) = kept_cut_released(&pad);
    assert!(
        long_plain - short_plain >= OBJECTS * PAD,
        "calibration: a rest dropped in place frees every object's id on the caller"
    );
    assert!(
        long.saturating_sub(short) < OBJECTS * PAD,
        "the replaced rest's ids wait for the dropper: {short} then {long} bytes freed"
    );
}

/// What the caller frees committing one step of rectangles to a live scene.
fn commit_released(pad: &str) -> usize {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let many = rects(pad);
    let release = hold_dropper();
    let (effects, _, [_, released]) =
        crate::allocation::usage(|| commit(&mut presenter, LIVE, 0, many, now));
    let _job = only_job(effects.expect("a matching step"));
    drop(presenter);
    drop(release);
    released
}

/// What dropping a commit's created handles in place frees on the caller.
fn plain_handles_released(pad: &str) -> usize {
    let Ok(mut state) = SceneState::begin(header().begin) else {
        panic!("a valid header");
    };
    let mut global = GlobalBudget::default();
    let Ok(Admission::New(admitted)) = state.admit(&rects(pad), &global) else {
        panic!("an admitted step");
    };
    let Ok(committed) = state.commit(admitted, &mut global) else {
        panic!("a current admission");
    };
    let handles = committed.handles;
    let ((), _, [_, released]) = crate::allocation::usage(|| drop(handles));
    released
}

#[test]
fn a_commit_hands_its_created_handles_to_the_dropper() {
    let pad = "x".repeat(PAD);
    let short = commit_released("");
    let long = commit_released(&pad);
    assert_eq!(
        plain_handles_released(&pad) - plain_handles_released(""),
        OBJECTS * PAD,
        "calibration: handles dropped in place free every id on the caller"
    );
    assert!(
        long.saturating_sub(short) < OBJECTS * PAD,
        "the created handles wait for the dropper: {short} then {long} bytes freed"
    );
}
