//! Exact default, unit and architecture-example semantics.
use capsula_scene::{
    Record, Step, parse_record,
    record::{
        Align, AxisScale, ChartKind, Color, Direction, Ease, HighlightStyle, NodeShape, Op, Start,
    },
};

fn step(json: &str) -> Step {
    let Record::Step(step) = parse_record(json.as_bytes()).expect("valid static fixture") else {
        panic!("step fixture")
    };
    step
}
fn operation(json: &str) -> Op {
    step(&format!(r#"{{"step":{{"id":"s","put":[{json}]}}}}"#)).put[0].clone()
}

#[test]
fn header_and_step_defaults_are_exact() {
    let Record::Begin(header) =
        parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"inline","alt":"description"}}"#)
            .expect("header")
    else {
        panic!("begin fixture")
    };
    assert!(
        header.scene.as_str() == "s",
        "parsed text matches the fixture"
    );
    assert!(
        header.alt.as_str() == "description",
        "parsed text matches the fixture"
    );
    assert_eq!(header.size, None);
    assert_eq!(header.title, None);
    let value = step(r#"{"step":{"id":"s"}}"#);
    assert!(value.id.as_str() == "s", "parsed text matches the fixture");
    assert_eq!(value.say, None);
    assert!(
        value.put.is_empty() && value.set.is_empty() && value.remove.is_empty(),
        "empty lists by default"
    );
    assert_eq!(value.start, Start::Speech);
    let value = step(r#"{"step":{"id":"s","start":{"after":"earlier","ms":200}}}"#);
    let Start::After { step, ms } = value.start else {
        panic!("after dependency")
    };
    assert!(
        step.as_str() == "earlier",
        "parsed text matches the fixture"
    );
    assert_eq!(ms.get(), 200);
}

#[test]
fn primitive_defaults_and_millisecond_fields_are_exact() {
    let Op::Line(line) = operation(
        r##"{"line":{"id":"l","from":[1,2],"to":[3,4],"delay":12,"dur":34,"at_word":2,"color":"#01a2ff"}}"##,
    ) else {
        panic!("line fixture")
    };
    assert_eq!(line.delay.expect("delay").get(), 12);
    assert_eq!(line.dur.expect("duration").get(), 34);
    assert_eq!(line.at_word, Some(2));
    assert_eq!(line.parent, None);
    assert_eq!(line.enter, None);
    assert_eq!(line.opacity, None);
    assert!(line.via.is_empty(), "no intermediate points");
    assert!(!line.dash, "solid line default");
    assert_eq!(line.head, None);
    assert_eq!(line.bend, None);
    let Some(Color::Hex(rgb)) = line.color else {
        panic!("hex colour")
    };
    assert_eq!(rgb.get(), 0x0001_a2ff);
    let Op::Shape(shape) = operation(r#"{"shape":{"id":"s","kind":"circle","r":7}}"#) else {
        panic!("shape fixture")
    };
    assert!(!shape.dash, "solid shape default");
    assert!(shape.points.is_empty(), "no polygon vertices by default");
    assert_eq!(shape.r.expect("radius").get().to_bits(), 7.0_f32.to_bits());
    let Op::Text(text) = operation(r#"{"text":{"id":"t","at":[0,0],"text":""}}"#) else {
        panic!("text fixture")
    };
    assert!(!text.bold, "normal weight default");
    assert_eq!(text.align, Align::Center);
    let Op::Highlight(highlight) = operation(r#"{"highlight":{"id":"h","target":[0,0]}}"#) else {
        panic!("highlight fixture")
    };
    assert_eq!(highlight.style, HighlightStyle::Ring);
    let Op::Path(path) = operation(r#"{"path":{"id":"p","d":"M0 0"}}"#) else {
        panic!("path fixture")
    };
    assert!(!path.dash, "solid path default");
    assert!(path.d.as_str() == "M0 0", "parsed text matches the fixture");
}

#[test]
fn container_and_track_defaults_are_exact() {
    let Op::Diagram(diagram) = operation(r#"{"diagram":{"id":"d"}}"#) else {
        panic!("diagram fixture")
    };
    assert_eq!(
        diagram.dir, None,
        "surface-specific direction is deferred to preparation"
    );
    let Op::Node(node) = operation(r#"{"node":{"id":"n","label":"Node"}}"#) else {
        panic!("node fixture")
    };
    assert_eq!(node.shape, NodeShape::Box);
    assert!(
        node.label.as_str() == "Node",
        "parsed text matches the fixture"
    );
    let Op::Edge(edge) =
        operation(r#"{"edge":{"id":"e","in":"d","from":"a","to":"b","flow":500}}"#)
    else {
        panic!("edge fixture")
    };
    assert!(!edge.dash, "solid edge default");
    assert_eq!(edge.flow.expect("flow").get(), 500);
    assert!(
        edge.parent.expect("diagram").as_str() == "d",
        "parsed text matches the fixture"
    );
    assert!(edge.from.as_str() == "a", "parsed text matches the fixture");
    assert!(edge.to.as_str() == "b", "parsed text matches the fixture");
    let Op::Table(table) = operation(r#"{"table":{"id":"t","cols":["x"]}}"#) else {
        panic!("table fixture")
    };
    assert!(table.align.is_empty(), "layout chooses column alignment");
    let Op::Row(row) = operation(r#"{"row":{"id":"r","cells":[]}}"#) else {
        panic!("row fixture")
    };
    assert!(!row.emphasis, "ordinary row default");
    let Op::Chart(chart) = operation(r#"{"chart":{"id":"c"}}"#) else {
        panic!("chart fixture")
    };
    assert_eq!(chart.kind, ChartKind::Bar);
    assert!(chart.categories.is_empty(), "no categories by default");
    assert!(!chart.stacked, "unstacked by default");
    assert_eq!(chart.x.scale, AxisScale::Linear);
    assert_eq!(chart.y.scale, AxisScale::Linear);
    assert_eq!(chart.x.min, None);
    assert_eq!(chart.y.max, None);
    let Op::Series(series) = operation(r#"{"series":{"id":"s"}}"#) else {
        panic!("series fixture")
    };
    assert!(series.data.is_empty(), "no data by default");
    let Op::Anim(anim) =
        operation(r#"{"anim":{"target":"s","track":"x","keys":[[250,1]],"delay":100}}"#)
    else {
        panic!("animation fixture")
    };
    assert_eq!(anim.ease, Ease::InOut);
    assert_eq!(anim.repeat, 0);
    assert!(!anim.yoyo, "no reverse play by default");
    assert_eq!(anim.delay.expect("delay").get(), 100);
    assert_eq!(anim.keys[0].0.get(), 250);
}

#[test]
fn architecture_example_parses_verbatim() {
    let example = r#"{"records":[
 {"begin":{"v":1,"scene":"tls","surface":"overlay","title":"How HTTPS starts","alt":"Browser and server exchange hello and certificate"}},
 {"step":{"id":"s1","say":"First, your browser says hello.","put":[
   {"diagram":{"id":"d","box":[40,60,880,420],"dir":"right"}},
   {"node":{"id":"browser","in":"d","label":"Browser"}}]}},
 {"step":{"id":"s2","say":"The server answers with its certificate.","put":[
   {"node":{"id":"server","in":"d","label":"Server","color":"green"}},
   {"edge":{"id":"e1","in":"d","from":"browser","to":"server","label":"ClientHello"}}]}},
 {"end":{}}
]}"#;
    let value: serde_json::Value = serde_json::from_str(example).expect("architecture JSON");
    let records: Vec<Record> = value["records"]
        .as_array()
        .expect("array")
        .iter()
        .map(|r| {
            parse_record(&serde_json::to_vec(r).expect("record encodes"))
                .expect("architecture record")
        })
        .collect();
    assert_eq!(records.len(), 4);
    let Record::Begin(begin) = &records[0] else {
        panic!("begin")
    };
    assert!(
        begin.scene.as_str() == "tls",
        "parsed text matches the fixture"
    );
    assert!(
        begin.title.as_ref().expect("title").as_str() == "How HTTPS starts",
        "parsed text matches the fixture"
    );
    let Record::Step(first) = &records[1] else {
        panic!("first step")
    };
    assert!(first.id.as_str() == "s1", "parsed text matches the fixture");
    assert!(
        first.say.as_ref().expect("say").as_str() == "First, your browser says hello.",
        "parsed text matches the fixture"
    );
    assert_eq!(first.put.len(), 2);
    let Op::Diagram(diagram) = &first.put[0] else {
        panic!("diagram")
    };
    assert_eq!(diagram.dir, Some(Direction::Right));
    assert_eq!(
        diagram.bounds.expect("box").map(|n| n.get().to_bits()),
        [40.0_f32, 60.0, 880.0, 420.0].map(f32::to_bits)
    );
    let Op::Node(browser) = &first.put[1] else {
        panic!("browser node")
    };
    assert!(
        browser.id.as_str() == "browser",
        "parsed text matches the fixture"
    );
    assert!(
        browser.parent.as_ref().expect("parent").as_str() == "d",
        "parsed text matches the fixture"
    );
    assert!(
        browser.label.as_str() == "Browser",
        "parsed text matches the fixture"
    );
    let Record::Step(second) = &records[2] else {
        panic!("second step")
    };
    assert!(
        second.id.as_str() == "s2",
        "parsed text matches the fixture"
    );
    assert_eq!(second.put.len(), 2);
    let Op::Node(server) = &second.put[0] else {
        panic!("server node")
    };
    assert!(
        server.id.as_str() == "server",
        "parsed text matches the fixture"
    );
    assert!(
        server.label.as_str() == "Server",
        "parsed text matches the fixture"
    );
    let Op::Edge(edge) = &second.put[1] else {
        panic!("edge")
    };
    assert!(
        edge.from.as_str() == "browser",
        "parsed text matches the fixture"
    );
    assert!(
        edge.to.as_str() == "server",
        "parsed text matches the fixture"
    );
    assert!(
        edge.label.as_ref().expect("label").as_str() == "ClientHello",
        "parsed text matches the fixture"
    );
    assert!(matches!(records[3], Record::End(_)), "end record");
}
