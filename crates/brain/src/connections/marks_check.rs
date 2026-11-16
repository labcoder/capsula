//! Debug builds only: `kumochid --marks-check`, the screen marks live check behind
//! `cargo xtask live marks`. On generated pictures it asks the model to mark five named things
//! through the screen surface's schema, at most [`BUDGET`] requests in all and none retried, checks
//! each call with the format's own parser, stamp and admission, and prints numbers and fixed
//! words only: never the model's text, labels or boxes. No screen, file or log is touched.

mod picture;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::time::Instant;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{CREDENTIAL_SERVICE, DATA_DIR, LIVE_BUDGET_ENV};
use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use kumochi_scene::record::{HighlightStyle, ScreenDisplay, Target};
use kumochi_scene::{
    Admission, GlobalBudget, Op, Record, SceneState, parse_screen_draw_record, screen,
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use self::picture::{Thing, index, jpeg, layout, picture, shrink};
use super::catalog::DEFAULT_SLUG;
use super::clock::{Clock as _, SystemClock};
use super::error::VaultError;
use super::model::budget::Budget;
use super::model::conn::{Connect, ModelClient, Tls};
use super::model::error::Failure;
use super::model::sse::SseEvent;
use super::model::{ResponsesBody, stream};
use super::provenance::{Outcome, dev_names, output_path, usable};
use super::vault::Vault;
use crate::context::{InputItem, Jpeg, ModelRequest, Role, ToolSchema};
use crate::engines::responses::body;
use crate::engines::{CallId, Opaque};
use crate::platform::credential::NativeCredentialStore;

/// The owner's approval: 45 requests, renewals included, none retried.
const BUDGET: u32 = 45;
const _: () = assert!(BUDGET <= 45, "the owner approved 45 requests");
/// Exit code when a picture or a body couldn't be built; nothing was sent.
const UNBUILT: u8 = 18;
/// Exit code when the session went stale or a line couldn't be written; later requests unspent.
const STOPPED: u8 = 20;
const EFFORT: Effort = Effort::Low;
/// The display the pictures are drawn for, at 100%.
const DISPLAY: ScreenDisplay = ScreenDisplay {
    origin: [0, 0],
    size: [2560, 1440],
    dpi: 96,
};
/// As the hold sends now, a larger picture, and the full display.
const SIZES: [[u32; 2]; 3] = [[1280, 720], [1600, 900], [2560, 1440]];
const LAYOUTS: u32 = 6;
/// The most call arguments kept.
const ARGUMENTS: usize = 64 * 1024;
const INSTRUCTIONS: &str = "You are Kumo, a desktop assistant. Answer the user's question about \
                            their screen, and use the draw tool to point at what they ask about.";
/// The draw prompt with the marking paragraph pull request 4 adds to it, both unchanged.
const DESCRIPTION: &str = concat!(
    include_str!("../../prompts/draw.md"),
    "\n\n",
    include_str!("marks_check/marking.md")
);
const DRAWN: &str = "Drawn.";
const FOLLOW_UP: &str = "Thanks. Which one did you mark first?";
/// Which of a layout's ten things each version names.
const NAMED: [[usize; 5]; 2] = [[0, 3, 4, 6, 8], [2, 1, 7, 9, 5]];

/// One planned request: a layout, a size and a version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Planned {
    layout: u32,
    size: usize,
    version: usize,
}

/// The 36 requests: each layout's two versions, each at the three sizes in turn. So the first
/// three are one picture at every size, and a size the endpoint refuses is known after them.
fn plan() -> Vec<Planned> {
    (0..LAYOUTS)
        .flat_map(|layout| {
            (0..2).flat_map(move |version| {
                (0..SIZES.len()).map(move |size| Planned {
                    layout,
                    size,
                    version,
                })
            })
        })
        .collect()
}

