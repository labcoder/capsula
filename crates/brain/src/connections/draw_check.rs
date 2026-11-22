//! Debug builds only: `kumochid --draw-check`, the drawing live check behind
//! `cargo xtask live draw`. It asks for the two drawings the recorded calls in
//! `fixtures/responses` hold, the water cycle and y = x², through the user's own `ChatGPT`
//! connection, with `draw` as Kumo's model is offered it: at most [`BUDGET`] requests, none
//! retried. Each call is checked as the drawing ingress checks a new scene: the format's parser,
//! the scene's begin and every step's admission. It prints numbers and fixed words only, never the
//! model's text or arguments. No screen, file or log is touched.

#[cfg(test)]
mod tests;

use std::env::VarError;
use std::io::Write as _;
use std::time::Instant;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{CREDENTIAL_SERVICE, DATA_DIR, LIVE_BUDGET_ENV};
use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use kumochi_scene::record::Surface;
use kumochi_scene::{
    Admission, ErrorCode, GlobalBudget, Record, SceneState, StepError, TokenError, Tokenizer,
    parse_screen_draw_record, refuse_three_dimensional,
};
use zeroize::Zeroizing;

use super::catalog::DEFAULT_SLUG;
use super::clock::{Clock as _, SystemClock};
use super::error::VaultError;
use super::marks_check::{account_refusal, outcome};
use super::model::budget::Budget;
use super::model::conn::{ModelClient, Tls};
use super::model::error::Failure;
use super::model::sse::SseEvent;
use super::model::{ResponsesBody, stream};
use super::provenance::{Outcome, dev_names, output_path, usable};
use super::vault::Vault;
use crate::context::{InputItem, ModelRequest, Origin, Role};
use crate::dispatch::{MAX_ARGS, model_schemas};
use crate::drawing::CallError;
use crate::engines::responses::body;
use crate::platform::credential::NativeCredentialStore;

/// One request per recorded drawing, none retried.
const BUDGET: u32 = 2;
/// Exit code when a body couldn't be built; nothing was sent.
const UNBUILT: u8 = 18;
/// Exit code when the session went stale, a reply couldn't be read whole or a line couldn't be
/// written; later requests unspent.
const STOPPED: u8 = 20;
/// Exit code when the provider refused the account; later requests unspent.
const REFUSED: u8 = 21;
const EFFORT: Effort = Effort::Low;
const INSTRUCTIONS: &str = "You are Kumo, a desktop assistant. Answer the user, and use the draw \
                            tool when a drawing helps.";
/// Each request's name and the user's words.
const PROMPTS: [(&str, &str); 2] = [
    ("water-cycle", "Draw the water cycle for me."),
    ("y-squared", "Graph y = x² for me."),
];

/// A built request: its body and the body's bytes.
struct Built {
    body: ResponsesBody,
    bytes: usize,
}

fn prepare() -> Option<Vec<(&'static str, Built)>> {
    let draw = model_schemas(Origin::Kumo)
        .into_iter()
        .find(|tool| tool.name == "draw")?;
    PROMPTS
        .iter()
        .map(|&(name, words)| {
            let request = ModelRequest {
                choice: Some(ModelChoice {
                    model: ModelSlug::new(DEFAULT_SLUG).ok()?,
                    effort: EFFORT,
                    connection: ConnectionKind::ChatGpt,
                }),
                instructions: INSTRUCTIONS.to_owned(),
                input: vec![InputItem::Message {
                    role: Role::User,
                    text: words.to_owned(),
                }],
                tools: vec![draw.clone()],
            };
            let value = body(&request).ok()?;
            Some((
                name,
                Built {
                    bytes: value.to_string().len(),
                    body: ResponsesBody::new(&value).ok()?,
                },
            ))
        })
        .collect()
}

/// What a reply held: its function calls and the first one's arguments when it is a `draw`
/// within the tool's argument bound. Held in memory only, and never printed.
#[derive(Default)]
struct Reply {
    calls: usize,
    draw: Option<Result<String, ()>>,
}

