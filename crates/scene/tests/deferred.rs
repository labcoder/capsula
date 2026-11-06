//! The process-wide dropper: discarded values leave the discarding thread and stay metered.
//! One test owns this binary's global dropper, so its counts are exact.

#[path = "support/allocation.rs"]
mod allocation;
#[path = "support/scene.rs"]
mod scene_support;

use kumochi_scene::{
    Admission, ErrorCode, Frame, GlobalBudget, Record, SceneState, SceneView, Step, StepError,
    StopCut, StopOutcome,
    deferred::{self, Deferred, Pending},
    layout::{LayoutState, PreparedStep},
    parse_record,
    text::Bounded,
};
use scene_support::{id, new_scene, prepared};
use serde_json::{Value, json};
use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

fn report(names: &Sender<Option<String>>) {
    let name = thread::current().name().map(str::to_owned);
    names.send(name).expect("the test still listens");
}

/// Reports the thread that destroys it.
struct Named(Sender<Option<String>>);
impl Drop for Named {
    fn drop(&mut self) {
        report(&self.0);
    }
}

/// Reports the thread that destroys it, then waits until the test releases it.
struct Blocking {
    names: Sender<Option<String>>,
    release: Receiver<()>,
    _nested: Deferred<Named>,
}
impl Drop for Blocking {
    fn drop(&mut self) {
        report(&self.names);
        self.release
            .recv()
            .expect("the test releases the destructor");
    }
}

/// Holds the dropper inside its destructor until the test releases it, so queued values and
/// their attributions stay pending.
struct Gate(Receiver<()>);
impl Drop for Gate {
    fn drop(&mut self) {
        self.0.recv().expect("the test releases the dropper");
    }
}

#[test]
fn discarded_values_are_destroyed_and_metered_on_the_dropper() {
    assert!(!deferred::running(), "intake stays closed until start");
    deferred::start().expect("the dropper starts");
    deferred::start().expect("a second start only confirms that it runs");
    assert!(deferred::running(), "intake may open");
    let (names, reports) = mpsc::channel();
    let (release, released) = mpsc::channel();
    drop(Deferred::new(
        Blocking {
            names: names.clone(),
            release: released,
            _nested: Deferred::new(Named(names), 1_000),
        },
        64,
    ));
    let dropper = Some("scene-dropper".to_owned());
    let wait = Duration::from_secs(5);
    assert_eq!(reports.recv_timeout(wait), Ok(dropper.clone()));
    assert_eq!(
        deferred::pending(),
        Pending {
            values: 1,
            bytes: 64
        },
        "a value stays counted while its destructor runs, and its nested value is not queued"
    );
    release.send(()).expect("the destructor waits");
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(dropper),
        "the nested value drops in place on the dropper"
    );
    drain();
    assert_eq!(deferred::take_peak(), 64, "the high water since start");
    assert_eq!(
        deferred::take_peak(),
        0,
        "reading resets it to the current backlog"
    );
    retirement_sites_leave_large_destruction_to_the_dropper();
    stale_candidate_retires_its_original_source();
    stop_retires_dropped_removals_and_cell_changes();
    rejected_tracks_retire_the_partial_predecessor_map();
    rejected_inserts_and_motion_growth_stay_attributed();
    frozen_series_data_stays_attributed();
    activation_retires_settled_recolored_and_superseded_tracks();
    a_discarded_stage_frees_nothing_on_the_ui_thread();
}

