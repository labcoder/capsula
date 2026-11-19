//! Debug builds only: `kumochid --catalog-provenance`, the one-shot capture behind
//! `cargo xtask live catalog-provenance`. It sends one model-list request with the stored
//! session, never renewing it, and writes a scrubbed copy of the answer into its run folder.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{
    BRAIN_EXE, CATALOG_PROVENANCE_FILE, CREDENTIAL_SERVICE, DATA_DIR, ProductIdentity,
};
use kumochi_protocol::message::{ModelName, ModelSlug};
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
use zeroize::Zeroizing;

use super::clock::{Clock as _, SystemClock};
use super::error::VaultError;
use super::http_client::{self, Endpoint, Http, HttpError, MAX_BODY_BYTES, Request, Response};
use super::records::Account;
use super::vault::Vault;
use crate::platform::credential::NativeCredentialStore;

/// The approved observation keys, matched exactly; any other key becomes `<key-n>`, its index
/// in its own object.
const KEYS: [&str; 15] = [
    "models",
    "slug",
    "display_name",
    "visibility",
    "supported_reasoning_levels",
    "supported_reasoning_efforts",
    "supported_efforts",
    "reasoning_levels",
    "reasoning_efforts",
    "default_reasoning_level",
    "default_reasoning_effort",
    "default_effort",
    "reasoning_effort",
    "effort",
    "input_modalities",
];
/// The strings written as they are, compared without ASCII case; any other becomes
/// `<string:N>`, N its byte length.
const WORDS: [&str; 15] = [
    "list",
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "x-high",
    "x_high",
    "extra-high",
    "extra_high",
    "max",
    "maximum",
    "text",
    "image",
];
/// The renewal's 60 s early margin plus the client's 120 s total limit, so the bearer outlives the
/// request.
const FRESH_S: u64 = 180;
/// Nesting levels, the body's own object first [local policy].
const MAX_DEPTH: usize = 32;
/// Values in the body [guess: 200 models with nested metadata stay near 10^4].
const MAX_NODES: usize = 65_536;

/// How the capture ended. Its code is the exit code: 10–19 sent nothing, and 0 and 20–29 sent
/// one catalog request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    /// The file was written.
    Written = 0,
    /// Not the dev names, not in a `live\<digits>\bin` folder, or the file already exists. A
    /// brain built without the mode exits with this code too.
    Preflight = 10,
    /// Another brain holds the session lock.
    Busy = 11,
    /// The vault couldn't be opened or read.
    Vault = 12,
    /// No account or no tokens; a pending sign-out reads as none.
    NoAccount = 13,
    /// The plan isn't enabled.
    Plan = 14,
    /// A sent renewal was never answered, or an identity check is pending.
    Unsettled = 15,
    /// No wall clock, or the bearer expires within 180 s.
    Due = 16,
    /// No async runtime or account client.
    NoClient = 17,
    /// The request failed, a body over 1 MiB included.
    Request = 20,
    /// 401.
    Unauthorized = 21,
    /// 403.
    Forbidden = 22,
    /// 429.
    RateLimited = 23,
    /// 5xx.
    ServerError = 24,
    /// Any other status but 200.
    Status = 25,
    /// A 200 whose body isn't a JSON object.
    NotObject = 26,
    /// Over a scrub or output bound.
    Bound = 27,
    /// The file wasn't written.
    NotWritten = 28,
    /// The counting wrapper refused the request.
    Refused = 29,
}

impl Outcome {
    /// The process exit code.
    #[expect(
        clippy::as_conversions,
        reason = "a fieldless repr(u8) enum's discriminant is its exit code"
    )]
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Captures the catalog once. The vault, and the session lock with it, is held until the file
/// is written, so no other dev brain can renew the session meanwhile.
pub fn run() -> Outcome {
    if !dev_names(CREDENTIAL_SERVICE, DATA_DIR) {
        return Outcome::Preflight;
    }
    let Some(out) = std::env::current_exe()
        .ok()
        .and_then(|exe| output_path(&exe))
    else {
        return Outcome::Preflight;
    };
    if out.try_exists().ok() != Some(false) {
        return Outcome::Preflight;
    }
    let vault = match Vault::open(NativeCredentialStore) {
        Ok(vault) => vault,
        Err(VaultError::Busy) => return Outcome::Busy,
        Err(_) => return Outcome::Vault,
    };
    let Ok(account) = vault.load_account() else {
        return Outcome::Vault;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Outcome::NoClient;
    };
    let Ok(client) = http_client::build() else {
        return Outcome::NoClient;
    };
    let bearer = match usable(
        account.as_ref().map(Secret::expose),
        SystemClock.unix_seconds(),
    ) {
        Ok(bearer) => bearer,
        Err(outcome) => return outcome,
    };
    drop(account);
    let http = Counted::new(client);
    let outcome = match runtime.block_on(capture(&http, bearer, OffsetDateTime::now_utc().date())) {
        Ok(bytes) => write(&out, &bytes),
        Err(outcome) => outcome,
    };
    drop(vault);
    outcome
}

