use kumochi_scene::record::HighlightStyle;
use kumochi_scene::screen::PLACEHOLDER_BOX;

use super::picture::{Canvas, layout, shrink, text_width};
use super::{
    BUDGET, DISPLAY, LAYOUTS, NAMED, Verdict, account_refusal, assign, build, check, draw_tool,
    limit, mark_line, picture_line, placeholders, plan, truths,
};
use crate::connections::model::error::{Code, ProviderError};
use crate::context::{InputItem, Jpeg, Role};

/// The marking paragraph's own example, as the model would send it.
const EXAMPLE: &str = r#"{"records":[
 {"begin":{"scene":"reply-box","surface":"screen","alt":"The message box and the line it answers"}},
 {"step":{"id":"box","say":"This is where your draft is.","put":[
   {"highlight":{"id":"draft","target":{"box":[186,598,450,78]},"style":"box"}}]}},
 {"step":{"id":"line","say":"And this is the line it replies to.","put":[
   {"highlight":{"id":"ask","target":{"box":[201,561,438,12]},"style":"underline"}},
   {"callout":{"id":"why","target":{"box":[201,561,438,12]},"text":"the question","side":"top"}}]}},
 {"end":{}}]}"#;

#[test]
fn the_plan_sends_each_size_once_first_then_every_request_once() {
    let plan = plan();
    assert_eq!(plan.len(), 36);
    for (size, first) in plan[..3].iter().enumerate() {
        assert_eq!((first.layout, first.size, first.version), (0, size, 0));
    }
    let mut seen: Vec<_> = plan.iter().map(|p| (p.layout, p.size, p.version)).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 36);
    assert!(
        plan.iter()
            .all(|p| p.layout < LAYOUTS && p.size < 3 && p.version < 2)
    );
}

#[test]
fn the_budget_is_the_owner_s_unless_the_variable_lowers_it() {
    assert_eq!(BUDGET, 45);
    for (value, expected) in [
        (None, 45),
        (Some("0"), 0),
        (Some(" 12 "), 12),
        (Some("45"), 45),
        (Some("46"), 45),
        (Some("-1"), 0),
        (Some("many"), 0),
        (Some(""), 0),
    ] {
        assert_eq!(limit(value.map(str::to_owned)), expected, "{value:?}");
    }
}

#[test]
fn drawn_text_fills_exactly_its_known_box() {
    for (height, width) in [(11, 65), (12, 71), (13, 77), (14, 82), (15, 88), (16, 94)] {
        let mut canvas = Canvas::new([400, 40], [0, 0, 0]);
        canvas.text(7, 5, height, "SAVE 42", [255, 255, 255]);
        let lit: Vec<(u32, u32)> = (0..40)
            .flat_map(|y| (0..400).map(move |x| (x, y)))
            .filter(|&(x, y)| canvas.rgb[usize::try_from((y * 400 + x) * 3).expect("small")] == 255)
            .collect();
        let left = lit.iter().map(|p| p.0).min().expect("ink");
        let right = lit.iter().map(|p| p.0).max().expect("ink");
        let top = lit.iter().map(|p| p.1).min().expect("ink");
        let bottom = lit.iter().map(|p| p.1).max().expect("ink");
        assert_eq!(
            [left, top, right - left + 1, bottom - top + 1],
            [7, 5, width, height],
            "height {height}"
        );
        assert_eq!(text_width("SAVE 42", height), width);
    }
    assert_eq!(text_width("A", 7), 5);
    assert_eq!(text_width("AB", 14), 22);
}

#[test]
fn every_layout_fits_the_display_and_names_five_different_things() {
    for k in 0..LAYOUTS {
        let things = layout(k);
        assert_eq!(things.len(), 10);
        for thing in &things {
            let [x, y, w, h] = thing.rect;
            assert!(w > 0 && h > 0 && x + w <= DISPLAY.size[0] && y + h <= DISPLAY.size[1]);
        }
        for named in NAMED {
            let mut at = named.to_vec();
            at.sort_unstable();
            at.dedup();
            assert_eq!(at.len(), 5);
        }
    }
}

#[test]
fn shrinking_averages_whole_areas() {
    let rgb = [0, 0, 0, 100, 100, 100, 200, 200, 200, 255, 255, 255];
    assert_eq!(shrink(&rgb, [4, 1], [2, 1]), [50, 50, 50, 228, 228, 228]);
    assert_eq!(shrink(&rgb, [4, 1], [1, 1]), [139, 139, 139]);
    assert_eq!(shrink(&rgb, [4, 1], [4, 1]), rgb);
}

#[test]
fn the_example_call_is_accepted_with_its_two_highlights() {
    assert!(
        check(EXAMPLE, [1280, 720])
            == Verdict::Accepted(vec![
                (HighlightStyle::Box, [186.0, 598.0, 450.0, 78.0]),
                (HighlightStyle::Underline, [201.0, 561.0, 438.0, 12.0]),
            ])
    );
}