/// The `screen-picture` line as pull request 2 carries it into `brain.ftl`.
fn picture_line([width, height]: [u32; 2]) -> String {
    format!(
        "The image is a picture of the user's screen, {width} by {height} pixels, taken when they \
         let go of the talk keys, with their mouse pointer where it was then."
    )
}

fn question(things: &[Thing], version: usize) -> String {
    let names: Vec<String> = NAMED[version]
        .iter()
        .map(|&at| things[at].named())
        .collect();
    format!(
        "Show me where these are on my screen: {}.",
        names.join(", ")
    )
}

/// The `draw` tool as pull request 4 offers it: the screen schema, with overlay and screen the
/// only surfaces.
fn draw_tool() -> ToolSchema {
    let mut parameters = kumochi_scene::screen_draw_schema();
    parameters["$defs"]["Surface"] = json!({"type": "string", "enum": ["overlay", "screen"]});
    ToolSchema {
        name: "draw",
        description: DESCRIPTION,
        parameters,
    }
}

/// A built request: its body, the body's bytes, and the effort and image detail it sends.
struct Built {
    body: ResponsesBody,
    bytes: usize,
    effort: String,
    detail: String,
}

fn build(input: Vec<InputItem>, image: Option<(Jpeg, [u32; 2])>) -> Option<Built> {
    let mut request = ModelRequest {
        choice: Some(ModelChoice {
            model: ModelSlug::new(DEFAULT_SLUG).ok()?,
            effort: EFFORT,
            connection: ConnectionKind::ChatGpt,
        }),
        instructions: INSTRUCTIONS.to_owned(),
        input,
        tools: vec![draw_tool()],
    };
    if let Some((image, size)) = image {
        request.attach(image).ok()?;
        request.input.push(InputItem::Message {
            role: Role::Developer,
            text: picture_line(size),
        });
    }
    let value = body(&request).ok()?;
    let text = |value: &Value| value.as_str().unwrap_or("-").to_owned();
    Some(Built {
        bytes: value.to_string().len(),
        effort: text(&value["reasoning"]["effort"]),
        detail: detail(&value).map_or_else(|| "-".to_owned(), str::to_owned),
        body: ResponsesBody::new(&value).ok()?,
    })
}

/// The first image detail anywhere in `value`.
fn detail(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => map
            .get("detail")
            .and_then(Value::as_str)
            .or_else(|| map.values().find_map(detail)),
        Value::Array(items) => items.iter().find_map(detail),
        _ => None,
    }
}

/// What a reply held: the function calls, the first one when it is a `draw` small enough to
/// keep, and the input token count. Held in memory only, and never printed.
#[derive(Default)]
struct Reply {
    calls: usize,
    item: Option<Value>,
    input_tokens: Option<u64>,
}

fn keep(reply: &mut Reply, event: &SseEvent) {
    match event.kind() {
        "response.output_item.done" if event.value()["item"]["type"] == "function_call" => {
            reply.calls += 1;
            let item = &event.value()["item"];
            let small = item["arguments"]
                .as_str()
                .is_some_and(|arguments| arguments.len() <= ARGUMENTS);
            if reply.calls == 1 && item["name"] == "draw" && small {
                reply.item = Some(item.clone());
            }
        }
        "response.completed" => {
            reply.input_tokens = event.value()["response"]["usage"]["input_tokens"].as_u64();
        }
        _ => {}
    }
}

/// One mark's style and box, in picture pixels.
type Mark = (HighlightStyle, [f32; 4]);

/// A call's outcome: accepted with its box highlights, or refused with a fixed reason. No Debug:
/// the boxes are the model's.
#[derive(PartialEq)]
enum Verdict {
    Accepted(Vec<Mark>),
    /// An error code's name and the field path it points at.
    Refused(String, String),
    Malformed,
}

