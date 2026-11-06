use std::time::Instant;

use kumochi_protocol::message::{DataToShell, SceneId};
use kumochi_scene::deferred::Deferred;

use crate::allocation;

#[path = "../../../scene/tests/support/maximal_content.rs"]
mod maximal_content;

use super::{GENERATION, RUN, Shaper, begin, commit, controls, live, only_job, step};

#[test]
#[ignore = "release reducer timing on quiet Rez, no concurrent builds"]
#[expect(
    clippy::too_many_lines,
    reason = "construct and verify full histories with bounded read-ahead outside the timers"
)]
fn measure_motion_heavy_prepared_tip_admission_and_clear() {
    use kumochi_protocol::frame::{self, Pipe};

    kumochi_scene::deferred::start().expect("dropper starts before scene intake");

    for counts in [vec![600], vec![375; 4]] {
        let tracks = |range: std::ops::Range<usize>| {
            range
                .map(|i| {
                    format!(
                        r#"{{"anim":{{"target":"s{i}","track":"opacity","keys":[[0,0],[60000,1]]}}}}"#
                    )
                })
                .collect::<Vec<_>>()
        };
        let workloads = counts
            .iter()
            .map(|&objects| {
                let shapes = |range: std::ops::Range<usize>| {
                    range.map(|i| {
                        if counts.len() == 1 {
                            format!(r#"{{"shape":{{"id":"s{i}","kind":"rect","box":[0,0,1,1]}}}}"#)
                        } else {
                            format!(r#"{{"shape":{{"id":"s{i}","kind":"polygon","points":[[0,0],[1,0],[1,1],[0,1]]}}}}"#)
                        }
                    })
                };
                let first = shapes(0..128)
                    .chain(tracks(0..128))
                    .collect::<Vec<_>>()
                    .join(",");
                let second = shapes(128..256)
                    .chain(tracks(128..256))
                    .collect::<Vec<_>>()
                    .join(",");
                let changes = (0..objects)
                    .map(|i| format!(r#"{{"target":"s{i}","color":"red"}}"#))
                    .collect::<Vec<_>>()
                    .join(",");
                let third = shapes(256..objects).collect::<Vec<_>>().join(",");
                let last = if counts.len() == 1 {
                    step(&format!(r#"{{"step":{{"id":"last","set":[{changes}]}}}}"#))
                } else {
                    step(r#"{"step":{"id":"last"}}"#)
                };
                (
                    step(&format!(r#"{{"step":{{"id":"first","put":[{first}]}}}}"#)),
                    step(&format!(r#"{{"step":{{"id":"second","put":[{second}]}}}}"#)),
                    step(&format!(r#"{{"step":{{"id":"third","put":[{third}]}}}}"#)),
                    last,
                )
            })
            .collect::<Vec<_>>();
        let empty = (3..63)
            .map(|index| step(&format!(r#"{{"step":{{"id":"empty{index}"}}}}"#)))
            .collect::<Vec<_>>();
        let mut admissions = Vec::new();
        let mut clears = Vec::new();
        let mut consumptions = Vec::new();
        let mut queued_consumptions = Vec::new();
        for iteration in 0..37 {
            let (mut mirror, now) = live();
            for (offset, (first, second, third, _)) in workloads.iter().enumerate() {
                let id = SceneId(100 + u64::try_from(offset).expect("four scenes"));
                begin(&mut mirror, id, now);
                for (index, value) in std::iter::once(first)
                    .chain(std::iter::once(second))
                    .chain(std::iter::once(third))
                    .chain(empty.iter())
                    .enumerate()
                {
                    if mirror.scenes[&id].ready.len() == super::super::scene::WINDOW {
                        if index == super::super::scene::WINDOW {
                            let effects = commit(
                                &mut mirror,
                                id,
                                u32::try_from(index).expect("ninth step"),
                                value.clone(),
                                now,
                            )
                            .expect("a successor held by the window");
                            assert!(super::jobs(effects).is_empty());
                            let head = mirror.scenes[&id]
                                .ready
                                .front()
                                .expect("the head")
                                .step
                                .index;
                            let start = Instant::now();
                            let consumed = mirror
                                .consume_ready(id, head, now, 0, 0)
                                .expect("the head fits");
                            let job = only_job(consumed.effects.expect("the successor dispatch"));
                            drop(consumed.step);
                            let elapsed = start.elapsed().as_nanos();
                            let effects = mirror
                                .completed(job.run(&mut Shaper), now)
                                .expect("the queued successor prepared");
                            assert!(effects.is_empty());
                            if iteration >= 5 {
                                queued_consumptions.push(elapsed);
                            }
                            continue;
                        }
                        let elapsed = consume_timed(&mut mirror, id, now);
                        if iteration >= 5 {
                            consumptions.push(elapsed);
                        }
                    }
                    let job = only_job(
                        commit(
                            &mut mirror,
                            id,
                            u32::try_from(index).expect("63 prefixes"),
                            value.clone(),
                            now,
                        )
                        .expect("legal prefix"),
                    );
                    let effects = mirror
                        .completed(job.run(&mut Shaper), now)
                        .expect("prepared prefix");
                    assert!(controls(&effects).is_empty(), "prefix within residency");
                }
                assert_eq!(mirror.scenes[&id].ready.len(), super::super::scene::WINDOW);
                assert_eq!(mirror.scenes[&id].tip().revision(), 63);
                assert_eq!(
                    mirror.scenes[&id].state.view().object_count(),
                    counts[offset]
                );
            }
            let usage = mirror.global.usage();
            assert_eq!(usage.objects, counts.iter().sum::<usize>());
            assert_eq!(usage.scenes, counts.len());
            assert_eq!(usage.tracks, 256 * counts.len());
            assert_eq!(usage.keys, 512 * counts.len());
            assert_eq!(
                usage.segments,
                if counts.len() == 1 { 17_400 } else { 6_000 }
            );
            let mut elapsed = Vec::with_capacity(workloads.len());
            let mut receipts = Vec::with_capacity(workloads.len());
            let mut applies = Vec::with_capacity(workloads.len());
            for (offset, (_, _, _, last)) in workloads.iter().enumerate() {
                let scene = SceneId(100 + u64::try_from(offset).expect("four scenes"));
                let consumed = consume_timed(&mut mirror, scene, now);
                if iteration >= 5 {
                    consumptions.push(consumed);
                }
                let message = DataToShell::SceneStepCommit {
                    run: RUN,
                    scene,
                    index: 63,
                    step: last.clone(),
                };
                let bytes = u32::try_from(
                    frame::encode(&message, Pipe::Data)
                        .expect("bounded frame")
                        .len(),
                )
                .expect("frame bytes");
                let start = Instant::now();
                let effects = mirror
                    .data(GENERATION, Deferred::new(message, 0), bytes, now)
                    .expect("64th admission");
                let admission = super::only_admission(effects);
                let receipt = start.elapsed().as_nanos();
                let done = admission.run();
                let start = Instant::now();
                let effects = mirror.completed(done, now).expect("64th admitted prefix");
                let job = only_job(effects);
                let apply = start.elapsed().as_nanos();
                elapsed.push(receipt + apply);
                receipts.push(receipt);
                applies.push(apply);
                let effects = mirror
                    .completed(job.run(&mut Shaper), now)
                    .expect("64th prepared prefix");
                assert!(
                    controls(&effects).is_empty(),
                    "64 lifetime prefixes with bounded residency"
                );
                assert_eq!(
                    mirror.scenes[&scene].ready.len(),
                    super::super::scene::WINDOW
                );
            }
            let charged = mirror.residency();
            let start = Instant::now();
            let effects = mirror.clear(now).expect("clear full histories");
            drop(effects);
            let cleared = start.elapsed().as_nanos();
            assert!(mirror.scenes.is_empty());
            if iteration >= 5 {
                admissions.extend_from_slice(&elapsed);
                clears.push(cleared);
                println!(
                    "motion_admit_parts receipt_ns={receipts:?} apply_ns={applies:?} sum_ns={elapsed:?}; reducer and effect selection, executor dispatch and pipe drain unmeasured"
                );
                println!(
                    "motion_ui_raw scenes={} objects={} lifetime_prefixes=64 ready_per_scene=8 tracks_per_scene=256 sample={} admission_ns={elapsed:?} clear_ns={cleared} charged_bytes={charged}",
                    counts.len(),
                    counts.iter().sum::<usize>(),
                    iteration - 5
                );
            }
        }
        admissions.sort_unstable();
        clears.sort_unstable();
        consumptions.sort_unstable();
        queued_consumptions.sort_unstable();
        println!(
            "motion_consume_queued scenes={} objects={} samples={} p50_ns={} p95_ns={} max_ns={}; ninth successor queued; includes retirement, rest clone, successor input clone, owner release and effect selection; executor dispatch unmeasured",
            counts.len(),
            counts.iter().sum::<usize>(),
            queued_consumptions.len(),
            queued_consumptions[queued_consumptions.len() / 2],
            queued_consumptions[queued_consumptions.len() * 95 / 100],
            queued_consumptions[queued_consumptions.len() - 1],
        );
        println!(
            "motion_consume scenes={} objects={} samples={} p50_ns={} p95_ns={} max_ns={}; no queued successor; includes retirement, rest index clone, owner credit and effects disposal",
            counts.len(),
            counts.iter().sum::<usize>(),
            consumptions.len(),
            consumptions[consumptions.len() / 2],
            consumptions[consumptions.len() * 95 / 100],
            consumptions[consumptions.len() - 1],
        );
        println!(
            "motion_ui scenes={} objects={} admission_samples={} clear_samples=32 warmups=5 admission_p50_ns={} admission_p95_ns={} admission_max_ns={} clear_p50_ns={} clear_p95_ns={} clear_max_ns={}; {} reducer, eight-ready tip, clear includes output destruction, setup/preparation excluded, no paint",
            counts.len(),
            counts.iter().sum::<usize>(),
            admissions.len(),
            admissions[admissions.len() / 2 - 1],
            admissions[admissions.len() * 95 / 100],
            admissions[admissions.len() - 1],
            clears[15],
            clears[30],
            clears[31],
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
}

fn consume_timed(presenter: &mut super::Presenter, scene: SceneId, now: Instant) -> u128 {
    let index = presenter.scenes[&scene]
        .ready
        .front()
        .expect("a ready head")
        .step
        .index;
    let start = Instant::now();
    let consumed = presenter
        .consume_ready(scene, index, now, 0, 0)
        .expect("the head fits");
    let effects = consumed.effects.expect("a current scene");
    let empty = effects.is_empty();
    drop(effects);
    drop(consumed.step);
    let elapsed = start.elapsed().as_nanos();
    assert!(empty, "no queued successor in this admission fixture");
    elapsed
}

#[test]
#[ignore = "run with --exact --ignored alone under the quiet-Rez lock; no window or input"]
#[expect(
    clippy::too_many_lines,
    reason = "keep ownership measurement in one worker"
)]
fn measure_native_maximal_content_residency() {
    use kumochi_scene::{
        Admission, GlobalBudget, SceneState,
        layout::{LayoutState, PrepareInput, PreparedStep, SurfaceGeometry, prepare},
        theme::Theme,
    };
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    assert!(!kumochi_scene::deferred::running());
    let app = gpui_kit::application();
    for content in [
        maximal_content::Content::UnicodeBytes,
        maximal_content::Content::AsciiCells,
    ] {
        let ascii = matches!(content, maximal_content::Content::AsciiCells);
        let text = app.text_system();
        let (sender, receiver) = mpsc::sync_channel(1);
        app.background_executor()
            .spawn(async move {
                let mut measure =
                    super::super::Measure::new(text, gpui_kit::Font::default().family);
                let theme = Arc::new(Theme::default());
                let mut budget = GlobalBudget::default();
                let mut states = Vec::with_capacity(4);
                let mut histories: Vec<Vec<PreparedStep>> = Vec::with_capacity(4);
                let mut empty_charge = 0;
                let mut drawing_bytes = 0;
                for scene in 0..4 {
                    let (bytes, steps) = maximal_content::fixture(content, scene);
                    drawing_bytes = bytes;
                    let mut state = SceneState::begin(super::header().begin).expect("scene");
                    let empty = LayoutState::new(&state);
                    empty_charge += empty.resident_bytes();
                    let mut history: Vec<PreparedStep> = Vec::with_capacity(64);
                    for source in &steps {
                        let Admission::New(candidate) =
                            state.admit(source, &budget).expect("legal content")
                        else {
                            panic!("new fixture step");
                        };
                        let committed = state.commit(candidate, &mut budget).expect("commit");
                        let before = history.last().map_or(&empty, |prepared| &prepared.after);
                        let input = PrepareInput::new(
                            committed.preparation,
                            before.clone(),
                            SurfaceGeometry::Overlay {
                                logical_size: [1920.0, 1080.0],
                            },
                            Arc::clone(&theme),
                        )
                        .expect("next prefix");
                        history.push(
                            prepare(input, &mut measure, &AtomicBool::new(false))
                                .expect("native-font preparation"),
                        );
                    }
                    histories.push(history);
                    states.push(state);
                }
                let usage = budget.usage();
                assert_eq!(
                    (usage.scenes, usage.objects, usage.cells, usage.marks),
                    (4, 1500, 4000, 20000)
                );
                let full_charge = empty_charge
                    + histories
                        .iter()
                        .flatten()
                        .map(|prepared| prepared.resident_bytes)
                        .sum::<usize>();
                let windows: Vec<_> = [1, 2, 4, 8, 16, 32, 63]
                    .into_iter()
                    .map(|ready| {
                        let peak = histories
                            .iter()
                            .map(|history| {
                                history
                                    .windows(ready + 1)
                                    .map(|window| {
                                        window[0].after.resident_bytes()
                                            + window[1..]
                                                .iter()
                                                .map(|prepared| prepared.resident_bytes)
                                                .sum::<usize>()
                                    })
                                    .max()
                                    .expect("nonempty windows")
                            })
                            .sum::<usize>();
                        (ready, peak)
                    })
                    .collect();
                let ((), _, [_, released]) = allocation::usage(|| drop(histories));
                assert!(
                    released <= full_charge,
                    "native prepared ownership releases {released} but charges {full_charge}"
                );
                sender
                    .send((drawing_bytes, full_charge, released, windows))
                    .expect("measurement receiver");
                drop(states);
            })
            .detach();
        let (drawing_bytes, full_charge, released, windows) = receiver
            .recv_timeout(std::time::Duration::from_secs(180))
            .expect("native residency worker");
        println!(
            "native_maximal_content ascii_cells={ascii} scenes=4 objects=1500 cells=4000 marks=20000 prefixes=256 drawing_bytes_per_scene={drawing_bytes} all_prefix_charge_bytes={full_charge} released_prepared_bytes={released}"
        );
        for (ready, charge) in windows {
            println!(
                "native_maximal_content ascii_cells={ascii} ready_per_scene={ready} peak_charge_bytes={charge}"
            );
        }
    }
}

#[test]
#[ignore = "run with --exact --ignored alone under the quiet-Rez lock; no window or input"]
#[expect(
    clippy::too_many_lines,
    reason = "drive the real eight-window scheduler and returned owners on native font shaping"
)]
fn measure_native_eight_window_residency() {
    use kumochi_scene::limits::MAX_RESIDENT_BYTES;
    use std::{sync::mpsc, time::Duration};

    let _timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(420));
        std::process::exit(124);
    });
    assert!(!kumochi_scene::deferred::running());
    let app = gpui_kit::application();
    for content in [
        maximal_content::Content::UnicodeBytes,
        maximal_content::Content::AsciiCells,
    ] {
        let ascii = matches!(content, maximal_content::Content::AsciiCells);
        let text = app.text_system();
        let (sender, receiver) = mpsc::sync_channel(1);
        app.background_executor()
            .spawn(async move {
                let mut measure =
                    super::super::Measure::new(text, gpui_kit::Font::default().family);
                let (mut presenter, now) = live();
                let mut running = Vec::new();
                let mut drawing_bytes = 0;
                for offset in 0..4 {
                    let id = SceneId(200 + u64::from(offset));
                    begin(&mut presenter, id, now);
                    let (bytes, steps) = maximal_content::fixture(content, offset);
                    drawing_bytes = bytes;
                    for (index, source) in steps.into_iter().enumerate() {
                        let effects = commit(
                            &mut presenter,
                            id,
                            u32::try_from(index).expect("64 steps"),
                            source,
                            now,
                        )
                        .expect("legal maximal content");
                        assert!(controls(&effects).is_empty());
                        running.extend(super::jobs(effects));
                    }
                }
                while let Some(job) = running.pop() {
                    let effects = presenter
                        .completed(job.run(&mut measure), now)
                        .expect("a native answer");
                    assert!(controls(&effects).is_empty(), "every prefix fits");
                    running.extend(super::jobs(effects));
                }
                let usage = presenter.global.usage();
                assert_eq!(
                    (usage.scenes, usage.objects, usage.cells, usage.marks),
                    (4, 1500, 4000, 20000)
                );
                for scene in presenter.scenes.values() {
                    assert_eq!(scene.ready.len(), super::super::scene::WINDOW);
                    assert_eq!(scene.queue.len(), 56);
                }
                let mut peak = presenter.residency();
                let mut active: Vec<_> = (0..4).map(|_| None).collect();
                let mut consumed = 0;
                for expected in 0..64 {
                    for (offset, previous) in active.iter_mut().enumerate() {
                        let id = SceneId(200 + u64::try_from(offset).expect("four scenes"));
                        let done = presenter
                            .consume_ready(id, expected, now, 0, 0)
                            .expect("the next native head fits with the prior owner held");
                        assert_eq!(done.step.step().index, expected);
                        let scene = presenter.scenes.get_mut(&id).expect("the consumed scene");
                        scene.view = kumochi_scene::SceneView::new(
                            &scene.rest,
                            scene.state.view().activated(),
                        );
                        scene.view_bytes = scene.view.resident_bytes();
                        peak = peak.max(presenter.residency());
                        drop(previous.replace(done.step));
                        let effects = done.effects.expect("a current scene");
                        assert!(controls(&effects).is_empty());
                        running.extend(super::jobs(effects));
                        while let Some(job) = running.pop() {
                            let effects = presenter
                                .completed(job.run(&mut measure), now)
                                .expect("a native answer");
                            assert!(controls(&effects).is_empty(), "every successor fits");
                            running.extend(super::jobs(effects));
                            peak = peak.max(presenter.residency());
                        }
                        for scene in presenter.scenes.values() {
                            assert!(
                                scene.ready.len() + usize::from(scene.running.is_some())
                                    <= super::super::scene::WINDOW
                            );
                        }
                        assert!(presenter.residency() <= MAX_RESIDENT_BYTES);
                        consumed += 1;
                    }
                }
                assert_eq!(consumed, 256);
                for scene in presenter.scenes.values() {
                    assert_eq!(scene.state.view().activated(), 64);
                    assert!(scene.ready.is_empty() && scene.queue.is_empty());
                }
                assert!(peak <= MAX_RESIDENT_BYTES);
                drop(active);
                assert_eq!(
                    presenter
                        .returned
                        .load(std::sync::atomic::Ordering::Relaxed),
                    0
                );
                drop(presenter.clear(now).expect("clear native rest"));
                assert_eq!(presenter.residency(), 0);
                sender.send((drawing_bytes, peak)).expect("the receiver");
            })
            .detach();
        let (drawing_bytes, peak) = receiver
            .recv_timeout(Duration::from_secs(180))
            .expect("native window worker");
        println!(
            "native_eight_window ascii_cells={ascii} scenes=4 objects=1500 cells=4000 marks=20000 lifetime_prefixes=256 ready_per_scene=8 consumed=256 drawing_bytes_per_scene={drawing_bytes} peak_charge_bytes={peak} allowance_bytes={MAX_RESIDENT_BYTES}; fixture replaces resting views after consumption, reserved frames included, one returned owner per scene, previous retained until next handoff, synthetic scheduler clock, no production activation, paint or RSS measurement"
        );
    }
}
