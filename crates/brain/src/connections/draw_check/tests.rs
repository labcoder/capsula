use super::{BUDGET, PROMPTS, Reply, Verdict, check, limit, line};

const RECORDED: [&str; 3] = [
    include_str!("../../../fixtures/responses/draw_water_cycle.json"),
    include_str!("../../../fixtures/responses/draw_y_squared_1.json"),
    include_str!("../../../fixtures/responses/draw_y_squared_2.json"),
];

#[test]
fn the_budget_is_one_request_per_prompt() {
    assert_eq!(usize::try_from(BUDGET), Ok(PROMPTS.len()));
}

#[test]
fn the_variable_can_only_lower_the_budget_of_two() {
    for (value, want) in [
        (None, 2),
        (Some("2"), 2),
        (Some(" 1 "), 1),
        (Some("0"), 0),
        (Some("3"), 2),
        (Some("4294967296"), 0),
        (Some("-1"), 0),
        (Some("x"), 0),
        (Some(""), 0),
    ] {
        assert_eq!(limit(value.map(str::to_owned)), want, "{value:?}");
    }
}

#[test]
fn the_recorded_calls_draw_and_each_refusal_names_only_its_code_and_field() {
    for recorded in RECORDED {
        assert_eq!(check(recorded.as_bytes()), Verdict::Drawn(1));
    }
    let refused = |code: &str, field: &str| Verdict::Refused(code.to_owned(), field.to_owned());
    for (arguments, want) in [
        (
            r#"{"records":[{"begin":{"scene":"s","surface":"inline","title":"t","alt":"a"}}]}"#,
            refused("scene.Unsupported", "surface"),
        ),
        (
            r#"{"records":[{"begin":{"scene":"s","surface":"screen","title":"t","alt":"a"}}]}"#,
            refused("drawing.NoScreenPicture", "surface"),
        ),
        (
            r#"{"records":[{"step":{"id":"a","put":[]}}]}"#,
            refused("scene.BeforeBegin", "$"),
        ),
        (
            r#"{"records":[{"begin":{"scene":"s","surface":"overlay","title":"t","alt":"a"}},{"begin":{"scene":"t","surface":"overlay","title":"t","alt":"a"}}]}"#,
            refused("check.SecondBegin", "begin"),
        ),
        (r#"{"records":[]}"#, refused("scene.BeforeBegin", "$")),
    ] {
        assert_eq!(check(arguments.as_bytes()), want, "{arguments}");
    }
}

#[test]
fn a_refused_or_truncated_call_prints_no_model_text() {
    let marker = "zqxdrawchecksentinel";
    for arguments in [
        format!(r#"{{"records":[{{"begin":{{"scene":"{marker}","surface":"{marker}"}}}}]}}"#),
        format!(
            r#"{{"records":[{{"begin":{{"scene":"{marker}","surface":"overlay","title":"{marker}"#
        ),
        format!(
            r#"{{"records":[{{"begin":{{"scene":"{marker}","surface":"overlay"}}}},{{"step":{{"id":"{marker}","put":[{{"line":{{"id":"{marker}","from":["{marker}"]}}}}]}}}}]}}"#
        ),
    ] {
        let reply = Reply {
            calls: 1,
            draw: Some(Ok(arguments.clone())),
        };
        let (text, drawn) = line(1, "water-cycle", 10, Ok(()), &reply, 5);
        assert!(!drawn, "{text}");
        assert!(text.contains(" draw=refused error="), "{text}");
        assert!(!text.contains(marker), "{text}");
    }
}
