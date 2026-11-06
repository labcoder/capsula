use std::time::{Duration, Instant};

use kumochi_scene::{
    Step,
    display::{MeasureError, TextMeasure, TextStyle},
};

use super::{LIVE, Shaper, begin, commit, controls, live, only_job, step};

pub(crate) fn drawing(iteration: usize, extra_labels: usize) -> Step {
    let source = format!(
        r#"{{"step":{{"id":"cap","put":[{}]}}}}"#,
        drawing_put(iteration, extra_labels).join(",")
    );
    assert!(source.len() <= kumochi_scene::limits::MAX_RECORD_BYTES);
    step(&source)
}

fn drawing_put(iteration: usize, extra_labels: usize) -> Vec<String> {
    let label = |index| {
        format!(
            "stage {iteration:03} label {index:02} {}",
            "drawing label ".repeat(32)
        )
        .chars()
        .take(400)
        .collect::<String>()
    };
    let mut put = Vec::new();
    for index in 0..4 {
        put.push(format!(r#"{{"shape":{{"id":"box{index}","kind":"rect","box":[{},150,120,80],"radius":8,"fill":"blue","label":"{}"}}}}"#,40 + index * 300,label(index)));
    }
    for index in 0..25 {
        let points = if index == 24 {
            "[[0,0],[1,0],[1,1],[0,1]]"
        } else {
            "[[0,0],[1,0],[0,1]]"
        };
        put.push(format!(
            r#"{{"shape":{{"id":"fill{index}","kind":"polygon","points":{points},"fill":"blue"}}}}"#
        ));
    }
    for index in 0..12 {
        let radius = if index < 4 { r#", "radius":0.001"# } else { "" };
        put.push(format!(
            r#"{{"shape":{{"id":"outline{index}","kind":"rect","box":[0,0,8,8]{radius}}}}}"#
        ));
    }
    put.push(
        r#"{"line":{"id":"wire","from":{"ref":"box0"},"to":[5200,190],"dash":true}}"#.to_owned(),
    );
    for index in 0..extra_labels {
        put.push(format!(r#"{{"text":{{"id":"text{index}","at":[100,800],"text":"{}","max_width":240,"bold":{}}}}}"#,label(index + 4),index % 2 == 0));
    }
    put
}

struct Timed<M> {
    inner: M,
    nanos: u128,
    calls: usize,
}

impl<M: TextMeasure> TextMeasure for Timed<M> {
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let start = Instant::now();
        let result = self.inner.measure(text, style, max_width);
        self.nanos += start.elapsed().as_nanos();
        self.calls += 1;
        result
    }
}

#[test]
fn the_native_preparation_fixture_fills_every_paint_cap_and_shapes_every_label() {
    for extra in [0, 40] {
        let state = kumochi_scene::SceneState::begin(super::header().begin).expect("header");
        drop(
            state
                .admit(&drawing(0, extra), &kumochi_scene::GlobalBudget::default())
                .expect("legal cap fixture"),
        );
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let job = only_job(commit(&mut presenter, LIVE, 0, drawing(0, extra), now).expect("admit"));
        let mut measure = Timed {
            inner: Shaper,
            nanos: 0,
            calls: 0,
        };
        let effects = presenter
            .completed(job.run(&mut measure), now)
            .expect("prepared answer");
        assert!(controls(&effects).is_empty());
        let work = presenter.paint_work();
        assert_eq!(
            (work.dash_pieces, work.fill_vertices, work.outline_points),
            (512, 512, 512)
        );
        assert_eq!(measure.calls, 4 + extra);
        assert_eq!(presenter.scenes[&LIVE].ready.len(), 1);
    }
}

#[test]
#[ignore = "exact isolated optimized quiet-Rez full preparation and native text shaping"]
fn measure_native_full_preparation_at_paint_caps() {
    let _timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(180));
        std::process::exit(124);
    });
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let app = gpui_kit::application();
    let family = gpui_kit::component::Theme::default().font_family;
    let text = app.text_system();
    app.background_executor()
        .spawn(async move {
            let mut measure = Timed {
                inner: super::super::Measure::new(text, family),
                nanos: 0,
                calls: 0,
            };
            let mut rows = Vec::new();
            for iteration in 0..136 {
                for extra in if iteration % 2 == 0 { [0, 40] } else { [40, 0] } {
                    let source = drawing(iteration, extra);
                    let (mut presenter, now) = live();
                    begin(&mut presenter, LIVE, now);
                    measure.nanos = 0;
                    measure.calls = 0;
                    let start = Instant::now();
                    let job =
                        only_job(commit(&mut presenter, LIVE, 0, source, now).expect("admit"));
                    let admitted = start.elapsed().as_nanos();
                    let prepare_start = Instant::now();
                    let done = job.run(&mut measure);
                    let prepared = prepare_start.elapsed().as_nanos();
                    let effects = presenter.completed(done, now).expect("install");
                    let full = start.elapsed().as_nanos();
                    assert!(controls(&effects).is_empty());
                    let work = presenter.paint_work();
                    assert_eq!(
                        (work.dash_pieces, work.fill_vertices, work.outline_points),
                        (512, 512, 512)
                    );
                    assert_eq!(measure.calls, 4 + extra);
                    if iteration >= 8 {
                        rows.push((extra, admitted, prepared, measure.nanos, full));
                    }
                }
            }
            sender.send(rows).expect("measurement receiver");
        })
        .detach();
    let rows = receiver
        .recv_timeout(Duration::from_secs(150))
        .expect("bounded worker");
    println!(
        "cap_prepare method=real_presenter_admission_prepare_install native_font=true font=product_theme unique_text=true warmups=8 samples_per_case=128 dash=512 fill=512 outline=512; no queue delay, window paint or compositor claim"
    );
    for (extra, admission_ns, prepare_ns, shaping_ns, full_ns) in rows {
        println!(
            "cap_prepare_sample labels={} chars_per_label=400 admission_ns={admission_ns} prepare_ns={prepare_ns} shaping_ns={shaping_ns} full_ns={full_ns}",
            4 + extra
        );
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one legal four-scene eight-window ownership cycle"
)]
fn window_residency(measure: &mut dyn TextMeasure) -> usize {
    use kumochi_protocol::message::SceneId;
    use kumochi_scene::{SceneView, limits::MAX_RESIDENT_BYTES};

    let (mut presenter, now) = live();
    let mut running = Vec::new();
    for offset in 0..4_u64 {
        let id = SceneId(100 + offset);
        begin(&mut presenter, id, now);
        let mut put = if offset == 0 {
            drawing_put(0, 40)
        } else {
            Vec::new()
        };
        for index in 0..(375 - put.len()) {
            put.push(format!(
                r#"{{"line":{{"id":"line{index}","from":[0,0],"to":[1,1]}}}}"#
            ));
        }
        let initial: Vec<_> = put
            .chunks(64)
            .enumerate()
            .map(|(index, put)| {
                let encoded = format!(
                    r#"{{"step":{{"id":"initial{index}","put":[{}]}}}}"#,
                    put.join(",")
                );
                assert!(encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES);
                step(&encoded)
            })
            .collect();
        for index in 0..64 {
            let source = if let Some(initial) =
                initial.get(usize::try_from(index).expect("prefix index"))
            {
                initial.clone()
            } else {
                let target = if offset == 0 { "box0" } else { "line0" };
                let color = if index % 2 == 0 { "red" } else { "blue" };
                step(&format!(
                    r#"{{"step":{{"id":"change{index}","set":[{{"target":"{target}","color":"{color}"}}]}}}}"#
                ))
            };
            running.extend(super::jobs(
                commit(&mut presenter, id, index, source, now).expect("admit"),
            ));
        }
    }
    while let Some(job) = running.pop() {
        let effects = presenter
            .completed(job.run(measure), now)
            .expect("prepared");
        assert!(controls(&effects).is_empty());
        running.extend(super::jobs(effects));
    }
    assert_eq!(
        (presenter.scenes.len(), presenter.global.usage().objects),
        (4, 1500)
    );
    let work = presenter.paint_work();
    assert_eq!(
        (work.dash_pieces, work.fill_vertices, work.outline_points),
        (512, 512, 512)
    );
    for scene in presenter.scenes.values() {
        assert_eq!((scene.ready.len(), scene.queue.len()), (8, 56));
    }
    let mut peak = presenter.residency();
    let mut previous = [None, None, None, None];
    for index in 0..64 {
        for (offset, prior) in previous.iter_mut().enumerate() {
            let id = SceneId(100 + u64::try_from(offset).expect("four scenes"));
            let consumed = presenter.consume_ready(id, index, now).expect("ready head");
            assert_eq!(consumed.step.step().index, index);
            presenter.frames.borrow_mut().replaced(id);
            let scene = presenter.scenes.get_mut(&id).expect("scene");
            scene.view = SceneView::new(&scene.rest, scene.state.view().activated());
            scene.view_bytes = scene.view.resident_bytes();
            let (sample, effects) = presenter.sample(0);
            assert!(sample.visible);
            assert!(effects.is_empty());
            peak = peak.max(presenter.residency());
            drop(prior.replace(consumed.step));
            running.extend(super::jobs(consumed.effects.expect("current scene")));
            while let Some(job) = running.pop() {
                let effects = presenter
                    .completed(job.run(measure), now)
                    .expect("successor");
                assert!(controls(&effects).is_empty());
                running.extend(super::jobs(effects));
                peak = peak.max(presenter.residency());
            }
            assert!(presenter.residency() <= MAX_RESIDENT_BYTES);
        }
    }
    for scene in presenter.scenes.values() {
        assert_eq!(scene.state.view().activated(), 64);
        assert_eq!((scene.ready.len(), scene.queue.len()), (0, 0));
    }
    let mut items = 0;
    assert!(!presenter.frames.borrow().paint(|frame| {
        items += frame.items().len();
        Ok::<_, std::fmt::Error>(())
    }));
    assert_eq!(items, 1500);
    drop(previous);
    assert_eq!(
        presenter
            .returned
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    drop(presenter.clear(now).expect("clear"));
    assert_eq!(presenter.residency(), 0);
    assert!(peak <= MAX_RESIDENT_BYTES);
    peak
}

#[test]
fn the_paint_cap_window_fixture_fills_eight_ready_slots_and_consumes_every_prefix() {
    window_residency(&mut Shaper);
}

#[test]
#[ignore = "exact isolated quiet-Rez native eight-window charge at paint/object caps, no input"]
fn measure_native_eight_window_residency_at_paint_caps() {
    let _timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(180));
        std::process::exit(124);
    });
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let app = gpui_kit::application();
    let family = gpui_kit::component::Theme::default().font_family;
    let text = app.text_system();
    app.background_executor()
        .spawn(async move {
            let peak = window_residency(&mut super::super::Measure::new(text, family));
            sender.send(peak).expect("receiver");
        })
        .detach();
    let peak = receiver
        .recv_timeout(Duration::from_secs(150))
        .expect("native windows");
    println!(
        "native_paint_cap_windows scenes=4 objects=1500 dash=512 fill=512 outline=512 labels=44 chars_per_label=400 lifetime_prefixes=256 ready_per_scene=8 consumed=256 peak_charge_bytes={peak} allowance_bytes={}; reserved and sampled frames plus one prior returned owner per scene; not maximal cells/marks/source bytes, production activation, RSS or heap peak",
        kumochi_scene::limits::MAX_RESIDENT_BYTES
    );
}