/// Checks a `draw` call's arguments as the screen surface will: the format's parser, the
/// brain's stamp for a picture of `size`, and admission of every step.
fn check(arguments: &str, size: [u32; 2]) -> Verdict {
    let refused = |error: kumochi_scene::StepError| {
        Verdict::Refused(format!("{:?}", error.code), error.path.as_str().to_owned())
    };
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return Verdict::Malformed;
    };
    let Some(records) = value["records"].as_array() else {
        return Verdict::Malformed;
    };
    let mut scene: Option<SceneState> = None;
    let mut budget = GlobalBudget::default();
    let mut marks = Vec::new();
    for record in records {
        let Ok(bytes) = serde_json::to_vec(record) else {
            return Verdict::Malformed;
        };
        match (parse_screen_draw_record(&bytes), scene.as_mut()) {
            (Err(error), _) => return refused(error),
            (Ok(Record::Begin(mut begin)), None) => {
                if let Err(error) = screen::stamp(&mut begin, size, DISPLAY) {
                    return refused(error);
                }
                match SceneState::begin(begin) {
                    Ok(state) => scene = Some(state),
                    Err(error) => return refused(error),
                }
            }
            (Ok(Record::Step(step)), Some(scene)) => {
                match scene.admit(&step, &budget) {
                    Ok(Admission::New(candidate)) => {
                        if scene.commit(candidate, &mut budget).is_err() {
                            return Verdict::Malformed;
                        }
                    }
                    Ok(Admission::Duplicate) => {}
                    Err(error) => return refused(error),
                }
                marks.extend(step.put.iter().filter_map(|op| match op {
                    Op::Highlight(mark) => match mark.target {
                        Target::Box(bounds) => Some((mark.style, bounds.map(f32::from))),
                        Target::Anchor(_) => None,
                    },
                    _ => None,
                }));
            }
            (Ok(Record::End(_)), Some(_)) => {}
            _ => return Verdict::Malformed,
        }
    }
    if scene.is_none() {
        return Verdict::Malformed;
    }
    Verdict::Accepted(marks)
}

/// `arguments` as the store will keep them: every step's box targets replaced through
/// [`screen::without_boxes`].
fn placeholders(arguments: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(arguments).ok()?;
    for record in value["records"].as_array_mut()? {
        if let Ok(Record::Step(step)) = parse_screen_draw_record(&serde_json::to_vec(record).ok()?)
        {
            *record = serde_json::to_value(Record::Step(screen::without_boxes(&step))).ok()?;
        }
    }
    Some(value.to_string())
}

/// Area over union of two `[x, y, w, h]` boxes.
fn overlap(a: [f32; 4], b: [f32; 4]) -> f32 {
    let width = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let height = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    let shared = width.max(0.0) * height.max(0.0);
    let union = a[2] * a[3] + b[2] * b[3] - shared;
    if union > 0.0 { shared / union } else { 0.0 }
}

/// A mark's line: the named thing it overlaps most, and its signed edge errors in picture and
/// display pixels, against that thing's box scaled into the picture.
fn mark_line(
    number: usize,
    part: usize,
    (style, mark): Mark,
    named: &[&Thing],
    size: [u32; 2],
) -> String {
    let scale = f32::from(u16::try_from(size[0]).expect("a picture width")) / 2560.0;
    let truths: Vec<[f32; 4]> = named
        .iter()
        .map(|thing| {
            thing
                .rect
                .map(|value| f32::from(u16::try_from(value).expect("on the display")) * scale)
        })
        .collect();
    let best = truths
        .iter()
        .enumerate()
        .map(|(at, truth)| (at, overlap(mark, *truth)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, overlap)| *overlap > 0.0);
    let style = format!("{style:?}").to_lowercase();
    let Some((at, shared)) = best else {
        return format!("mark={number}.{part} thing=none style={style}");
    };
    let truth = truths[at];
    let edges = [
        mark[0] - truth[0],
        mark[1] - truth[1],
        mark[0] + mark[2] - truth[0] - truth[2],
        mark[1] + mark[3] - truth[1] - truth[3],
    ];
    let display = edges.map(|edge| edge / scale);
    format!(
        "mark={number}.{part} thing={} style={style} left={:+.1} top={:+.1} right={:+.1} \
         bottom={:+.1} left_display={:+.1} top_display={:+.1} right_display={:+.1} \
         bottom_display={:+.1} overlap={shared:.2}",
        named[at].word(),
        edges[0],
        edges[1],
        edges[2],
        edges[3],
        display[0],
        display[1],
        display[2],
        display[3],
    )
}