#[test]
fn a_refused_call_names_its_code_and_field() {
    let sized = EXAMPLE.replace(
        r#""surface":"screen","#,
        r#""surface":"screen","size":[1600,900],"#,
    );
    assert!(check(&sized, [1280, 720]) == Verdict::Refused("SizeNotPicture".into(), "size".into()));
    assert!(
        check(EXAMPLE, [1280, 640])
            == Verdict::Refused("OutOfRange".into(), "put[0].target".into())
    );
    let overlay = EXAMPLE.replace(r#""surface":"screen""#, r#""surface":"overlay""#);
    assert!(
        check(&overlay, [1280, 720]) == Verdict::Refused("Unsupported".into(), "surface".into())
    );
}

#[test]
fn a_call_that_isn_t_a_scene_is_malformed() {
    for arguments in [
        "",
        "not json",
        r#"{"records":"x"}"#,
        r#"{"records":[]}"#,
        r#"{"records":[{"end":{}}]}"#,
        r#"{"other":[]}"#,
    ] {
        assert!(
            check(arguments, [1280, 720]) == Verdict::Malformed,
            "{arguments}"
        );
    }
}

#[test]
fn the_replayed_call_keeps_no_box() {
    let stored = placeholders(EXAMPLE).expect("the example is a scene");
    for number in ["186", "598", "450", "201", "561", "438"] {
        assert!(!stored.contains(number), "{number}");
    }
    let Verdict::Accepted(marks) = check(&stored, [1280, 720]) else {
        panic!("the placeholder call is a scene");
    };
    assert_eq!(marks.len(), 2);
    assert!(marks.iter().all(|(_, mark)| *mark == PLACEHOLDER_BOX));
}

#[test]
fn a_mark_line_gives_the_paired_thing_s_place_and_signed_edges() {
    let things = layout(0);
    assert_eq!(things[0].rect, [1500, 24, 180, 44]);
    assert_eq!(
        truths(&[&things[0]], [1280, 720]),
        (vec![[750.0, 12.0, 90.0, 22.0]], 0.5)
    );
    let paired = Some((1, &things[0], [750.0, 12.0, 90.0, 22.0]));
    assert_eq!(
        mark_line(
            3,
            1,
            (HighlightStyle::Box, [748.0, 12.0, 92.0, 22.0]),
            paired,
            0.5
        ),
        "mark=3.1 thing=2 kind=button style=box left=-2.0 top=+0.0 right=+0.0 bottom=+0.0 \
         left_display=-4.0 top_display=+0.0 right_display=+0.0 bottom_display=+0.0 overlap=0.98"
    );
    assert_eq!(
        mark_line(
            3,
            2,
            (HighlightStyle::Ring, [0.0, 0.0, 5.0, 5.0]),
            None,
            0.5
        ),
        "mark=3.2 thing=none style=ring"
    );
}

#[test]
fn marks_pair_with_named_things_one_to_one_and_count_a_half_overlap_as_found() {
    let truths = [[0.0, 0.0, 10.0, 10.0], [20.0, 0.0, 10.0, 10.0]];
    let marks = [
        (HighlightStyle::Box, [4.0, 0.0, 20.0, 10.0]),
        (HighlightStyle::Ring, [0.0, 0.0, 10.0, 10.0]),
        (HighlightStyle::Box, [50.0, 50.0, 5.0, 5.0]),
    ];
    assert_eq!(assign(&marks, &truths), (vec![Some(1), Some(0), None], 1));
    let half = [(HighlightStyle::Box, [0.0, 0.0, 10.0, 20.0])];
    assert_eq!(assign(&half, &truths[..1]), (vec![Some(0)], 1));
    let under = [(HighlightStyle::Box, [0.0, 0.0, 10.0, 21.0])];
    assert_eq!(assign(&under, &truths[..1]), (vec![Some(0)], 0));
    assert_eq!(assign(&[], &truths), (vec![], 0));
}

#[test]
fn only_a_refusal_of_the_account_stops_the_run() {
    for (status, code, stops) in [
        (429, None, true),
        (401, None, true),
        (403, None, true),
        (400, Some(Code::UsageLimit), true),
        (200, Some(Code::Unavailable), true),
        (400, Some(Code::NotEligible), true),
        (400, Some(Code::RouteRefused), true),
        (400, Some(Code::InvalidUser), true),
        (200, Some(Code::NotAuthorized), true),
        (400, Some(Code::Unsupported), false),
        (400, Some(Code::Other), false),
        (400, None, false),
        (413, None, false),
        (500, None, false),
    ] {
        let error = ProviderError {
            status,
            code,
            param: None,
        };
        assert_eq!(account_refusal(error), stops, "{status} {code:?}");
    }
}

#[test]
fn a_request_goes_with_high_detail_low_effort_and_the_picture_s_size() {
    let jpeg = super::picture::jpeg(&[128; 16 * 9 * 3], [16, 9]).expect("encoded");
    let question = InputItem::Message {
        role: Role::User,
        text: "question".to_owned(),
    };
    let built = build(
        vec![question],
        Some((Jpeg::new(jpeg).expect("a JPEG"), [16, 9])),
    )
    .expect("built");
    assert_eq!(
        (built.effort.as_str(), built.detail.as_str()),
        ("low", "high")
    );
    assert!(picture_line([16, 9]).contains(", 16 by 9 pixels, "));
}

#[test]
fn the_draw_tool_offers_the_screen_and_the_marking_paragraph() {
    let tool = draw_tool();
    assert_eq!(tool.name, "draw");
    assert!(
        tool.description
            .starts_with(include_str!("../../../prompts/draw.md"))
    );
    assert!(tool.description.contains("\n\nMarking the user's screen. "));
    assert_eq!(
        tool.parameters["$defs"]["Surface"],
        serde_json::json!({"type": "string", "enum": ["overlay", "screen"]})
    );
    assert!(
        tool.parameters["$defs"]["Target"]["anyOf"]
            .as_array()
            .expect("targets")
            .iter()
            .any(|target| !target["properties"]["box"].is_null())
    );
}