fn drain() {
    let deadline = Instant::now() + Duration::from_secs(5);
    while deferred::pending() != Pending::default() {
        assert!(
            Instant::now() < deadline,
            "the destructor's return clears the count"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        deferred::running(),
        "the dropper remains live after destruction"
    );
}

fn populated() -> (SceneState, GlobalBudget, PreparedStep) {
    let mut scene = new_scene("retirement");
    let mut budget = GlobalBudget::default();
    let empty = LayoutState::new(&scene);
    let shapes: Vec<_> = (0..256)
        .map(|i| json!({"shape":{"id":format!("s{i}"),"kind":"rect","box":[0,0,1,1]}}))
        .collect();
    let step = prepared(
        &mut scene,
        &mut budget,
        &empty,
        json!({"id":"shapes","put":shapes}),
    );
    assert_eq!(
        step.after.handle(&id("s0")),
        Some(kumochi_scene::Handle::new(1, 0).expect("handle")),
        "the fixture retains its first rectangle"
    );
    (scene, budget, step)
}

fn retirement_sites_leave_large_destruction_to_the_dropper() {
    let (mut scene, mut budget, first) = populated();
    let changes: Vec<_> = (0..256)
        .map(
            |i| json!({"anim":{"target":format!("s{i}"),"track":"opacity","keys":[[0,0],[100,1]]}}),
        )
        .collect();
    let _changed = prepared(
        &mut scene,
        &mut budget,
        &first.after,
        json!({"id":"changes","put":changes}),
    );
    assert_eq!(
        scene.usage().tracks,
        256,
        "all tracks own change undo records"
    );
    let ((), [_, deallocations]) =
        allocation::activity(|| scene.clear(&mut budget).expect("clear"));
    assert_eq!(
        deallocations, 0,
        "clear retires both the object map and its undo records"
    );
    drain();

    let (mut scene, mut budget, first) = populated();
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::new(first.after.revealed_at_rest()).expect("bounded handles"),
    };
    let ((), _, [_, released]) = allocation::usage(|| {
        assert_eq!(
            scene.stop(&cut, &mut budget).expect("stop"),
            StopOutcome::Kept,
            "all revealed objects survive the Stop"
        );
    });
    assert!(
        allocation::largest_release() < 4096,
        "Stop may free its small borrowed scratch nodes, but not semantic object nodes; total {released}, largest {}",
        allocation::largest_release()
    );
    drain();

    let (mut scene, mut budget, first) = populated();
    let mut view = SceneView::new(&first.after, 1);
    drop(first);
    let removals: Vec<_> = (0..256).map(|i| format!("s{i}")).collect();
    let empty = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"remove","remove":removals}),
    );
    let mut frame = Frame::new();
    let ((), _, [_, released]) = allocation::usage(|| {
        view.activate(empty, 1, 0, &mut frame)
            .expect("empty activation");
    });
    assert!(
        released < 32 * 1024,
        "activation retires the last geometry owner: {released}"
    );
    assert_eq!(
        view.layout().revealed_at_rest(),
        Vec::new(),
        "all objects were removed"
    );
    drain();

    let (mut scene, mut budget, _) = populated();
    let Record::Step(step) = parse_record(br#"{"step":{"id":"next"}}"#).expect("step") else {
        panic!("step fixture");
    };
    let Admission::New(candidate) = scene.admit(&step, &budget).expect("candidate") else {
        panic!("new candidate");
    };
    let (committed, _, [_, released]) =
        allocation::usage(|| scene.commit(candidate, &mut budget).expect("commit"));
    assert_eq!(committed.revision, 2, "the empty second step commits");
    assert!(
        released < 256,
        "commit retires the replaced map, freeing only its small payload box: {released}"
    );
    drain();
}

fn stale_candidate_retires_its_original_source() {
    let (mut scene, mut budget, _) = populated();
    let Record::Step(mut step) = parse_record(br#"{"step":{"id":"stale"}}"#).expect("step") else {
        panic!("step fixture");
    };
    step.put = scene
        .recovery_log()
        .iter()
        .find_map(|event| match event {
            kumochi_scene::SceneEvent::Step { step, .. } => Some(step.put.clone()),
            kumochi_scene::SceneEvent::Stop(_) => None,
        })
        .expect("original operations");
    let mut fresh = new_scene("stale");
    let mut global = GlobalBudget::default();
    let Admission::New(candidate) = fresh.admit(&step, &global).expect("candidate") else {
        panic!("new candidate");
    };
    fresh.clear(&mut global).expect("clear empty scene");
    let ((), _, [_, released]) = allocation::usage(|| {
        assert_eq!(
            fresh.commit(candidate, &mut global),
            Err(kumochi_scene::StaleAdmission),
            "clearing invalidates the pending candidate"
        );
    });
    assert!(
        released < 256,
        "stale candidates retire their owned source record as well as the object map: {released}"
    );
    let ((), allocations) = allocation::count(|| scene.clear(&mut budget).expect("clear"));
    assert!(
        allocations <= 4,
        "clear hands off bounded payloads, not one allocation per object"
    );
    drain();
}

fn typed_step(value: &Value) -> Step {
    let bytes = serde_json::to_vec(&json!({"step":value})).expect("step JSON");
    let Record::Step(step) = parse_record(&bytes).expect("typed step") else {
        panic!("step fixture");
    };
    step
}