/// A request's status, provider code and parameter, and result word.
fn outcome(result: Result<(), Failure>) -> [String; 4] {
    let none = || "-".to_owned();
    match result {
        Ok(()) => ["2xx".to_owned(), none(), none(), "completed".to_owned()],
        Err(Failure::Provider(error)) => [
            error.status.to_string(),
            error.code.map_or_else(none, |code| format!("{code:?}")),
            error.param.map_or_else(none, |param| format!("{param:?}")),
            "refused".to_owned(),
        ],
        Err(other) => [none(), none(), none(), format!("{other:?}").to_lowercase()],
    }
}

/// Request `number`'s lines: its own, then one per box mark of an accepted call; and whether its
/// call was accepted the first time.
fn lines(
    number: usize,
    request: &Prepared,
    result: Result<(), Failure>,
    reply: &Reply,
    ms: u128,
) -> (Vec<String>, bool) {
    let [status, code, param, word] = outcome(result);
    let head = format!(
        "{} status={status} code={code} param={param} result={word}",
        label(number, request)
    );
    if result.is_err() {
        let line =
            format!("{head} call=- accepted=- refusal=- path=- marks=- input_tokens=- ms={ms}");
        return (vec![line], false);
    }
    let size = SIZES[request.planned.size];
    let call = match reply.calls {
        0 => "none",
        1 => "one",
        _ => "several",
    };
    let verdict = reply
        .item
        .as_ref()
        .and_then(|item| item["arguments"].as_str())
        .map_or(Verdict::Malformed, |arguments| check(arguments, size));
    let none = || "-".to_owned();
    let (accepted, refusal, path, count) = match &verdict {
        Verdict::Accepted(marks) => ("true", none(), none(), marks.len().to_string()),
        Verdict::Refused(code, path) => ("false", code.clone(), path.clone(), none()),
        Verdict::Malformed => ("false", "malformed".to_owned(), none(), none()),
    };
    let tokens = reply
        .input_tokens
        .map_or_else(none, |tokens| tokens.to_string());
    let mut lines = vec![format!(
        "{head} call={call} accepted={accepted} refusal={refusal} path={path} marks={count} \
         input_tokens={tokens} ms={ms}"
    )];
    let Verdict::Accepted(marks) = verdict else {
        return (lines, false);
    };
    let named: Vec<&Thing> = NAMED[request.planned.version]
        .iter()
        .map(|&at| &request.things[at])
        .collect();
    lines.extend(
        marks
            .into_iter()
            .enumerate()
            .map(|(part, mark)| mark_line(number, part + 1, mark, &named, size)),
    );
    (lines, reply.calls == 1)
}

/// The dry run's lines: every request built, none sent.
fn dry(out: &mut impl std::io::Write, prepared: &[Prepared], detail: &str) -> std::io::Result<()> {
    for (number, request) in prepared.iter().enumerate() {
        writeln!(out, "{} result=dry", label(number + 1, request))?;
    }
    writeln!(out, "request={} replay=of-1 result=dry", prepared.len() + 1)?;
    writeln!(
        out,
        "{}",
        summary(0, 0, 0, prepared.len(), "not-sent", detail)
    )
}

/// One prepared request: its plan, its layout's things and its built body.
struct Prepared {
    planned: Planned,
    things: Vec<Thing>,
    question: String,
    jpeg_bytes: usize,
    built: Built,
}