/// Whether the build resolved the dev credential service and data folder, so the capture can
/// never touch an installed Kumochi's account.
pub(super) fn dev_names(service: &str, data_dir: &str) -> bool {
    let dev = ProductIdentity::DEV;
    service == dev.credential_service && data_dir == dev.data_dir
}

/// `<run>\catalog-provenance.json` for an executable at `<run>\bin\kumochid.exe`, where `<run>`
/// is `...\live\<digits>`.
pub(super) fn output_path(exe: &Path) -> Option<PathBuf> {
    let parts: Vec<Component<'_>> = exe.components().collect();
    let [
        ..,
        Component::Normal(live),
        Component::Normal(run),
        Component::Normal(bin),
        Component::Normal(name),
    ] = parts[..]
    else {
        return None;
    };
    let digits = run.to_str()?.bytes().all(|byte| byte.is_ascii_digit());
    if live != "live" || !digits || bin != "bin" || name != BRAIN_EXE {
        return None;
    }
    Some(exe.parent()?.parent()?.join(CATALOG_PROVENANCE_FILE))
}

/// The bearer, only for a settled set that outlives the request; anything else stops before a
/// request, since this mode never renews a session.
pub(super) fn usable(
    account: Option<&Account>,
    now: Option<u64>,
) -> Result<Secret<Zeroizing<String>>, Outcome> {
    let account = account.ok_or(Outcome::NoAccount)?;
    let tokens = account.tokens.as_ref().ok_or(Outcome::NoAccount)?;
    if !account.plan_enabled {
        return Err(Outcome::Plan);
    }
    if tokens.refresh_sent || tokens.pending_identity_validation {
        return Err(Outcome::Unsettled);
    }
    if now.is_none_or(|now| tokens.expires_at <= now.saturating_add(FRESH_S)) {
        return Err(Outcome::Due);
    }
    Ok(Secret::new(tokens.access_token.clone()))
}

/// The capture's only [`Http`]: one catalog request at most, no auth request, nothing else.
/// Each is counted before it's sent, so a failed attempt is never free.
struct Counted<H> {
    http: H,
    auth: AtomicU32,
    catalog: AtomicU32,
}

impl<H> Counted<H> {
    fn new(http: H) -> Self {
        Self {
            http,
            auth: AtomicU32::new(0),
            catalog: AtomicU32::new(0),
        }
    }
}

impl<H: Http> Http for Counted<H> {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        let (sent, cap) = match &request.endpoint {
            Endpoint::Models(_) => (&self.catalog, 1),
            Endpoint::Token | Endpoint::Discovery | Endpoint::Jwks(_) | Endpoint::Revocation(_) => {
                (&self.auth, 0)
            }
            Endpoint::GitHub(_) => return Err(HttpError::Request),
        };
        sent.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < cap).then_some(n + 1)
        })
        .map_err(|_| HttpError::Request)?;
        self.http.execute(request).await
    }
}

/// Sends the one request and builds the file: a header of counts and the UTC date, then the
/// scrubbed body, pretty-printed.
async fn capture(
    http: &Counted<impl Http>,
    bearer: Secret<Zeroizing<String>>,
    date: Date,
) -> Result<Vec<u8>, Outcome> {
    // Only a spent budget makes the wrapper refuse a Models request; the transport's own
    // refusals are failed requests.
    let spent = http.catalog.load(Ordering::SeqCst) > 0;
    let response = match http
        .execute(Request {
            endpoint: Endpoint::Models(bearer),
            fields: Vec::new(),
        })
        .await
    {
        Ok(response) => response,
        Err(HttpError::Request) if spent => return Err(Outcome::Refused),
        Err(_) => return Err(Outcome::Request),
    };
    match response.status {
        200 => {}
        401 => return Err(Outcome::Unauthorized),
        403 => return Err(Outcome::Forbidden),
        429 => return Err(Outcome::RateLimited),
        500..=599 => return Err(Outcome::ServerError),
        _ => return Err(Outcome::Status),
    }
    let body_bytes = response.body.expose().len();
    // shortcut: the parsed Value's strings aren't zeroized (as in catalog::read), switch to a borrowing visitor before this endpoint can carry private content
    let Ok(parsed @ Value::Object(_)) = serde_json::from_slice(response.body.expose()) else {
        return Err(Outcome::NotObject);
    };
    drop(response);
    let (body, counts) = scrub(&parsed)?;
    drop(parsed);
    let file = json!({
        "format": "catalog-provenance/1",
        "captured_utc_date": date.to_string(),
        "requests": {
            "auth": http.auth.load(Ordering::SeqCst),
            "catalog": http.catalog.load(Ordering::SeqCst),
        },
        "body_bytes": body_bytes,
        "scrubbed": {
            "entries": counts.entries,
            "keys": counts.keys,
            "strings": counts.strings,
            "numbers": counts.numbers,
        },
        "body": body,
    });
    let bytes = serde_json::to_vec_pretty(&file).expect("a JSON value with string keys serializes");
    if bytes.len() > MAX_BODY_BYTES {
        return Err(Outcome::Bound);
    }
    Ok(bytes)
}

