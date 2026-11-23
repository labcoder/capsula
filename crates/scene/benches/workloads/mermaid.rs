use crate::{
    mermaid_fixture::{connected_full_graph, graph, scene},
    scene_support::Measure,
};
use kumochi_scene::{
    Admission, GlobalBudget,
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    theme::Theme,
};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) fn measure_mermaid_admission_and_prepare() {
    let maximum = connected_full_graph();
    for (name, source, count) in [
        (
            "branching",
            include_str!("../../tests/fixtures/mermaid/gates/branching.mmd"),
            14,
        ),
        (
            "long_label_cycle",
            include_str!("../../tests/fixtures/mermaid/gates/long-label-cycle.mmd"),
            12,
        ),
        (
            "nested_cross_group",
            include_str!("../../tests/fixtures/mermaid/gates/nested-cross-group.mmd"),
            16,
        ),
        (
            "orientations",
            include_str!("../../tests/fixtures/mermaid/gates/orientations.mmd"),
            10,
        ),
        (
            "labelled_edges",
            include_str!("../../tests/fixtures/mermaid/gates/labelled-edges.mmd"),
            12,
        ),
        ("connected600", maximum.as_str(), 600),
    ] {
        let step = graph(source);
        for iteration in 0..69 {
            let mut scene = scene();
            let mut budget = GlobalBudget::default();
            let before = LayoutState::new(&scene);
            let start = std::time::Instant::now();
            let admitted = scene.admit(&step, &budget).expect("admitted source");
            let admission_ns = start.elapsed().as_nanos();
            let Admission::New(candidate) = admitted else {
                panic!("fresh source");
            };
            let committed = scene.commit(candidate, &mut budget).expect("commit");
            let input = PrepareInput::new(
                committed.preparation,
                before.clone(),
                SurfaceGeometry::default(),
                Arc::new(Theme::default()),
            )
            .expect("input");
            let cancelled = AtomicBool::new(false);
            let start = std::time::Instant::now();
            let prepared = prepare(input, &mut Measure, &cancelled).expect("prepared source");
            let prepare_ns = start.elapsed().as_nanos();
            prepared.validate(&scene, &before).expect("current source");
            assert_eq!(
                scene.object_count(),
                count,
                "{name}: expected workload size"
            );
            if iteration >= 5 {
                println!(
                    "MERMAID name={name} sample={} admit_ns={admission_ns} prepare_ns={prepare_ns} objects={}",
                    iteration - 5,
                    scene.object_count()
                );
            }
        }
    }
}
