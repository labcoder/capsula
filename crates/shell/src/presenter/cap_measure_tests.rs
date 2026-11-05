use std::time::{Duration, Instant};

use gpui_kit::component::ActiveTheme as _;

use kumochi_scene::{
    Step,
    display::{MeasureError, TextMeasure, TextStyle},
};

use super::{LIVE, Shaper, begin, commit, controls, live, only_job, step};

pub(crate) fn drawing(iteration: usize, extra_labels: usize) -> Step {
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
    let source = format!(r#"{{"step":{{"id":"cap","put":[{}]}}}}"#, put.join(","));
    assert!(source.len() <= kumochi_scene::limits::MAX_RECORD_BYTES);
    step(&source)
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
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let text = std::sync::Arc::clone(cx.text_system());
            let family = cx.theme().font_family.clone();
            let worker = cx.background_executor().spawn(async move {
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
                rows
            });
            cx.spawn(async move |cx| {
                sender.send(worker.await).expect("measurement receiver");
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
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
    (presenter, now)
}

#[test]
fn the_disposal_fixture_really_retains_1500_objects_across_four_scenes() {
    let (mut presenter, now) = disposal_fixture();
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
        let start = Instant::now();
        for cut in cuts.iter().cloned() {
            let effects =
                super::acknowledge(&mut presenter, cut.scene, 1, cut.cut, now).expect("kept cut");
            assert!(controls(&effects).is_empty());
        }
        let cut_ns = start.elapsed().as_nanos();
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
            rows.push((clear_ns, stop_ns, cut_ns, eviction_ns));
        }
    }
    println!(
        "cap_disposal objects=1500 scenes=4 warmups=8 samples=128; UI reduction and deferred transfer, not destructor completion or IPC latency"
    );
    for (clear_ns, stop_ns, cut_ns, eviction_ns) in rows {
        println!(
            "cap_disposal_sample clear_ns={clear_ns} stop_ns={stop_ns} cut_apply_all_ns={cut_ns} eviction_ns={eviction_ns}"
        );
    }
}