/// What the scrub replaced: `entries` counts the `models` objects whose slug and name it
/// renamed, the rest count placeholders.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    entries: usize,
    keys: usize,
    strings: usize,
    numbers: usize,
}

/// The rule a value falls under: the catalog reader's three fields get theirs, so the scrubbed
/// body still replays through `catalog::read`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    Root,
    Models,
    Entry(usize),
    Slug,
    Name(usize),
    Plain,
}

/// The scrub's state. It holds the body's slugs, so it has no Debug.
struct Scrubber<'a> {
    nodes: usize,
    counts: Counts,
    slugs: HashMap<&'a str, usize>,
}

/// Rewrites `body` so only approved keys, closed words, integers 0–16, booleans, null and fixed
/// placeholders remain.
fn scrub(body: &Value) -> Result<(Value, Counts), Outcome> {
    let mut scrubber = Scrubber {
        nodes: 0,
        counts: Counts::default(),
        slugs: HashMap::new(),
    };
    let body = scrubber.value(body, 1, Rule::Root)?;
    Ok((body, scrubber.counts))
}

impl<'a> Scrubber<'a> {
    fn value(&mut self, value: &'a Value, depth: usize, rule: Rule) -> Result<Value, Outcome> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(Outcome::Bound);
        }
        Ok(match (value, rule) {
            (Value::String(slug), Rule::Slug) => Value::String(if ModelSlug::new(slug).is_ok() {
                let next = self.slugs.len();
                format!("m-{}", self.slugs.entry(slug).or_insert(next))
            } else {
                "<invalid>".to_owned()
            }),
            (Value::String(name), Rule::Name(index)) => {
                Value::String(if ModelName::new(name).is_ok() {
                    format!("Model {index}")
                } else {
                    String::new()
                })
            }
            (Value::String(text), _)
                if WORDS.iter().any(|word| text.eq_ignore_ascii_case(word)) =>
            {
                value.clone()
            }
            (Value::String(text), _) => {
                self.counts.strings += 1;
                Value::String(format!("<string:{}>", text.len()))
            }
            (Value::Number(number), _) => {
                if let Some(small @ 0..=16) = number.as_u64() {
                    small.into()
                } else {
                    self.counts.numbers += 1;
                    (-1).into()
                }
            }
            (Value::Null | Value::Bool(_), _) => value.clone(),
            (Value::Array(items), _) => items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let child = if rule == Rule::Models {
                        Rule::Entry(index)
                    } else {
                        Rule::Plain
                    };
                    self.value(item, depth + 1, child)
                })
                .collect::<Result<_, _>>()?,
            (Value::Object(map), _) => {
                if let Rule::Entry(_) = rule {
                    self.counts.entries += 1;
                }
                map.iter()
                    .enumerate()
                    .map(|(index, (key, item))| {
                        let child = match (rule, key.as_str()) {
                            (Rule::Root, "models") => Rule::Models,
                            (Rule::Entry(_), "slug") => Rule::Slug,
                            (Rule::Entry(entry), "display_name") => Rule::Name(entry),
                            _ => Rule::Plain,
                        };
                        let key = if KEYS.contains(&key.as_str()) {
                            key.clone()
                        } else {
                            self.counts.keys += 1;
                            format!("<key-{index}>")
                        };
                        Ok((key, self.value(item, depth + 1, child)?))
                    })
                    .collect::<Result<_, _>>()?
            }
        })
    }
}

/// Writes `bytes` to a new file at `out`, never over an existing one.
fn write(out: &Path, bytes: &[u8]) -> Outcome {
    let Ok(mut file) = File::create_new(out) else {
        return Outcome::NotWritten;
    };
    if file.write_all(bytes).and_then(|()| file.sync_all()).is_ok() {
        return Outcome::Written;
    }
    drop(file);
    // The exit is 28 either way, and a partial file holds only scrubbed bytes.
    let _best_effort = fs::remove_file(out);
    Outcome::NotWritten
}
