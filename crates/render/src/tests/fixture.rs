//! Views built through the scene crate's own admission, preparation under the presenter's
//! paint cap and activation from step records, shared by the tests and the benchmark.

use std::{
    fmt::Write as _,
    sync::{Arc, atomic::AtomicBool},
};

use kumochi_scene::{
    Admission, Frame, GlobalBudget, Record, SceneState, SceneView,
    display::{MeasureError, PaintWork, TextMeasure, TextStyle},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    motion::synthetic_duration_ms,
    parse_record,
    theme::Theme,
};

/// A time after every entrance and track has settled.
pub(crate) const SETTLED: u64 = 600_000;

struct Measure;

impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let width = f32::from(u16::try_from(text.chars().count()).map_err(|_| MeasureError)?) * 8.0;
        Ok([max_width.map_or(width, |limit| width.min(limit)), 10.0])
    }
}

/// An overlay scene named `name` with every step of `steps` committed and activated at 0.
pub(crate) fn view(name: &str, steps: &[String], budget: &mut GlobalBudget) -> SceneView {
    let begin = format!(r#"{{"begin":{{"v":1,"scene":"{name}","surface":"overlay","alt":""}}}}"#);
    let Ok(Record::Begin(begin)) = parse_record(begin.as_bytes()) else {
        panic!("begin fixture");
    };
    let mut scene = SceneState::begin(begin).expect("fixture scene");
    let mut view = SceneView::new(&LayoutState::new(&scene), 0);
    for body in steps {
        let Ok(Record::Step(step)) = parse_record(format!(r#"{{"step":{body}}}"#).as_bytes())
        else {
            panic!("step fixture");
        };
        let Admission::New(candidate) = scene.admit(&step, budget).expect("admitted fixture")
        else {
            panic!("new fixture step");
        };
        let committed = scene.commit(candidate, budget).expect("committed fixture");
        let input = PrepareInput::new(
            committed.preparation,
            view.layout().clone(),
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("consecutive preparation")
        .with_paint_cap(Some(PaintWork::LIMITS));
        let prepared = prepare(input, &mut Measure, &AtomicBool::new(false)).expect("prepared");
        let segment = synthetic_duration_ms(prepared.step().say.as_ref());
        view.activate(prepared, 0, segment, &mut Frame::new())
            .expect("fresh activation");
    }
    view
}

/// `view` sampled once it has settled.
pub(crate) fn sample(view: &SceneView) -> Frame {
    let mut frame = Frame::new();
    view.frame(SETTLED, &mut frame).expect("fresh frame");
    frame
}

/// `count` small stroked squares, 150 to a step: four segments and no fill vertex each.
pub(crate) fn squares(count: usize) -> Vec<String> {
    (0..count)
        .step_by(150)
        .map(|start| {
            let put: Vec<String> = (start..(start + 150).min(count))
                .map(|n| {
                    let (x, y) = (n % 30 * 10, n / 30 * 10);
                    format!(
                        r#"{{"shape":{{"id":"s{n}","kind":"polygon","points":[[{x},{y}],[{},{y}],[{},{}],[{x},{}]]}}}}"#,
                        x + 8,
                        x + 8,
                        y + 8,
                        y + 8
                    )
                })
                .collect();
            format!(r#"{{"id":"squares{start}","put":[{}]}}"#, put.join(","))
        })
        .collect()
}

/// Fills at the fill-vertex cap: a 512-point comb of 255 teeth, the most spans one fill can
/// have, and a {512/3} star, whose 512 edges each cross four others, nonzero: 512 + 512 + 1,024.
pub(crate) fn fills() -> String {
    let mut comb = String::from("[0,0],[1020,0]");
    for at in 0..510_u16 {
        let y = if at % 2 == 0 { 100 } else { 10 };
        write!(comb, ",[{},{y}]", 1020 - at * 2).expect("writing to a string");
    }
    let mut star = String::new();
    for at in 0..512_u32 {
        let angle = f64::from(at * 3 % 512) * std::f64::consts::TAU / 512.0;
        let (x, y) = (1500.0 + 250.0 * angle.cos(), 300.0 + 250.0 * angle.sin());
        let comma = if at == 0 { "" } else { "," };
        write!(star, "{comma}[{x:.3},{y:.3}]").expect("writing to a string");
    }
    format!(
        r#"{{"id":"fills","put":[{{"shape":{{"id":"comb","kind":"polygon","fill":"green","width":0,"points":[{comb}]}}}},{{"shape":{{"id":"star","kind":"polygon","fill":"green","width":0,"points":[{star}]}}}}]}}"#
    )
}

/// Geometry at the global paint caps: 2,048 fill points in four 512-point polygons, 756
/// outline points in 36 bordered boxes, and 19,955 of the 20,000 path segments. It doesn't reach
/// the dash cap.
pub(crate) fn at_limits() -> Vec<String> {
    let mut steps = Vec::new();
    let fills: Vec<String> = (0..4)
        .map(|n| {
            let mut points = String::new();
            for at in 0..512_u16 {
                let angle = f64::from(at) * std::f64::consts::TAU / 512.0;
                let (x, y) = (300.0 + 250.0 * angle.cos(), 300.0 + 250.0 * angle.sin());
                let comma = if at == 0 { "" } else { "," };
                write!(points, "{comma}[{x:.2},{y:.2}]").expect("writing to a string");
            }
            format!(
                r#"{{"shape":{{"id":"f{n}","kind":"polygon","fill":"green","width":0,"points":[{points}]}}}}"#
            )
        })
        .collect();
    steps.push(format!(r#"{{"id":"fills","put":[{}]}}"#, fills.join(",")));
    for chunk in 0..11 {
        let lines: Vec<String> = (chunk * 3..chunk * 3 + 3)
            .map(|n| {
                let mut via = String::new();
                for at in 0..510_u16 {
                    let x = f64::from(at + 1) * 2.0;
                    let y = f64::from(n) * 12.0 + f64::from(at % 2) * 6.0;
                    let comma = if at == 0 { "" } else { "," };
                    write!(via, "{comma}[{x:.1},{y:.1}]").expect("writing to a string");
                }
                format!(
                    r#"{{"line":{{"id":"l{n}","from":[0,{}],"to":[1100,{}],"via":[{via}]}}}}"#,
                    n * 12,
                    n * 12
                )
            })
            .collect();
        steps.push(format!(
            r#"{{"id":"lines{chunk}","put":[{}]}}"#,
            lines.join(",")
        ));
    }
    let boxes: Vec<String> = (0..36)
        .map(|n| {
            format!(
                r#"{{"shape":{{"id":"b{n}","kind":"rect","box":[{},{},40,24],"radius":4,"width":2}}}}"#,
                n % 6 * 50,
                n / 6 * 30 + 420
            )
        })
        .collect();
    steps.push(format!(r#"{{"id":"boxes","put":[{}]}}"#, boxes.join(",")));
    steps
}

/// Five line charts whose 40 series reach the path-segment cap exactly: 5 × 64 axis segments
/// and 39 series of 500 points and one of 220, each charged a segment per point after its first.
pub(crate) fn charts() -> Vec<String> {
    let charts: Vec<String> = (0..5)
        .map(|n| {
            format!(
                r#"{{"chart":{{"id":"c{n}","kind":"line","box":[0,{},4000,3000]}}}}"#,
                n * 3200
            )
        })
        .collect();
    let mut steps = vec![format!(r#"{{"id":"charts","put":[{}]}}"#, charts.join(","))];
    for pair in 0..20 {
        let series: Vec<String> = (pair * 2..pair * 2 + 2)
            .map(|n| {
                let data: Vec<String> = (0..if n == 39 { 220 } else { 500 })
                    .map(|at| format!("[{at},{}]", at % 17))
                    .collect();
                format!(
                    r#"{{"series":{{"id":"z{n}","in":"c{}","data":[{}]}}}}"#,
                    n / 8,
                    data.join(",")
                )
            })
            .collect();
        steps.push(format!(
            r#"{{"id":"series{pair}","put":[{}]}}"#,
            series.join(",")
        ));
    }
    steps
}

/// Thirteen eight-column tables whose 500 rows reach the table-cell cap exactly, every cell the
/// number 1e299, which layout writes out as 300 digits grouped by 99 commas.
pub(crate) fn tables() -> Vec<String> {
    let tables: Vec<String> = (0..13)
        .map(|n| format!(r#"{{"table":{{"id":"t{n}","cols":["A","B","C","D","E","F","G","H"],"box":[0,0,16000,12000]}}}}"#))
        .collect();
    let mut steps = vec![format!(r#"{{"id":"tables","put":[{}]}}"#, tables.join(","))];
    let cells = ["1e299"; 8].join(",");
    for table in 0..13 {
        let rows: Vec<String> = (table * 40..(table * 40 + 40).min(500))
            .map(|n| format!(r#"{{"row":{{"id":"r{n}","in":"t{table}","cells":[{cells}]}}}}"#))
            .collect();
        steps.push(format!(
            r#"{{"id":"rows{table}","put":[{}]}}"#,
            rows.join(",")
        ));
    }
    steps
}
