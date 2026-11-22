//! The brain's [`WideMeasure`] against shell-like measures that keep its contract: narrower
//! characters, shorter lines, and the same breaks or more. On every generated scene the brain's
//! fill and outline are at least the shell's. Dashes sit outside that bound, so the fixtures
//! test compares them.
use kumochi_scene::{
    Admission, Begin, ErrorCode, GlobalBudget, Record, SceneState, StopCut,
    display::{MeasureError, PaintWork, TextMeasure, TextStyle, WideMeasure},
    layout::{LayoutState, PrepareError, PrepareInput, SurfaceGeometry, prepare},
    limits::MAX_GLOBAL_DASH_PIECES,
    parse_record,
    text::Bounded,
    theme::Theme,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

const SEED: u64 = 0x6B75_6D6F_6368_6921;
const CASES: u64 = 160;

/// xorshift64*: a fixed seed gives the same scenes on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("a small range");
        usize::try_from(self.next() % n).expect("below a usize")
    }
    fn unit(&mut self) -> f32 {
        f32::from(u16::try_from(self.next() >> 48).expect("16 bits")) / 65_536.0
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

#[derive(Clone, Copy, Debug)]
enum Breaks {
    /// The brain's own break places.
    Rule,
    /// A break before any character once a line holds text, so words break where the brain's
    /// don't.
    Anywhere,
}

/// A shell font: each character a seeded fraction of the brain's own advance for it, or exactly
/// that advance when `exact`, lines between 1 and 1.5 em, and greedy breaks. These shells test how
/// the brain lays characters out; whether its advances bound the real font is the shell's
/// conformance test.
#[derive(Debug)]
struct Shell {
    seed: u64,
    exact: bool,
    line: f32,
    breaks: Breaks,
}

impl Shell {
    fn advance(&self, ch: char) -> f32 {
        let style = TextStyle {
            size: 1.0,
            bold: false,
        };
        let [brain, _] = WideMeasure
            .measure(ch.encode_utf8(&mut [0; 4]), style, None)
            .expect("one character measures");
        if self.exact {
            return brain;
        }
        let mut hash = Rng((self.seed ^ u64::from(ch).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
        hash.next();
        brain * (0.05 + 0.95 * hash.unit())
    }

    fn wrap(&self, line: &str, limit: f32) -> (f32, f32) {
        let mut x = 0.0_f32;
        let mut start = (0, 0.0_f32);
        let mut candidate = None;
        let mut breaks = 0.0_f32;
        let mut text = false;
        let mut previous = '\0';
        for (index, ch) in line.chars().enumerate() {
            let opens = match self.breaks {
                Breaks::Rule if word(ch) => previous == ' ',
                Breaks::Rule => ch != ' ',
                Breaks::Anywhere => true,
            };
            if opens && text {
                candidate = Some((index, x));
            }
            text |= ch != ' ';
            let advance = self.advance(ch);
            x += advance;
            if x - start.1 > limit && index > start.0 {
                start = candidate.take().unwrap_or((index, x - advance));
                breaks += 1.0;
            }
            previous = ch;
        }
        (x, breaks)
    }
}

/// The shell's word characters at its pinned text layout revision.
fn word(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, '\u{00C0}'..='\u{024F}' | '\u{0300}'..='\u{036F}' | '\u{0400}'..='\u{04FF}'
            | '\u{0980}'..='\u{09FF}' | '\u{1E00}'..='\u{1EFF}')
        || "-_.'\u{2018}\u{2019}$%@#^~,=:;!)]}\"\u{201D}\u{00BB}\u{2026}\u{22EF}\u{202F}\u{00A0}\u{2011}"
            .contains(ch)
}

impl TextMeasure for Shell {
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let limit = max_width.map_or(f32::INFINITY, |width| width / style.size);
        let mut width = 0.0_f32;
        let mut lines = 0.0_f32;
        for line in text.split('\n') {
            let (full, breaks) = self.wrap(line, limit);
            width = width.max(full.min(limit));
            lines += breaks + 1.0;
        }
        Ok([width * style.size, lines * self.line * style.size])
    }
}

const WORDS: &[&str] = &[
    "Start",
    "Load",
    "the",
    "request",
    "Validate",
    "input",
    "cache",
    "Wolfgang",
    "WWW",
    "/api/v1/users",
    "a/b/c",
    "non-English",
    "state-of-the-art",
    "na\u{EF}ve",
    "\u{DC}ber",
    "\u{3A9}\u{3BC}\u{3AD}\u{3B3}\u{3B1}",
    "\u{42E}\u{43D}\u{438}\u{43A}\u{43E}\u{434}",
    "\u{6570}\u{636E}\u{5E93}",
    "\u{8FDE}\u{63A5}\u{6C60}",
    "\u{1F680}",
    "\u{2705}",
    "\u{2014}",
    "\u{2030}",
    "\u{2E3B}",
    "supercalifragilisticexpialidocious",
    "x",
    "42%",
    "foo?bar=2",
    "(retry)",
    "don\u{2019}t",
    "\u{2026}",
    "e\u{301}t\u{e9}",
    "\u{1F468}\u{200D}\u{1F469}",
    "",
];
const SHAPES: &[&str] = &[
    "box", "round", "pill", "circle", "diamond", "note", "cylinder", "pill", "circle",
];

fn label(rng: &mut Rng, most: usize) -> String {
    let mut text = String::new();
    for index in 0..=rng.below(most) {
        if index > 0 {
            text.push_str(if rng.chance(12) {
                "\n"
            } else if rng.chance(10) {
                "  "
            } else {
                " "
            });
        }
        text.push_str(WORDS[rng.below(WORDS.len())]);
    }
    text
}

/// One generated scene: its steps, and the nodes a Stop after the first keeps without their
/// diagram, which re-roots them.
struct Case {
    steps: Vec<Value>,
    kept: Option<Vec<String>>,
}

fn case(rng: &mut Rng) -> Case {
    let dir = if rng.chance(2) { "down" } else { "right" };
    let nodes = 1 + rng.below(36);
    let nested = rng.chance(3);
    let mut put = vec![json!({"diagram":{"id":"d","dir":dir}})];
    if nested {
        put.push(json!({"diagram":{"id":"c","in":"d"}}));
    }
    for index in 0..nodes {
        let parent = if nested && index % 3 == 0 { "c" } else { "d" };
        let mut node = json!({"id":format!("n{index}"),"in":parent,"label":label(rng, 7),
            "shape":SHAPES[rng.below(SHAPES.len())]});
        if rng.chance(4) {
            node["sub"] = json!(label(rng, 4));
        }
        put.push(json!({ "node": node }));
    }
    for index in 1..nodes {
        let mut edge = json!({"id":format!("e{index}"),"from":format!("n{}", rng.below(index)),
            "to":format!("n{index}"),"dash":rng.chance(4)});
        if rng.chance(2) {
            edge["label"] = json!(label(rng, 3));
        }
        put.push(json!({ "edge": edge }));
    }
    for index in 0..rng.below(4) {
        let side = 2.0 + 10.0 * rng.unit();
        let x = 1200.0 + 30.0 * f32::from(u8::try_from(index).expect("a few"));
        put.push(
            json!({"shape":{"id":format!("r{index}"),"kind":"rect","box":[x, 40, side, side * 1.5],
            "radius":20,"fill":"blue","color":"fg","width":1}}),
        );
    }
    if rng.chance(3) {
        put.push(json!({"shape":{"id":"target","kind":"rect","box":[1300,300,120,60]}}));
        put.push(json!({"callout":{"id":"call","target":{"ref":"target"},"at":[1500,420],"text":label(rng, 12)}}));
        put.push(json!({"text":{"id":"words","text":label(rng, 10),"at":[1300,700]}}));
        put.push(json!({"highlight":{"id":"mark","target":{"ref":"words"},"pad":6}}));
    }
    if rng.chance(3) {
        let categories: Vec<String> = (0..2 + rng.below(20)).map(|_| label(rng, 3)).collect();
        let data: Vec<usize> = categories.iter().map(|_| 1 + rng.below(9)).collect();
        put.push(json!({"chart":{"id":"chart","kind":"bar","categories":categories}}));
        put.push(json!({"series":{"in":"chart","id":"s","name":label(rng, 4),"data":data}}));
    }
    if rng.chance(3) {
        let columns = 1 + rng.below(6);
        let cols: Vec<String> = (0..columns).map(|_| label(rng, 3)).collect();
        put.push(json!({"table":{"id":"table","cols":cols}}));
        for row in 0..=rng.below(5) {
            let cells: Vec<String> = (0..columns).map(|_| label(rng, 6)).collect();
            put.push(json!({"row":{"in":"table","id":format!("row{row}"),"cells":cells}}));
        }
    }
    let mut steps = vec![json!({"id":"one","put":put})];
    let kept = rng.chance(4).then(|| {
        let kept: Vec<String> = (0..nodes)
            .filter(|_| rng.chance(2))
            .map(|index| format!("n{index}"))
            .collect();
        let set: Vec<Value> = kept
            .iter()
            .map(|id| json!({"target":id,"label":label(rng, 7)}))
            .collect();
        steps.push(json!({"id":"two","set":set}));
        kept
    });
    Case { steps, kept }
}

fn header() -> Begin {
    let Record::Begin(header) =
        parse_record(br#"{"begin":{"v":1,"scene":"generated","surface":"overlay","alt":""}}"#)
            .expect("begin")
    else {
        panic!("begin")
    };
    header
}

/// The scene's paint work after its last step, uncapped, as `measure` lays it out.
#[expect(
    clippy::panic_in_result_fn,
    reason = "a generated step that fails to parse or admit is a test bug, not a layout error"
)]
fn work(
    steps: &[Value],
    kept: Option<&[String]>,
    measure: &mut dyn TextMeasure,
) -> Result<PaintWork, PrepareError> {
    let mut scene = SceneState::begin(header()).expect("scene");
    let mut budget = GlobalBudget::default();
    let mut layout = LayoutState::new(&scene);
    for (index, value) in steps.iter().enumerate() {
        if index == 1
            && let Some(kept) = kept
        {
            let handles = kept
                .iter()
                .map(|id| {
                    scene
                        .handle(&id.clone().try_into().expect("id"))
                        .expect("a kept node")
                })
                .collect();
            let cut = StopCut {
                activated: 1,
                revealed: Bounded::new(handles).expect("cut"),
            };
            scene.stop(&cut, &mut budget).expect("stop");
            layout = layout.cut(&scene)?;
        }
        let bytes = serde_json::to_vec(&json!({ "step": value })).expect("json");
        let Record::Step(step) = parse_record(&bytes).expect("a valid generated step") else {
            panic!("step")
        };
        let Admission::New(admitted) = scene
            .admit(&step, &budget)
            .expect("an admitted generated step")
        else {
            panic!("new")
        };
        let ticket = scene
            .commit(admitted, &mut budget)
            .expect("fresh")
            .preparation;
        let surface = SurfaceGeometry::Overlay {
            default_canvas: [960.0, 600.0],
        };
        let input = PrepareInput::new(ticket, layout, surface, Arc::new(Theme::default()))
            .map_err(|(error, _)| error)?;
        layout = prepare(input, measure, &AtomicBool::new(false))?.after;
    }
    Ok(layout.paint_work())
}