fn prepare() -> Option<Vec<Prepared>> {
    let pictures: Vec<(Vec<Thing>, Vec<u8>)> = (0..LAYOUTS)
        .map(|k| {
            let things = layout(k);
            let rgb = picture(k, &things);
            (things, rgb)
        })
        .collect();
    plan()
        .into_iter()
        .map(|planned| {
            let (things, rgb) = &pictures[index(planned.layout)];
            let size = SIZES[planned.size];
            let jpeg = jpeg(&shrink(rgb, DISPLAY.size, size), size)?;
            let jpeg_bytes = jpeg.len();
            let question = question(things, planned.version);
            let built = build(
                vec![InputItem::Message {
                    role: Role::User,
                    text: question.clone(),
                }],
                Some((Jpeg::new(jpeg)?, size)),
            )?;
            Some(Prepared {
                planned,
                things: layout(planned.layout),
                question,
                jpeg_bytes,
                built,
            })
        })
        .collect()
}

fn label(number: usize, prepared: &Prepared) -> String {
    let Planned {
        layout,
        size,
        version,
    } = prepared.planned;
    let [width, height] = SIZES[size];
    format!(
        "request={number} layout=L{} size={width}x{height} version={} jpeg_bytes={} body_bytes={}",
        layout + 1,
        ["a", "b"][version],
        prepared.jpeg_bytes,
        prepared.built.bytes,
    )
}

/// The budget this run may spend: [`BUDGET`], or less when the live-budget variable says so; a
/// value that isn't a count allows nothing.
fn limit(value: Option<String>) -> u32 {
    value.map_or(BUDGET, |value| {
        value.trim().parse().unwrap_or(0).min(BUDGET)
    })
}

/// Sends one body, keeping what its reply holds, and returns the result and its milliseconds.
async fn send(
    client: &ModelClient<impl Connect>,
    body: &ResponsesBody,
    bearer: &Secret<Zeroizing<String>>,
    budget: &Budget,
    reply: &mut Reply,
) -> (Result<(), Failure>, u128) {
    let start = Instant::now();
    let result = stream(
        client,
        body,
        bearer,
        std::future::ready(Some(())),
        budget,
        |event: &SseEvent| keep(reply, event),
    )
    .await;
    (result, start.elapsed().as_millis())
}

