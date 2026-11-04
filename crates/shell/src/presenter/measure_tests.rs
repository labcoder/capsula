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
    reason = "construct and verify full prepared histories outside admission and clear timers"
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
                assert_eq!(mirror.scenes[&id].ready.len(), 63);
                assert_eq!(mirror.scenes[&id].tip().revision(), 63);
                assert_eq!(mirror.scenes[&id].state.object_count(), counts[offset]);
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
            for (offset, (_, _, _, last)) in workloads.iter().enumerate() {
                let scene = SceneId(100 + u64::try_from(offset).expect("four scenes"));
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
                let sample = start.elapsed().as_nanos();
                elapsed.push(sample);
                let job = only_job(effects);
                let effects = mirror
                    .completed(job.run(&mut Shaper), now)
                    .expect("64th prepared prefix");
                assert!(
                    controls(&effects).is_empty(),
                    "64 prefixes within residency"
                );
                assert_eq!(mirror.scenes[&scene].ready.len(), 64);
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
                    "motion_ui_raw scenes={} objects={} prefixes=64 tracks_per_scene=256 sample={} admission_ns={elapsed:?} clear_ns={cleared} charged_bytes={charged}",
                    counts.len(),
                    counts.iter().sum::<usize>(),
                    iteration - 5
                );
            }
        }
        admissions.sort_unstable();
        clears.sort_unstable();
        println!(
            "motion_ui scenes={} objects={} admission_samples={} clear_samples=32 warmups=5 admission_p50_ns={} admission_max_ns={} clear_p50_ns={} clear_max_ns={}; {} reducer, fully prepared tip, clear includes output destruction, setup/preparation excluded, no paint",
            counts.len(),
            counts.iter().sum::<usize>(),
            admissions.len(),
            admissions[admissions.len() / 2 - 1],
            admissions[admissions.len() - 1],
            clears[15],
            clears[31],
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
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