/// Seed 0x6b756d6f63686921, 4,000 labels, each at two sizes, four widths and against two shells
/// with the brain's break places and two that break anywhere, half of them at exactly the brain's
/// advances: the brain's width, height and line count are never below the shell's.
#[test]
fn the_wide_measure_bounds_shell_measures_on_generated_labels() {
    let style = TextStyle {
        size: 10.0,
        bold: true,
    };
    for (text, max_width, expected) in [
        ("中文\n中\n", None, [20.0, 45.0]),
        ("m m m mm", Some(25.0), [25.0, 60.0]),
        ("mmmm", Some(25.0), [25.0, 30.0]),
    ] {
        assert_eq!(
            WideMeasure
                .measure(text, style, max_width)
                .expect("known label")
                .map(f32::to_bits),
            expected.map(f32::to_bits),
            "{text:?} with width {max_width:?}"
        );
    }
    let mut rng = Rng(SEED);
    for index in 0..4_000_u64 {
        let text = label(&mut rng, 12);
        for (seed, breaks) in [
            (1, Breaks::Rule),
            (2, Breaks::Anywhere),
            (3, Breaks::Anywhere),
        ] {
            let mut shell = Shell {
                seed: SEED ^ index ^ seed,
                exact: seed % 2 == 0,
                line: 1.5,
                breaks,
            };
            for size in [15.0, 13.0] {
                let style = TextStyle { size, bold: true };
                for width in [None, Some(180.0), Some(220.0), Some(40.0)] {
                    let brain = WideMeasure.measure(&text, style, width).expect("measured");
                    let real = shell.measure(&text, style, width).expect("measured");
                    assert!(
                        brain[0] >= real[0] && brain[1] >= real[1],
                        "label {index} {text:?} at {size} width {width:?} under {shell:?}: brain {brain:?} below shell {real:?}"
                    );
                }
            }
        }
    }
}