fn keep(reply: &mut Reply, event: &SseEvent) {
    if event.kind() != "response.output_item.done"
        || event.value()["item"]["type"] != "function_call"
    {
        return;
    }
    reply.calls += 1;
    let item = &event.value()["item"];
    if reply.calls == 1 && item["name"] == "draw" {
        reply.draw = item["arguments"].as_str().map(|arguments| {
            (arguments.len() <= MAX_ARGS)
                .then(|| arguments.to_owned())
                .ok_or(())
        });
    }
}

/// A call's outcome, as error codes and schema field paths only.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Every record was accepted; the steps that were new.
    Drawn(usize),
    /// The code and field that stopped it.
    Refused(String, String),
}

impl Verdict {
    fn words(&self) -> String {
        match self {
            Self::Drawn(steps) => format!("draw=drawn steps={steps}"),
            Self::Refused(code, field) => format!("draw=refused error={code} field={field}"),
        }
    }
}

/// Checks a `draw` call's arguments as a new scene with no picture: each closed record through
/// the format's parser, the scene's begin and every step's checked admission and commit.
fn check(arguments: &[u8]) -> Verdict {
    let step = |error: StepError| Verdict::Refused(error.code.to_string(), error.path.to_string());
    let token = |error: TokenError| Verdict::Refused(error.to_string(), "-".to_owned());
    let mut tokenizer = Tokenizer::new();
    let mut rest = arguments;
    let mut scene: Option<SceneState> = None;
    let mut budget = GlobalBudget::default();
    let mut steps = 0;
    let mut ended = false;
    loop {
        let parsed = match tokenizer.next(&mut rest) {
            Ok(Some(record)) => parse_screen_draw_record(record.bytes),
            Ok(None) => match tokenizer.finish() {
                Ok(()) => break,
                Err(error) => return token(error),
            },
            Err(TokenError::AfterRecords) => break,
            Err(error) => return token(error),
        };
        let record = match parsed {
            Ok(record) => record,
            Err(error) => return step(error),
        };
        if ended {
            return step(StepError::new(ErrorCode::AfterEnd));
        }
        match (record, scene.as_mut()) {
            (Record::Begin(begin), None) if begin.surface == Surface::Screen => {
                return Verdict::Refused(CallError::NoScreenPicture.to_string(), "surface".into());
            }
            (Record::Begin(begin), None) => match SceneState::begin(begin) {
                Ok(state) => scene = Some(state),
                Err(error) => return step(error),
            },
            (Record::Step(next), Some(scene)) => {
                if let Err(error) = refuse_three_dimensional(&next) {
                    return step(error);
                }
                match scene.admit_checked(&next, &budget) {
                    Ok(Admission::New(candidate)) => {
                        if scene.commit(candidate, &mut budget).is_err() {
                            return Verdict::Refused("scene.StaleAdmission".into(), "-".into());
                        }
                        steps += 1;
                    }
                    Ok(Admission::Duplicate) => {}
                    Err(error) => return step(error),
                }
            }
            (Record::End(_), Some(_)) => ended = true,
            (Record::Begin(_), Some(_)) => {
                return Verdict::Refused("check.SecondBegin".into(), "begin".into());
            }
            (Record::Step(_) | Record::End(_), None) => {
                return step(StepError::new(ErrorCode::BeforeBegin));
            }
        }
    }
    match scene {
        Some(_) => Verdict::Drawn(steps),
        None => step(StepError::new(ErrorCode::BeforeBegin)),
    }
}

