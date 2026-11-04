//! The residency workload must be legal before the native shaper is involved.

#[path = "support/maximal_content.rs"]
mod maximal_content;

use kumochi_scene::{Admission, GlobalBudget, Record, SceneState, parse_record};

#[test]
fn native_content_fixtures_reach_the_exact_joint_caps() {
    for content in [
        maximal_content::Content::UnicodeBytes,
        maximal_content::Content::AsciiCells,
    ] {
        let mut budget = GlobalBudget::default();
        let mut states = Vec::new();
        for scene in 0..4 {
            let (_, steps) = maximal_content::fixture(content, scene);
            assert_eq!(steps.len(), 64);
            let Ok(Record::Begin(begin)) = parse_record(
                br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"diagram"}}"#,
            ) else {
                panic!("begin");
            };
            let mut state = SceneState::begin(begin).expect("scene");
            for step in steps {
                let Admission::New(candidate) = state.admit(&step, &budget).expect("legal content")
                else {
                    panic!("new step");
                };
                state.commit(candidate, &mut budget).expect("commit");
            }
            states.push(state);
        }
        let usage = budget.usage();
        assert_eq!(
            (usage.scenes, usage.objects, usage.cells, usage.marks),
            (4, 1500, 4000, 20000)
        );
    }
}