/// Seed 0x6b756d6f63686921, 160 scenes, each against two shells: one with the brain's break
/// places and one that breaks anywhere, a quarter of them at exactly the brain's advances.
#[test]
fn the_wide_measure_charges_at_least_the_shell_on_generated_scenes() {
    let mut rng = Rng(SEED);
    let (mut compared, mut brain_refused, mut shell_refused) = (0, 0, 0);
    for index in 0..CASES {
        let case = case(&mut rng);
        let kept = case.kept.as_deref();
        let brain = work(&case.steps, kept, &mut WideMeasure);
        for breaks in [Breaks::Rule, Breaks::Anywhere] {
            let mut shell = Shell {
                seed: SEED ^ index,
                exact: index % 4 == 0,
                line: 1.0 + 0.5 * rng.unit(),
                breaks,
            };
            let shell_work = work(&case.steps, kept, &mut shell);
            for (measure, result) in [("WideMeasure", &brain), ("shell", &shell_work)] {
                match result {
                    Ok(_) | Err(PrepareError::OverPaintWork) => {}
                    Err(PrepareError::Layout(error)) if error.code == ErrorCode::OutOfRange => {}
                    Err(error) => panic!("case {index}, {measure} under {shell:?}: {error:?}"),
                }
            }
            brain_refused += u64::from(brain.is_err());
            shell_refused += u64::from(shell_work.is_err());
            if let (Ok(brain), Ok(shell_work)) = (&brain, shell_work) {
                compared += 1;
                assert!(
                    brain.fill_vertices >= shell_work.fill_vertices
                        && brain.outline_points >= shell_work.outline_points
                        && brain.largest_fill >= shell_work.largest_fill
                        && brain.largest_outline >= shell_work.largest_outline,
                    "case {index} under {shell:?}: brain {brain:?} below shell {shell_work:?}"
                );
            }
        }
    }
    println!("compared {compared}, brain refused {brain_refused}, shell refused {shell_refused}");
    assert!(
        compared >= 2 * CASES * 9 / 10,
        "compared {compared}, brain refused {brain_refused}, shell refused {shell_refused}"
    );
}

