//! Debug builds only: `kumochid --image-check`, the bounded image check behind
//! `cargo xtask live image`. It sends at most three Responses requests (two in the diagnostic) on
//! the stored session,
//! never renewing it, each carrying one synthetic JPEG built here through the product's own
//! request path, and prints one line of counts per request. No screen, file or log is touched.

#[cfg(test)]
mod tests;

use std::io::Write as _;
use std::time::Instant;

use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use kumochi_protocol::Secret;
use kumochi_protocol::constants::{CREDENTIAL_SERVICE, DATA_DIR};
use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use zeroize::Zeroizing;

use super::catalog::DEFAULT_SLUG;
use super::clock::{Clock as _, SystemClock};
use super::error::VaultError;
use super::model::budget::Budget;
use super::model::conn::{ModelClient, Tls};
use super::model::error::Failure;
use super::model::sse::SseEvent;
use super::model::{ResponsesBody, stream};
use super::provenance::{Outcome, dev_names, output_path, usable};
use super::vault::Vault;
use crate::context::{BODY_CEILING, InputItem, Jpeg, ModelRequest, Role};
use crate::engines::responses::body;
use crate::platform::credential::NativeCredentialStore;
use crate::screen::JPEG_CAP;

/// The hard cap on model requests this process may send, retries and renewals included; the
/// owner's terms allow five, the check uses three and the diagnostic two.
const BUDGET: u32 = 5;
const _: () = assert!(BUDGET <= 5, "the owner's terms allow five image requests");
/// Exit code when an image or a body couldn't be built; nothing was sent.
const UNBUILT: u8 = 18;
/// Exit code when a request didn't complete, or the session went stale between requests.
const STOPPED: u8 = 20;
const INSTRUCTIONS: &str = "Answer with exactly two lowercase words.";
const PROMPT: &str = "Name the colour of the image's left half, then of its right half.";
/// The most answer text kept for the match.
const ANSWER_BYTES: usize = 256;
const QUALITY: u8 = 85;
/// Noise in each channel, out of 256, so the large images don't compress away while each half
/// stays plainly red or blue.
const NOISE: u32 = 64;
/// The noisy images' width, and the most their searched height may reach. At this width both
/// large images come out between square and 3:2, so a refusal can't be about an extreme shape.
const WIDTH: u32 = 2048;
const TOP: u32 = 4096;
/// Room left in the near-ceiling body for everything but the image.
const SLACK: usize = 16 * 1024;

/// Which images a run sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Set {
    /// Three: the small flat image, then noisy ones at the JPEG cap and near the body ceiling.
    Check,
    /// Two, to tell "too small" from "too flat": the small image with the large ones' noise, and
    /// the flat halves at 1024×1024.
    Diagnostic,
}

/// Runs `set` and returns its exit code: 0 when all its requests completed, 10–17 as the
/// catalog capture's preflight (nothing sent), 18 when nothing could be built (nothing sent), and
/// 20 once a request didn't complete or the session went stale (later ones unspent).
pub fn run(set: Set) -> u8 {
    let located = std::env::current_exe()
        .ok()
        .and_then(|exe| output_path(&exe))
        .is_some();
    if !dev_names(CREDENTIAL_SERVICE, DATA_DIR) || !located {
        return Outcome::Preflight.code();
    }
    let Some(requests) = requests(set) else {
        return UNBUILT;
    };
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
    let budget = Budget::limited(Some(BUDGET));
    let code = runtime.block_on(async {
        for (number, (kind, size, jpeg_bytes, body, body_bytes)) in requests.iter().enumerate() {
            let bearer = match usable(
                account.as_ref().map(Secret::expose),
                SystemClock.unix_seconds(),
            ) {
                Ok(bearer) => bearer,
                Err(outcome) if number == 0 => return outcome.code(),
                Err(_) => return STOPPED,
            };
            let mut answer = Zeroizing::new(String::new());
            let mut input_tokens = None;
            let start = Instant::now();
            let result = stream(
                &client,
                body,
                &bearer,
                std::future::ready(Some(())),
                &budget,
                |event: &SseEvent| keep(&mut answer, &mut input_tokens, event),
            )
            .await
            .map(|()| (matches(&answer), input_tokens));
            let line = report(
                number + 1,
                kind,
                *size,
                *jpeg_bytes,
                *body_bytes,
                start.elapsed().as_millis(),
                result,
            );
            if writeln!(std::io::stdout(), "{line}").is_err() || result.is_err() {
                return STOPPED;
            }
        }
        0
    });
    drop(vault);
    code
}

