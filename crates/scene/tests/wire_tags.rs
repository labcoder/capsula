//! Golden postcard discriminants: declaration order is append only.
use capsula_scene::Record;
use capsula_scene::record::{
    Align, Anchor, AxisScale, Cell, ChartKind, Color, Direction, Ease, Enter, Head, HighlightStyle,
    KeyValue, NodeShape, Op, Point, ShapeKind, Side, Start, Surface, Token, Track, Version,
};
use serde::{Serialize, de::DeserializeOwned};

fn golden<T: Serialize + DeserializeOwned + PartialEq>(json: &str, expected: &[u8]) {
    let value: T = serde_json::from_str(json).expect("valid static fixture");
    let bytes = postcard::to_allocvec(&value).expect("fixture encodes");
    assert!(bytes == expected, "golden wire bytes are stable");
    let decoded: T = postcard::from_bytes(expected).expect("golden bytes decode");
    assert!(decoded == value, "golden bytes decode to the exact fixture");
}
fn tags<T: Serialize + DeserializeOwned + PartialEq>(cases: &[(&str, u8)]) {
    for (name, tag) in cases {
        golden::<T>(&format!("{name:?}"), &[*tag]);
    }
}
#[test]
fn vocabulary_tags_are_append_only() {
    tags::<Surface>(&[("overlay", 0), ("inline", 1)]);
    tags::<Enter>(&[
        ("none", 0),
        ("fade", 1),
        ("draw", 2),
        ("pop", 3),
        ("slide", 4),
        ("grow", 5),
        ("fly", 6),
    ]);
    tags::<Head>(&[("none", 0), ("end", 1), ("start", 2), ("both", 3)]);
    tags::<ShapeKind>(&[("rect", 0), ("ellipse", 1), ("circle", 2), ("polygon", 3)]);
    tags::<Align>(&[("left", 0), ("center", 1), ("right", 2)]);
    tags::<Side>(&[
        ("center", 0),
        ("top", 1),
        ("bottom", 2),
        ("left", 3),
        ("right", 4),
        ("top-left", 5),
        ("top-right", 6),
        ("bottom-left", 7),
        ("bottom-right", 8),
    ]);
    tags::<HighlightStyle>(&[
        ("ring", 0),
        ("box", 1),
        ("spotlight", 2),
        ("underline", 3),
        ("marker", 4),
    ]);
    tags::<Direction>(&[("right", 0), ("down", 1)]);
    tags::<NodeShape>(&[
        ("box", 0),
        ("round", 1),
        ("pill", 2),
        ("diamond", 3),
        ("circle", 4),
        ("cylinder", 5),
        ("note", 6),
    ]);
    tags::<ChartKind>(&[
        ("bar", 0),
        ("line", 1),
        ("area", 2),
        ("scatter", 3),
        ("pie", 4),
    ]);
    tags::<AxisScale>(&[("linear", 0), ("log", 1)]);
    tags::<Track>(&[
        ("x", 0),
        ("y", 1),
        ("pos", 2),
        ("opacity", 3),
        ("scale", 4),
        ("rotate", 5),
        ("draw", 6),
        ("color", 7),
    ]);
    tags::<Ease>(&[
        ("linear", 0),
        ("in", 1),
        ("out", 2),
        ("inout", 3),
        ("spring", 4),
    ]);
    tags::<Token>(&[
        ("blue", 0),
        ("green", 1),
        ("red", 2),
        ("orange", 3),
        ("amber", 4),
        ("purple", 5),
        ("teal", 6),
        ("pink", 7),
        ("accent", 8),
        ("surface", 9),
        ("fg", 10),
        ("muted", 11),
        ("gray", 12),
        ("white", 13),
        ("black", 14),
    ]);
}
#[test]
fn mixed_syntax_tags_are_append_only() {
    golden::<Version>("1", &[1]);
    golden::<Start>(r#""speech""#, &[0]);
    golden::<Start>(r#""now""#, &[1]);
    golden::<Start>(r#"{"after":"s","ms":1}"#, &[2, 1, b's', 1]);
    golden::<Anchor>("[0,0]", &[0; 9]);
    golden::<Anchor>(r#"{"ref":"n"}"#, &[1, 1, b'n']);
    golden::<Color>(r#""blue""#, &[0, 0]);
    golden::<Color>(r##""#000000""##, &[1, 0]);
    golden::<Color>(r##""#ffffff""##, &[1, 255, 255, 255, 7]);
    golden::<Cell>(r#""""#, &[0, 0]);
    golden::<Cell>("0", &[1, 0, 0, 0, 0, 0, 0, 0, 0]);
    golden::<Cell>("true", &[2, 1]);
    golden::<Point>("0", &[0; 9]);
    golden::<Point>(
        "[0,0]",
        &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    golden::<KeyValue>("0", &[0; 5]);
    golden::<KeyValue>("[0,0]", &[1, 0, 0, 0, 0, 0, 0, 0, 0]);
    golden::<KeyValue>(r#""blue""#, &[2, 0, 0]);
}
fn tag<T: Serialize + DeserializeOwned + PartialEq>(json: &str, expected: u8) {
    let value: T = serde_json::from_str(json).expect("valid static fixture");
    let bytes = postcard::to_allocvec(&value).expect("fixture encodes");
    assert_eq!(bytes.first(), Some(&expected), "golden discriminant");
    let decoded: T = postcard::from_bytes(&bytes).expect("wire fixture decodes");
    assert!(decoded == value, "tagged payload has the same value");
}
#[test]
fn record_and_operation_tags_are_append_only() {
    tag::<Record>(
        r#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":""}}"#,
        0,
    );
    tag::<Record>(r#"{"step":{"id":"s"}}"#, 1);
    golden::<Record>(r#"{"end":{}}"#, &[2]);
    tag::<Op>(r#"{"line":{"id":"l","from":[0,0],"to":[0,0]}}"#, 0);
    tag::<Op>(r#"{"arrow":{"id":"a","from":[0,0],"to":[0,0]}}"#, 1);
    tag::<Op>(r#"{"path":{"id":"p","d":""}}"#, 2);
    tag::<Op>(r#"{"shape":{"id":"s","kind":"rect"}}"#, 3);
    tag::<Op>(r#"{"text":{"id":"t","at":[0,0],"text":""}}"#, 4);
    tag::<Op>(r#"{"callout":{"id":"c","target":[0,0],"text":""}}"#, 5);
    tag::<Op>(r#"{"highlight":{"id":"h","target":[0,0]}}"#, 6);
    tag::<Op>(r#"{"diagram":{"id":"d"}}"#, 7);
    tag::<Op>(r#"{"node":{"id":"n","label":""}}"#, 8);
    tag::<Op>(r#"{"edge":{"id":"e","from":"a","to":"b"}}"#, 9);
    tag::<Op>(r#"{"table":{"id":"t","cols":[]}}"#, 10);
    tag::<Op>(r#"{"row":{"id":"r","cells":[]}}"#, 11);
    tag::<Op>(r#"{"chart":{"id":"c"}}"#, 12);
    tag::<Op>(r#"{"series":{"id":"s"}}"#, 13);
    tag::<Op>(r#"{"anim":{"target":"a","track":"x","keys":[]}}"#, 14);
    tag::<Op>(r#"{"mermaid":{"in":"d","src":""}}"#, 15);
}