/// Runs the check and returns its exit code: 0 when the plan was worked through, 10–17 as the
/// catalog capture's preflight (nothing sent), 18 when nothing could be built (nothing sent), and
/// 20 once the session went stale or a line couldn't be written (later requests unspent).
#[expect(
    clippy::too_many_lines,
    reason = "the run's steps in the order they spend, as one reads the budget"
)]
pub fn run() -> u8 {
    let located = std::env::current_exe()
        .ok()
        .and_then(|exe| output_path(&exe))
        .is_some();
    if !dev_names(CREDENTIAL_SERVICE, DATA_DIR) || !located {
        return Outcome::Preflight.code();
    }
    let Some(prepared) = prepare() else {
        return UNBUILT;
    };
    let limit = limit(std::env::var(LIVE_BUDGET_ENV).ok());
    let mut out = std::io::stdout();
    let first = &prepared[0].built;
    let header = format!(
        "marks-check model={DEFAULT_SLUG} effort={} detail={} date={} planned={} budget={limit}",
        first.effort,
        first.detail,
        time::OffsetDateTime::now_utc().date(),
        prepared.len(),
    );
    if writeln!(out, "{header}").is_err() {
        return STOPPED;
    }
    if limit == 0 {
        return if dry(&mut out, &prepared, &first.detail).is_ok() {
            0
        } else {
            STOPPED
        };
    }
    let vault = match Vault::open(NativeCredentialStore) {
        Ok(vault) => vault,
        Err(VaultError::Busy) => return Outcome::Busy.code(),
        Err(_) => return Outcome::Vault.code(),
    };
    let Ok(account) = vault.load_account() else {
        return Outcome::Vault.code();
    };
    let (Ok(runtime), Ok(tls)) = (
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        Tls::build(),
    ) else {
        return Outcome::NoClient.code();
    };
    let client = ModelClient::new(tls);
    let budget = Budget::limited(Some(limit));
    let code = runtime.block_on(async {
        let bearer = || {
            usable(
                account.as_ref().map(Secret::expose),
                SystemClock.unix_seconds(),
            )
        };
        let (mut attempted, mut accepted, mut skipped) = (0, 0, 0);
        let mut refused_sizes = BTreeSet::new();
        let mut replay_of = None;
        for (number, request) in prepared.iter().enumerate() {
            if refused_sizes.contains(&request.planned.size) {
                skipped += 1;
                let line = label(number + 1, request);
                if writeln!(out, "{line} result=skipped reason=size-refused").is_err() {
                    return STOPPED;
                }
                continue;
            }
            let bearer = match bearer() {
                Ok(bearer) => bearer,
                Err(outcome) if number == 0 => return outcome.code(),
                Err(_) => return STOPPED,
            };
            let mut reply = Reply::default();
            let (result, ms) =
                send(&client, &request.built.body, &bearer, &budget, &mut reply).await;
            attempted += usize::from(!matches!(result, Err(Failure::Spent)));
            if matches!(result, Err(Failure::Provider(_))) {
                refused_sizes.insert(request.planned.size);
            }
            let (lines, first_call) = lines(number + 1, request, result, &reply, ms);
            accepted += usize::from(first_call);
            if number == 0 && result.is_ok() {
                replay_of = reply.item.take();
            }
            if lines.iter().any(|line| writeln!(out, "{line}").is_err()) {
                return STOPPED;
            }
        }
        let replay = match replay_of.and_then(|item| replay(&prepared[0].question, &item)) {
            None => "not-sent",
            Some(built) => {
                let Ok(bearer) = bearer() else {
                    return STOPPED;
                };
                let (result, ms) = send(
                    &client,
                    &built.body,
                    &bearer,
                    &budget,
                    &mut Reply::default(),
                )
                .await;
                attempted += usize::from(!matches!(result, Err(Failure::Spent)));
                let [status, code, param, word] = outcome(result);
                if writeln!(
                    out,
                    "request={} replay=of-1 body_bytes={} status={status} code={code} \
                     param={param} result={word} ms={}",
                    prepared.len() + 1,
                    built.bytes,
                    ms,
                )
                .is_err()
                {
                    return STOPPED;
                }
                if result.is_ok() {
                    "accepted"
                } else {
                    "refused"
                }
            }
        };
        let summary = summary(
            attempted,
            accepted,
            skipped,
            prepared.len(),
            replay,
            &first.detail,
        );
        if writeln!(out, "{summary}").is_err() {
            STOPPED
        } else {
            0
        }
    });
    drop(vault);
    code
}

/// The second turn of a call: the question, the call as the store keeps it, a fixed result and a
/// fixed follow-up, with no picture.
fn replay(question: &str, item: &Value) -> Option<Built> {
    let mut stored = item.clone();
    stored["arguments"] = placeholders(item["arguments"].as_str()?)?.into();
    let call = CallId::new(item["call_id"].as_str()?).ok()?;
    build(
        vec![
            InputItem::Message {
                role: Role::User,
                text: question.to_owned(),
            },
            InputItem::Native(Opaque::new(serde_json::to_vec(&stored).ok()?).ok()?),
            InputItem::FunctionOutput {
                call,
                output: DRAWN.to_owned(),
            },
            InputItem::Message {
                role: Role::User,
                text: FOLLOW_UP.to_owned(),
            },
        ],
        None,
    )
}

fn summary(
    attempted: usize,
    accepted: usize,
    skipped: usize,
    planned: usize,
    replay: &str,
    detail: &str,
) -> String {
    format!(
        "summary attempted={attempted} accepted_first_call={accepted}/{planned} skipped={skipped} \
         replay={replay} note=the-full-size-picture-went-with-detail-{detail}-and-may-have-been-shrunk-by-the-provider"
    )
}