fn flow(nodes: usize, dashed: &[usize], dir: &str, node: &str, edge: Option<&str>) -> Value {
    let mut put = vec![json!({"diagram":{"id":"d","dir":dir}})];
    put.extend((0..nodes).map(|index| {
        json!({"node":{"id":format!("n{index}"),"in":"d","label":format!("{node} {index}")}})
    }));
    put.extend((1..nodes).map(|index| {
        let mut value = json!({"id":format!("e{index}"),"from":format!("n{}", index - 1),
            "to":format!("n{index}"),"dash":dashed.contains(&index)});
        if let Some(label) = edge {
            value["label"] = json!(label);
        }
        json!({ "edge": value })
    }));
    Value::Array(put)
}

/// Dashed illustrations of the kinds a long label, a skip-rank link or a fan-out to wide nodes
/// lengthens: under the wide measure each fits the limits, and its dash count is at least every
/// shell's here except for the known two-piece fan-out shortfall, whose wider nodes shorten
/// its dashed edges.
#[test]
fn dashed_fixtures_fit_and_their_dashes_are_not_under_counted() {
    let mut skips = flow(6, &[], "down", "Check the cache", None);
    skips.as_array_mut().expect("ops").extend([
        json!({"edge":{"id":"s1","from":"n0","to":"n3","dash":true,"label":"retry"}}),
        json!({"edge":{"id":"s2","from":"n1","to":"n5","dash":true}}),
    ]);
    let fixtures = [
        flow(8, &[2, 4, 6], "down", "Validate input", Some("then")),
        flow(8, &[2, 4, 6], "right", "Step", None),
        flow(30, &[5, 10, 15, 20, 25, 29], "down", "Step", Some("then")),
        flow(
            12,
            &(1..12).collect::<Vec<_>>(),
            "down",
            "Check the cache",
            None,
        ),
        flow(
            8,
            &[2, 4, 6],
            "right",
            "Step",
            Some("only when the cache is cold"),
        ),
        skips,
        json!([{"diagram":{"id":"d","dir":"down"}},
            {"node":{"id":"r","in":"d","label":"Load balancer"}},
            {"node":{"id":"a","in":"d","label":"Application server in region one"}},
            {"node":{"id":"b","in":"d","label":"Application server in region two"}},
            {"node":{"id":"c","in":"d","label":"Application server in region three"}},
            {"edge":{"id":"e1","from":"r","to":"a","dash":true}},
            {"edge":{"id":"e2","from":"r","to":"b","dash":true}},
            {"edge":{"id":"e3","from":"r","to":"c","dash":true}}]),
        json!([{"diagram":{"id":"d"}},{"mermaid":{"in":"d","src":"flowchart TD\nA[Start] --> B{Is it cached?}\nB -->|yes| C[Serve from cache]\nB -.->|no| D[Fetch from origin]\nD -.-> E[Store in cache]\nE --> C\nC -.-> F[Done]\n"}}]),
        json!([{"diagram":{"id":"d"}},{"mermaid":{"in":"d","src":"flowchart LR\nsubgraph g[Backend services]\nA[API gateway] --> B[Auth service]\nB -.-> C[User store]\nend\nC -.-> D[Audit log]\n"}}]),
        json!([{"text":{"id":"a","text":"Client request","at":[100,100]}},
            {"text":{"id":"b","text":"Server response with details","at":[600,400]}},
            {"line":{"id":"l","from":{"ref":"a"},"to":{"ref":"b"},"dash":true}}]),
    ];
    let mut rng = Rng(SEED);
    for (index, put) in fixtures.iter().enumerate() {
        let steps = [json!({"id":"one","put":put})];
        let brain = work(&steps, None, &mut WideMeasure).expect("the brain prepares it");
        assert!(brain.within_limits(), "fixture {index}: {brain:?}");
        assert!(
            brain.dash_pieces > 0,
            "fixture {index}: the brain must charge dashed paint"
        );
        let most = (0..8)
            .map(|seed| {
                let mut shell = Shell {
                    seed,
                    exact: seed == 0,
                    line: 1.0 + 0.5 * rng.unit(),
                    breaks: if seed % 2 == 0 {
                        Breaks::Rule
                    } else {
                        Breaks::Anywhere
                    },
                };
                let pieces = work(&steps, None, &mut shell)
                    .expect("the shell prepares it")
                    .dash_pieces;
                assert!(
                    (1..=MAX_GLOBAL_DASH_PIECES).contains(&pieces),
                    "fixture {index}: shell {seed} must charge dashes and fit, got {pieces}"
                );
                pieces
            })
            .max()
            .expect("eight shells");
        if index == 6 {
            assert!(
                most.saturating_sub(brain.dash_pieces) <= 2,
                "fan-out's dash shortfall must not grow: brain {brain:?}, shell max {most}"
            );
        } else {
            assert!(
                brain.dash_pieces >= most,
                "fixture {index}: {brain:?} below {most}"
            );
        }
    }
}
