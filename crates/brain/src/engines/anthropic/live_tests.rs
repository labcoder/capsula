use super::round_results;
use crate::context::InputItem;
use crate::engines::anthropic::encode::ToolUse;
use crate::engines::{CallId, Opaque, TurnEvent};

fn call(id: &str) -> CallId {
    CallId::new(id).expect("short")
}

fn made(id: &str) -> InputItem {
    let item = format!(r#"{{"type":"tool_use","id":"{id}","name":"list_tasks","input":{{}}}}"#);
    InputItem::Native(Opaque::new(item.into_bytes()).expect("small"))
}

fn answer(id: &str, output: &str) -> InputItem {
    InputItem::FunctionOutput {
        call: call(id),
        output: output.into(),
    }
}

#[test]
fn a_round_takes_only_the_results_that_follow_its_own_calls_one_each() {
    let calls = [call("a"), call("b")];
    let paired = Some(vec![
        (call("a"), "A".to_owned()),
        (call("b"), "B".to_owned()),
    ]);
    for (input, expected) in [
        (
            vec![made("a"), made("b"), answer("b", "B"), answer("a", "A")],
            paired.clone(),
        ),
        (
            vec![
                answer("a", "OLD"),
                made("a"),
                made("b"),
                answer("a", "A"),
                answer("b", "B"),
            ],
            paired,
        ),
        (
            vec![made("a"), made("b"), answer("a", "A"), answer("a", "A")],
            None,
        ),
        (vec![made("a"), made("b"), answer("a", "A")], None),
        (
            vec![
                made("a"),
                made("b"),
                answer("a", "A"),
                answer("b", "B"),
                answer("c", "C"),
            ],
            None,
        ),
        (
            vec![made("b"), made("a"), answer("a", "A"), answer("b", "B")],
            None,
        ),
        (vec![made("a"), answer("a", "A"), answer("b", "B")], None),
        (vec![answer("a", "A"), answer("b", "B")], None),
    ] {
        assert_eq!(round_results(&input, &calls), expected, "{input:?}");
    }
}

#[test]
fn a_key_call_formats_without_its_input() {
    let call = ToolUse::stored(
        br#"{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{"q":"zqxmarker"}}"#,
    )
    .expect("a call");
    let shown = format!("{:?}", TurnEvent::KeyCall { index: 0, call });
    assert!(!shown.contains("zqxmarker"), "{shown}");
    assert!(!format!("{:?}", shown.as_bytes()).contains("zqxmarker"));
    assert_eq!(
        shown,
        "KeyCall { index: 0, call: ToolUse { input_bytes: 17, .. } }"
    );
}