fn disposal_fixture() -> (super::Presenter, Instant) {
    let put = (0..375)
        .map(|index| format!(r#"{{"line":{{"id":"line{index}","from":[0,0],"to":[1,1]}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let source = step(&format!(r#"{{"step":{{"id":"lines","put":[{put}]}}}}"#));
    let scenes = (0..4)
        .map(|index| {
            (
                kumochi_protocol::message::SceneId(100 + index),
                source.clone(),
            )
        })
        .collect();
    let (mut presenter, now, job) = super::recovering(scenes);
    let effects = presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered cap");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.global.usage().objects, 1500);
    assert_eq!(presenter.scenes.len(), 4);
    let (sample, effects) = presenter.sample(0);
    assert!(sample.visible);
    assert!(effects.is_empty());
    (presenter, now)
}

#[test]
fn the_disposal_fixture_really_retains_1500_objects_across_four_scenes() {
    let (mut presenter, now) = disposal_fixture();
    let mut items = 0;
    assert!(!presenter.frames.borrow().paint(|frame| {
        items += frame.items().len();
        Ok::<_, std::fmt::Error>(())
    }));
    assert_eq!(items, 1500);
    let effects = presenter.clear(now).expect("global clear");
    assert_eq!(
        controls(&effects),
        [&kumochi_protocol::message::ControlToBrain::ClearDrawing {
            scene_ids: (100..104).map(kumochi_protocol::message::SceneId).collect()
        }]
    );
    assert_eq!(presenter.global.usage().objects, 0);
    assert_eq!(presenter.residency(), 0);
}

#[test]
#[ignore = "exact isolated optimized quiet-Rez clear/cut/eviction timing, no window or input"]
fn measure_clear_cut_and_eviction_at_the_global_object_cap() {
    use kumochi_protocol::message::{ControlToBrain, DataToShell, SceneId};

    let _timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(180));
        std::process::exit(124);
    });
    let mut rows = Vec::new();
    for iteration in 0..136 {
        let (mut presenter, now) = disposal_fixture();
        let start = Instant::now();
        let effects = presenter.clear(now).expect("global clear");
        let clear_ns = start.elapsed().as_nanos();
        assert_eq!(presenter.global.usage().objects, 0);
        assert_eq!(presenter.residency(), 0);
        assert_eq!(controls(&effects).len(), 1);

        let (mut presenter, now) = disposal_fixture();
        let start = Instant::now();
        let effects = presenter.stop(super::RUN, now).expect("stop all scenes");
        let stop_ns = start.elapsed().as_nanos();
        let cuts = effects
            .into_iter()
            .find_map(|effect| match effect {
                super::Effect::Control(ControlToBrain::StopScenes { cuts, .. }) => Some(cuts),
                _ => None,
            })
            .expect("one stop");
        assert_eq!(cuts.len(), 4);
        assert_eq!(
            cuts.iter().map(|cut| cut.cut.revealed.len()).sum::<usize>(),
            1500
        );
        let mut scenes = [(0, 0, 0); 4];
        let start = Instant::now();
        let mut mark = start;
        for (cut, scene) in cuts.iter().zip(&mut scenes) {
            let owned = cut.clone();
            let cloned = Instant::now();
            let id = owned.scene;
            assert!(
                controls(
                    &super::acknowledge(&mut presenter, id, 1, owned.cut, now).expect("kept cut")
                )
                .is_empty()
            );
            let acked = Instant::now();
            *scene = (
                id.0,
                (cloned - mark).as_nanos(),
                (acked - cloned).as_nanos(),
            );
            mark = acked;
        }
        let cut_ns = (mark - start).as_nanos();
        assert_eq!(presenter.global.usage().objects, 1500);

        let (mut presenter, now) = disposal_fixture();
        let frame = DataToShell::ScenesRemoved {
            scenes: (100..104).map(SceneId).collect(),
        };
        let start = Instant::now();
        let effects = super::data(&mut presenter, frame, now).expect("evicted scenes");
        let eviction_ns = start.elapsed().as_nanos();
        assert!(controls(&effects).is_empty());
        assert_eq!(presenter.global.usage().objects, 0);
        assert_eq!(presenter.residency(), 0);
        if iteration >= 8 {
            rows.push((clear_ns, stop_ns, cut_ns, eviction_ns, scenes));
        }
    }
    println!(
        "cap_disposal objects=1500 scenes=4 warmups=8 samples=128; UI reduction and deferred transfer, not destructor completion or IPC latency; cut_apply_all_ns spans every scene's owned cut clone and acknowledgement, cap_cut_scene_sample splits each scene's"
    );
    for (sample, (clear_ns, stop_ns, cut_ns, eviction_ns, scenes)) in rows.into_iter().enumerate() {
        println!(
            "cap_disposal_sample clear_ns={clear_ns} stop_ns={stop_ns} cut_apply_all_ns={cut_ns} eviction_ns={eviction_ns}"
        );
        for (scene, clone_ns, ack_ns) in scenes {
            println!(
                "cap_cut_scene_sample sample={sample} scene={scene} clone_ns={clone_ns} ack_ns={ack_ns} scene_ns={}",
                clone_ns + ack_ns
            );
        }
    }
}