fn rejected_tracks_retire_the_partial_predecessor_map() {
    let (scene, budget, _) = populated();
    let tracks: Vec<_> = (0..256)
        .map(|i| {
            json!({"anim":{"target":format!("s{i}"),"track":"opacity","keys":[[0,0],[100,1]],
            "repeat":if i == 255 { 30_000 } else { 0 }}})
        })
        .collect();
    let step = typed_step(&json!({"id":"invalid-last-track","put":tracks}));
    let (error, queued, _, released) = gated_rejection(&scene, &budget, &step);
    assert_eq!(
        error.code,
        ErrorCode::OutOfRange,
        "the last track is rejected"
    );
    assert_eq!(
        error.path.as_str(),
        "put[255].anim.repeat",
        "255 valid predecessors are collected before this failure"
    );
    assert!(
        released < 96 * 1024,
        "the rejected predecessor map leaves the caller, retaining only bounded scratch destruction: {released}"
    );
    assert_eq!(
        queued.values, 5,
        "the gate, source copy, expansion, object map copy and partial predecessor map are queued"
    );
    assert_eq!(
        (scene.object_count(), scene.revision(), scene.usage().tracks),
        (256, 1, 0),
        "rejection leaves the authoritative scene unchanged"
    );
}

/// Rejects `step` while a gate holds the dropper, so everything the rejection queued stays
/// pending. Checks that the attributions cover what the caller allocated and still holds, short
/// only of payload boxes, channel blocks and the error. Returns the error, the pending values
/// with their attributions, and the caller's held and freed bytes.
fn gated_rejection(
    scene: &SceneState,
    budget: &GlobalBudget,
    step: &Step,
) -> (StepError, Pending, usize, usize) {
    let (release, gate) = mpsc::channel();
    drop(Deferred::new(Gate(gate), 0));
    let (result, _, [allocated, released]) = allocation::usage(|| scene.admit(step, budget));
    let queued = deferred::pending();
    release.send(()).expect("the gate waits");
    let error = result.expect_err("the fixture is rejected");
    let held = allocated - released;
    assert!(
        queued.bytes + 4096 >= held,
        "{}: {} bytes attributed, {held} held",
        error.path.as_str(),
        queued.bytes
    );
    drain();
    (error, queued, held, released)
}

fn rejected_inserts_and_motion_growth_stay_attributed() {
    let (scene, budget, _) = populated();
    let shape = |id: String| json!({"shape":{"id":id,"kind":"rect","box":[0,0,1,1]}});
    let mut put: Vec<_> = (0..299).map(|i| shape(format!("n{i}"))).collect();
    put.push(shape("s5".to_owned()));
    let reused = typed_step(&json!({"id":"reused","put":put}));
    let (error, queued, _, _) = gated_rejection(&scene, &budget, &reused);
    assert_eq!(
        (error.code, error.path.as_str(), queued.values),
        (ErrorCode::IdReused, "put[299].shape.id", 4),
        "299 objects join the map copy before the reused id; the gate, source copy, expansion and map copy are queued"
    );
    let missing = |tracked: bool| {
        let mut put: Vec<_> = (0..200).map(|i| shape(format!("n{i}"))).collect();
        if tracked {
            put.extend((0..200).map(|i| {
                json!({"anim":{"target":format!("n{i}"),"track":"opacity","keys":[[0,0],[100,1]]}})
            }));
        }
        let step = typed_step(&json!({"id":"missing","put":put,"remove":["missing"]}));
        let (error, queued, held, _) = gated_rejection(&scene, &budget, &step);
        assert_eq!(
            (error.code, error.path.as_str(), queued.values),
            (ErrorCode::UnknownRef, "remove[0]", 5),
            "new objects have no predecessors: the gate, source copy, expansion, map copy and an empty predecessor map are queued"
        );
        (queued.bytes, held)
    };
    let (plain, plain_held) = missing(false);
    let (tracked, tracked_held) = missing(true);
    assert!(
        tracked - plain >= tracked_held - plain_held,
        "the map copy's attribution grows with the 200 motion maps admission built before the unknown removal: {} more attributed, {} more held",
        tracked - plain,
        tracked_held - plain_held
    );
}

