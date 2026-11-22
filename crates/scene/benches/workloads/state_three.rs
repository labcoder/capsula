use crate::state_three_fixture::{commit, compact, cube, lathe, put, scene, step, view};
use kumochi_scene::{Budget, ErrorCode, GlobalBudget};

fn polygon(name: &str) -> String {
    let points: Vec<String> = (0..64)
        .map(|i| {
            let angle = f64::from(i) * std::f64::consts::TAU / 64.0;
            format!("[{:.0},{:.0}]", 1000.0 * angle.cos(), 1000.0 * angle.sin())
        })
        .collect();
    format!(
        r#"{{"solid":{{"in":"v","id":"{name}","shape":"extrude","points":[{}],"depth":2}}}}"#,
        points.join(",")
    )
}

pub(super) fn measure_admit() {
    use kumochi_scene::limits::MAX_STEP_OUTLINE_POINTS;
    use std::{hint::black_box, time::Instant};

    let extrudes = |outline: fn(&str) -> String, first: Option<String>| {
        let mut ops: Vec<String> = first.into_iter().collect();
        let mut count = 0;
        loop {
            ops.push(outline(&format!("e{count}")));
            if count == MAX_STEP_OUTLINE_POINTS / 64
                || format!(r#"{{"step":{}}}"#, put("a", &ops)).len() > 32 * 1024
            {
                ops.pop();
                return (count, step(&put("a", &ops)));
            }
            count += 1;
        }
    };
    let (count, largest) = extrudes(polygon, Some(view("v")));
    let (_, late) = extrudes(polygon, None);
    let (compact_count, compact_largest) = extrudes(compact, Some(view("v")));
    let (_, compact_late) = extrudes(compact, None);
    let mut full = (scene(), GlobalBudget::default());
    commit(
        &mut full.0,
        &mut full.1,
        &put("f", &[view("v"), lathe("l", 251, 48)]),
    );
    let mut boxes = vec![view("v")];
    boxes.extend((0..160).map(|i| cube(&format!("s{i}"), "")));
    let empty = (scene(), GlobalBudget::default());
    println!(
        "largest legal steps: {count} round and {compact_count} compact extrudes of 64 points"
    );
    for (workload, (state, global), step, refused) in [
        ("largest-legal-extrude-step", &empty, largest, None),
        (
            "extrude-step-refused-late",
            &full,
            late,
            Some(ErrorCode::OverBudget(Budget::Triangles)),
        ),
        (
            "largest-compact-extrude-step",
            &empty,
            compact_largest,
            None,
        ),
        (
            "compact-step-refused-late",
            &full,
            compact_late,
            Some(ErrorCode::OverBudget(Budget::Triangles)),
        ),
        ("160-small-solids", &empty, step(&put("a", &boxes)), None),
    ] {
        assert_eq!(
            state.admit(&step, global).err().map(|error| error.code),
            refused,
            "{workload}"
        );
        let mut samples: Vec<f64> = (0..200)
            .map(|_| {
                let start = Instant::now();
                drop(black_box(state.admit(black_box(&step), global)));
                start.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        println!(
            "{workload}: n=200 p50={:.1}us p99={:.1}us max={:.1}us",
            samples[100], samples[197], samples[199]
        );
    }
}