/// One request, built before anything is sent: kind, image size, JPEG bytes, body and body bytes.
type Prepared = (&'static str, [u32; 2], usize, ResponsesBody, usize);

/// One image to send: kind, size and JPEG.
type Image = (&'static str, [u32; 2], Vec<u8>);

/// `set`'s images, in the order they're sent: kind, size and JPEG; nothing when one misses its
/// size.
fn images(set: Set) -> Option<Vec<Image>> {
    Some(match set {
        Set::Check => {
            let near_ceiling = (BODY_CEILING - SLACK) / 4 * 3;
            let (cap, cap_height) = sized(WIDTH, TOP, JPEG_CAP)?;
            let (ceiling, ceiling_height) = sized(WIDTH, TOP, near_ceiling)?;
            vec![
                ("small", [256, 256], halves(256, 256, 0)?),
                ("cap", [WIDTH, cap_height], cap),
                ("ceiling", [WIDTH, ceiling_height], ceiling),
            ]
        }
        Set::Diagnostic => vec![
            ("small-noisy", [256, 256], halves(256, 256, NOISE)?),
            ("flat-large", [1024, 1024], halves(1024, 1024, 0)?),
        ],
    })
}

/// `set`'s requests; nothing when an image misses its size or a body is refused.
fn requests(set: Set) -> Option<Vec<Prepared>> {
    let model = ModelSlug::new(DEFAULT_SLUG).ok()?;
    images(set)?
        .into_iter()
        .map(|(kind, size, jpeg)| {
            let jpeg_bytes = jpeg.len();
            let mut request = ModelRequest {
                choice: Some(ModelChoice {
                    model: model.clone(),
                    effort: Effort::Low,
                    connection: ConnectionKind::ChatGpt,
                }),
                instructions: INSTRUCTIONS.to_owned(),
                input: vec![InputItem::Message {
                    role: Role::User,
                    text: PROMPT.to_owned(),
                }],
                tools: Vec::new(),
            };
            request.attach(Jpeg::new(jpeg)?).ok()?;
            let value = body(&request).ok()?;
            let body_bytes = value.to_string().len();
            Some((
                kind,
                size,
                jpeg_bytes,
                ResponsesBody::new(&value).ok()?,
                body_bytes,
            ))
        })
        .collect()
}

/// The tallest noisy two-colour JPEG `width` wide and at most `top` high with at most `cap`
/// bytes, and its height; nothing unless it comes within 2% of `cap`, so a request never goes
/// out smaller than its line says.
fn sized(width: u32, top: u32, cap: usize) -> Option<(Vec<u8>, u32)> {
    let (mut low, mut high, mut best) = (16_u32, top, None);
    while low <= high {
        let height = low + (high - low) / 2;
        let jpeg = halves(width, height, NOISE)?;
        if jpeg.len() <= cap {
            low = height + 1;
            best = Some((jpeg, height));
        } else {
            high = height - 1;
        }
    }
    best.filter(|(jpeg, _)| jpeg.len() >= cap - cap / 50)
}

/// A JPEG whose left half is red and right half blue, each channel moved by up to `noise`.
fn halves(width: u32, height: u32, noise: u32) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity(usize::try_from(width * height * 3).ok()?);
    for y in 0..height {
        for x in 0..width {
            let [a, b, c] = [0, 1, 2].map(|channel| {
                let hashed = jitter(x * 3 + channel, y);
                u8::try_from(u32::from(hashed) * noise / 256).unwrap_or(0)
            });
            rgb.extend(if x < width / 2 {
                [255 - a, b, c]
            } else {
                [a, b, 255 - c]
            });
        }
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, QUALITY)
        .encode(&rgb, width, height, ExtendedColorType::Rgb8)
        .ok()?;
    Some(jpeg)
}

/// A fixed pseudo-random byte for a position, so every image is the same on every run.
fn jitter(x: u32, y: u32) -> u8 {
    let mut hash = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x2C1B_3C6D);
    hash ^= hash >> 12;
    hash.to_le_bytes()[0]
}

/// Keeps the answer's text, up to [`ANSWER_BYTES`], and the completed response's input token
/// count, so a line can show whether the image was counted.
fn keep(answer: &mut String, input_tokens: &mut Option<u64>, event: &SseEvent) {
    match event.kind() {
        "response.output_text.delta" => {
            if let Some(delta) = event.value()["delta"].as_str()
                && answer.len() + delta.len() <= ANSWER_BYTES
            {
                answer.push_str(delta);
            }
        }
        "response.completed" => {
            *input_tokens = event.value()["response"]["usage"]["input_tokens"].as_u64();
        }
        _ => {}
    }
}

/// Whether the answer names red, then blue, and nothing else.
fn matches(answer: &str) -> bool {
    let words: Vec<String> = answer
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words == ["red", "blue"]
}

/// One request's line, from counts and fixed words only: never the answer or a provider string.
fn report(
    number: usize,
    kind: &str,
    [width, height]: [u32; 2],
    jpeg_bytes: usize,
    body_bytes: usize,
    ms: u128,
    result: Result<(bool, Option<u64>), Failure>,
) -> String {
    let none = || "-".to_owned();
    let (status, code, param, outcome, tokens, matched) = match result {
        Ok((matched, tokens)) => (
            "2xx".to_owned(),
            none(),
            none(),
            "completed",
            tokens.map_or_else(none, |tokens| tokens.to_string()),
            matched.to_string(),
        ),
        Err(Failure::Provider(error)) => (
            error.status.to_string(),
            error.code.map_or_else(none, |code| format!("{code:?}")),
            error.param.map_or_else(none, |param| format!("{param:?}")),
            "refused",
            none(),
            none(),
        ),
        Err(failure) => (none(), none(), none(), kind_of(failure), none(), none()),
    };
    format!(
        "request={number} image={kind} size={width}x{height} jpeg_bytes={jpeg_bytes} \
         body_bytes={body_bytes} status={status} code={code} param={param} result={outcome} \
         input_tokens={tokens} ms={ms} content_match={matched}"
    )
}

fn kind_of(failure: Failure) -> &'static str {
    match failure {
        Failure::Provider(_) => "refused",
        Failure::Incomplete => "incomplete",
        Failure::Truncated => "truncated",
        Failure::Protocol => "protocol",
        Failure::Rebuild => "rebuild",
        Failure::Spent => "spent",
        Failure::Transport => "transport",
        Failure::Request => "request",
        Failure::Credential => "credential",
    }
}