fn frozen_series_data_stays_attributed() {
    let mut scene = new_scene("series");
    let mut budget = GlobalBudget::default();
    let mut put = vec![json!({"chart":{"id":"c","kind":"line"}})];
    put.extend(
        (0..8).map(|i| json!({"series":{"id":format!("p{i}"),"in":"c","data":vec![1; 500]}})),
    );
    let step = typed_step(&json!({"id":"series","put":put}));
    let (release, gate) = mpsc::channel();
    drop(Deferred::new(Gate(gate), 0));
    let ((), _, [allocated, released]) = allocation::usage(|| {
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admitted") else {
            panic!("new step");
        };
        drop(
            scene
                .commit(candidate, &mut budget)
                .expect("fresh admission"),
        );
        scene.clear(&mut budget).expect("clear");
    });
    let queued = deferred::pending();
    release.send(()).expect("the gate waits");
    let held = allocated - released;
    assert!(
        queued.bytes + scene.attributed_bytes() + 4096 >= held,
        "the cleared map's frozen series data and the kept history stay attributed: {} queued, {} kept, {held} held",
        queued.bytes,
        scene.attributed_bytes()
    );
    drain();
}

/// UI-thread bytes freed when, at 1,000 ms, a step recolors and re-tracks 16 shapes whose
/// earlier opacity tracks settled at 93 ms and whose color and x tracks run to 4,960 ms.
fn retrack_frees(keys: u32) -> usize {
    let mut scene = new_scene("tracks");
    let mut budget = GlobalBudget::default();
    let shapes: Vec<_> = (0..16)
        .map(|i| json!({"shape":{"id":format!("s{i}"),"kind":"rect","box":[0,0,1,1]}}))
        .collect();
    let empty = LayoutState::new(&scene);
    let first = prepared(
        &mut scene,
        &mut budget,
        &empty,
        json!({"id":"shapes","put":shapes}),
    );
    let mut view = SceneView::new(&first.after, 1);
    drop(first);
    let track = |i: u32, track: &str, end: u32| {
        let values: Vec<_> = (0..keys)
            .map(|k| match track {
                "color" => json!([
                    end * k / (keys - 1),
                    if k % 2 == 0 { "red" } else { "blue" }
                ]),
                _ => json!([end * k / (keys - 1), k % 2]),
            })
            .collect();
        json!({"anim":{"target":format!("s{i}"),"track":track,"keys":values}})
    };
    let tracks: Vec<_> = (0..16)
        .flat_map(|i| {
            [
                track(i, "opacity", 93),
                track(i, "color", 4960),
                track(i, "x", 4960),
            ]
        })
        .collect();
    let running = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"tracks","put":tracks}),
    );
    let mut frame = Frame::new();
    view.activate(running, 0, 0, &mut frame)
        .expect("tracks activate");
    let recolor: Vec<_> = (0..16)
        .map(|i| json!({"target":format!("s{i}"),"color":"green"}))
        .collect();
    let retrack: Vec<_> = (0..16)
        .map(|i| json!({"anim":{"target":format!("s{i}"),"track":"x","keys":[[0,0],[300,1]]}}))
        .collect();
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"retrack","set":recolor,"put":retrack}),
    );
    drain();
    let ((), _, [_, released]) = allocation::usage(|| {
        view.activate(next, 1000, 0, &mut frame)
            .expect("retrack activates");
    });
    assert_eq!(
        (view.next_wake(1000), view.next_wake(1300)),
        (Some(1300), None),
        "only the 16 new x tracks run; no settled, recolored or superseded track survives"
    );
    drain();
    released
}

fn activation_retires_settled_recolored_and_superseded_tracks() {
    assert_eq!(
        retrack_frees(32),
        retrack_frees(2),
        "the UI thread frees only activation scratch, never the 48 rejected key boxes, which differ by 600 bytes each"
    );
}

/// A refused host transition drops its staged activation: 256 compiled tracks, the candidate
/// nodes and the layout copy.
fn a_discarded_stage_frees_nothing_on_the_ui_thread() {
    let (mut scene, mut budget, first) = populated();
    let view = SceneView::new(&first.after, 1);
    drop(first);
    let tracks: Vec<_> = (0..256)
        .map(
            |i| json!({"anim":{"target":format!("s{i}"),"track":"opacity","keys":[[0,0],[100,1]]}}),
        )
        .collect();
    let next = prepared(
        &mut scene,
        &mut budget,
        view.layout(),
        json!({"id":"tracks","put":tracks}),
    );
    let staged = view.prepare_activation(&next, 0, 0).expect("stage");
    let layout_bytes = next.retained_bytes();
    drain();
    let (release, released) = mpsc::channel();
    drop(Deferred::new(Gate(released), 0));
    let ((), _, [_, released]) = allocation::usage(|| drop(staged));
    assert_eq!(released, 0, "the whole stage goes to the dropper");
    assert!(
        deferred::pending().bytes >= layout_bytes,
        "the queued stage attributes its whole shared layout, not only the candidate view"
    );
    release.send(()).expect("the dropper gate waits");
    drain();
}