/// Request `number`'s line and whether its call was drawn.
fn line(
    number: usize,
    name: &str,
    bytes: usize,
    result: Result<(), Failure>,
    reply: &Reply,
    ms: u128,
) -> (String, bool) {
    let [status, code, param, word] = outcome(result);
    let (draw, drawn) = match (&result, &reply.draw) {
        (Err(_), _) => ("draw=-".to_owned(), false),
        (Ok(()), None) => ("draw=none".to_owned(), false),
        (Ok(()), Some(Err(()))) => ("draw=too-long".to_owned(), false),
        (Ok(()), Some(Ok(arguments))) => {
            let verdict = check(arguments.as_bytes());
            (verdict.words(), matches!(verdict, Verdict::Drawn(_)))
        }
    };
    (
        format!(
            "request={number} prompt={name} body_bytes={bytes} status={status} code={code} \
             param={param} result={word} calls={} {draw} ms={ms}",
            reply.calls
        ),
        drawn,
    )
}

/// The budget this run may spend: [`BUDGET`], or less when the live-budget variable says so; a
/// value that isn't a count, or isn't Unicode, allows nothing.
fn limit(value: Result<String, VarError>) -> u32 {
    match value {
        Ok(value) => value.trim().parse().unwrap_or(0).min(BUDGET),
        Err(VarError::NotPresent) => BUDGET,
        Err(VarError::NotUnicode(_)) => 0,
    }
}

/// The exit code that ends the run after a request, if any: a refused account, or a reply that
/// couldn't be read whole, which may hide a refusal (a 429 whose body was cut short).
fn stop(result: Result<(), Failure>) -> Option<u8> {
    match result {
        Err(Failure::Provider(error)) if account_refusal(error) => Some(REFUSED),
        Ok(()) | Err(Failure::Provider(_) | Failure::Incomplete | Failure::Spent) => None,
        Err(_) => Some(STOPPED),
    }
}

fn summary(limit: u32, attempted: usize, drawn: usize) -> String {
    format!(
        "summary model={DEFAULT_SLUG} effort=low instructions=short-fixed budget={limit} \
         attempted={attempted} drawn={drawn}/{}",
        PROMPTS.len()
    )
}

/// Runs the check and returns its exit code: 0 when every prompt was worked through, 10–17 as
/// the catalog capture's preflight (nothing sent), 18 when a body couldn't be built (nothing
/// sent), 20 once the session went stale, a reply couldn't be read whole or a line couldn't be
/// written, and 21 once the provider
/// refused the account (later requests unspent).
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
    let limit = limit(std::env::var(LIVE_BUDGET_ENV));
    let mut out = std::io::stdout();
    if limit == 0 {
        let dry = prepared
            .iter()
            .enumerate()
            .try_for_each(|(at, (name, built))| {
                writeln!(
                    out,
                    "request={} prompt={name} body_bytes={} result=dry",
                    at + 1,
                    built.bytes
                )
            });
        return match dry.and_then(|()| writeln!(out, "{}", summary(0, 0, 0))) {
            Ok(()) => 0,
            Err(_) => STOPPED,
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
        let (mut attempted, mut drawn) = (0, 0);
        for (at, (name, built)) in prepared.iter().enumerate() {
            let bearer: Secret<Zeroizing<String>> = match usable(
                account.as_ref().map(Secret::expose),
                SystemClock.unix_seconds(),
            ) {
                Ok(bearer) => bearer,
                Err(outcome) if at == 0 => return outcome.code(),
                Err(_) => return STOPPED,
            };
            let mut reply = Reply::default();
            let start = Instant::now();
            let result = stream(
                &client,
                &built.body,
                &bearer,
                std::future::ready(Some(())),
                &budget,
                |event: &SseEvent| keep(&mut reply, event),
            )
            .await;
            let ms = start.elapsed().as_millis();
            attempted += usize::from(!matches!(result, Err(Failure::Spent)));
            let (text, ok) = line(at + 1, name, built.bytes, result, &reply, ms);
            drawn += usize::from(ok);
            if writeln!(out, "{text}").is_err() {
                return STOPPED;
            }
            if let Some(code) = stop(result) {
                return code;
            }
        }
        match writeln!(out, "{}", summary(limit, attempted, drawn)) {
            Ok(()) => 0,
            Err(_) => STOPPED,
        }
    });
    drop(vault);
    code
}