fn dropped_rows() -> (SceneState, GlobalBudget, StopCut) {
    let mut scene = new_scene("dropped-undo");
    let mut budget = GlobalBudget::default();
    let mut put: Vec<_> = (0..8)
        .map(|i| json!({"table":{"id":format!("t{i}"),"cols":["A","B"],"box":[0,0,300,300]}}))
        .collect();
    put.extend(
        (0..256).map(
            |i| json!({"row":{"id":format!("r{i}"),"in":format!("t{}",i / 32),"cells":["old"]}}),
        ),
    );
    let remove: Vec<_> = (0..128).map(|i| format!("r{i}")).collect();
    let changes: Vec<_> = (128..256)
        .map(|i| json!({"target":format!("r{i}"),"cells":["new","value"]}))
        .collect();
    for step in [
        typed_step(&json!({"id":"rows","put":put})),
        typed_step(&json!({"id":"remove-and-set","remove":remove,"set":changes})),
    ] {
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admitted fixture")
        else {
            panic!("new step");
        };
        scene
            .commit(candidate, &mut budget)
            .expect("committed fixture");
    }
    assert_eq!(
        scene.object_count(),
        136,
        "the second step removes 128 of 256 rows beside eight tables"
    );
    let revealed = (0..264)
        .map(|i| kumochi_scene::Handle::new(1, i).expect("table or row handle"))
        .collect();
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::new(revealed).expect("kept rows"),
    };
    (scene, budget, cut)
}

fn stop_retires_dropped_removals_and_cell_changes() {
    let (mut scene, mut budget, cut) = dropped_rows();
    let (outcome, _, [_, released]) = allocation::usage(|| scene.stop(&cut, &mut budget));
    assert_eq!(
        outcome.expect("valid cut"),
        StopOutcome::Kept,
        "the first step survives"
    );
    assert_eq!(
        (scene.object_count(), scene.committed(), scene.revision()),
        (264, 1, 2),
        "Stop restores the removed rows and drops the second step without inventing a revision"
    );
    assert_eq!(
        scene.step(1),
        None,
        "the removed-and-set step is no longer kept"
    );
    assert_eq!(
        scene.recovery_log().len(),
        3,
        "both original steps and the cut remain in history"
    );
    assert!(
        released < 128 * 1024,
        "the dropped-step fixture frees bounded scratch, not its large maps and undo records: {released}"
    );
    drain();
}

#[test]
#[ignore = "run with --exact --ignored alone, normal priority under the Rez quiet lock"]
fn measure_stop_with_drops_before_start() {
    assert!(!deferred::running());
    measure_stop_with_drops();
}

#[test]
#[ignore = "normal-priority release measurement under the Rez quiet lock"]
fn measure_stop_with_drops_started() {
    deferred::start().expect("dropper starts before intake");
    measure_stop_with_drops();
}

#[expect(clippy::print_stdout, reason = "manual Rez measurement output")]
fn measure_stop_with_drops() {
    let mut times = Vec::with_capacity(32);
    let mut released_bytes = Vec::with_capacity(32);
    for sample in 0..37 {
        if deferred::running() {
            drain();
        }
        let (mut scene, mut budget, cut) = dropped_rows();
        let start = Instant::now();
        let (outcome, _, [allocated, released]) =
            allocation::usage(|| scene.stop(&cut, &mut budget));
        let elapsed = start.elapsed().as_nanos();
        assert_eq!(
            outcome.expect("valid cut"),
            StopOutcome::Kept,
            "the first step survives the cut"
        );
        assert_eq!(
            (scene.object_count(), scene.committed(), scene.revision()),
            (264, 1, 2),
            "the cut drops the second step without rewinding the lifetime revision"
        );
        if sample >= 5 {
            times.push(elapsed);
            released_bytes.push(released);
            println!(
                "stop_dropped_undo sample={} elapsed_ns={elapsed} allocated_bytes={allocated} caller_freed_bytes={released}",
                sample - 5
            );
        }
    }
    if deferred::running() {
        drain();
    }
    times.sort_unstable();
    released_bytes.sort_unstable();
    println!(
        "stop_dropped_undo dropper={} samples=32 warmups=5 p50_ns={} max_ns={} caller_freed_p50_bytes={} caller_freed_max_bytes={}",
        deferred::running(),
        times[15],
        times[31],
        released_bytes[15],
        released_bytes[31]
    );
}
