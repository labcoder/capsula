use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{CREDENTIAL_SERVICE, DATA_DIR, ProductIdentity};
use serde_json::{Map, Value, json};
use time::{Date, Month};
use zeroize::Zeroizing;

use super::{Counted, Counts, Outcome, capture, dev_names, output_path, scrub, usable, write};
use crate::connections::catalog;
use crate::connections::github::{GitHubEndpoint, GitHubToken};
use crate::connections::http_client::{
    Endpoint, Http, HttpError, IssuerUrl, Request, Response, ResponseMeta,
};
use crate::connections::records::{Account, Tokens};
use crate::test_support::Scratch;

const MARKER: &str = "ZqxPrivatePrompt";
const NOW: u64 = 1_762_460_000;

/// Plays its replies in order and counts the requests that reach it.
struct Script {
    replies: Mutex<VecDeque<Result<Response, HttpError>>>,
    calls: AtomicUsize,
}

impl Script {
    fn new(replies: impl IntoIterator<Item = Result<Response, HttpError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Http for Script {
    fn execute(
        &self,
        _request: Request,
    ) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.replies.lock().expect("replies").pop_front();
        std::future::ready(reply.expect("a scripted reply"))
    }
}

fn answer(status: u16, body: &[u8]) -> Response {
    Response {
        status,
        request_id: None,
        meta: ResponseMeta::default(),
        body: Secret::new(Zeroizing::new(body.to_vec())),
    }
}

fn bearer() -> Secret<Zeroizing<String>> {
    Secret::new(Zeroizing::new("planted-bearer".to_owned()))
}

fn date() -> Date {
    Date::from_calendar_date(2026, Month::November, 7).expect("a real date")
}

fn request(endpoint: Endpoint) -> Request {
    Request {
        endpoint,
        fields: Vec::new(),
    }
}

/// What the wrapper answered, by status.
async fn sent(http: &Counted<Script>, endpoint: Endpoint) -> Result<u16, HttpError> {
    http.execute(request(endpoint))
        .await
        .map(|response| response.status)
}

fn counters(http: &Counted<Script>) -> (u32, u32, usize) {
    (
        http.auth.load(Ordering::SeqCst),
        http.catalog.load(Ordering::SeqCst),
        http.http.calls.load(Ordering::SeqCst),
    )
}

async fn captured(status: u16, body: &[u8]) -> Result<Vec<u8>, Outcome> {
    let http = Counted::new(Script::new([Ok(answer(status, body))]));
    capture(&http, bearer(), date()).await
}

fn account(edit: impl FnOnce(&mut Account)) -> Account {
    let mut account = Account {
        client_id: Zeroizing::new("client".to_owned()),
        subject: None,
        email: None,
        plan_enabled: true,
        tokens: Some(Tokens {
            access_token: Zeroizing::new("planted-bearer".to_owned()),
            refresh_token: Zeroizing::new("refresh".to_owned()),
            id_token: None,
            scope: Zeroizing::new("openid".to_owned()),
            expires_at: NOW + 181,
            pending_identity_validation: false,
            refresh_sent: false,
            connection: 0,
        }),
    };
    edit(&mut account);
    account
}

fn tokens(account: &mut Account) -> &mut Tokens {
    account.tokens.as_mut().expect("the fixture has tokens")
}

#[test]
fn each_outcome_keeps_its_exit_code() {
    for (outcome, code) in [
        (Outcome::Written, 0),
        (Outcome::Preflight, 10),
        (Outcome::Busy, 11),
        (Outcome::Vault, 12),
        (Outcome::NoAccount, 13),
        (Outcome::Plan, 14),
        (Outcome::Unsettled, 15),
        (Outcome::Due, 16),
        (Outcome::NoClient, 17),
        (Outcome::Request, 20),
        (Outcome::Unauthorized, 21),
        (Outcome::Forbidden, 22),
        (Outcome::RateLimited, 23),
        (Outcome::ServerError, 24),
        (Outcome::Status, 25),
        (Outcome::NotObject, 26),
        (Outcome::Bound, 27),
        (Outcome::NotWritten, 28),
        (Outcome::Refused, 29),
    ] {
        assert_eq!(outcome.code(), code, "{outcome:?}");
    }
}

#[test]
fn only_the_dev_names_pass_the_guard() {
    let (dev, release) = (ProductIdentity::DEV, ProductIdentity::RELEASE);
    assert!(dev_names(dev.credential_service, dev.data_dir));
    assert!(
        dev_names(CREDENTIAL_SERVICE, DATA_DIR),
        "a debug build resolves the dev names"
    );
    for (service, data_dir) in [
        (release.credential_service, release.data_dir),
        (dev.credential_service, release.data_dir),
        (release.credential_service, dev.data_dir),
    ] {
        assert!(!dev_names(service, data_dir), "{service} with {data_dir}");
    }
}

#[test]
fn only_a_settled_set_that_outlives_the_request_gives_the_bearer() {
    for (stored, now, want) in [
        (None, Some(NOW), Outcome::NoAccount),
        (
            Some(account(|a| a.tokens = None)),
            Some(NOW),
            Outcome::NoAccount,
        ),
        (
            Some(account(|a| a.plan_enabled = false)),
            Some(NOW),
            Outcome::Plan,
        ),
        (
            Some(account(|a| tokens(a).refresh_sent = true)),
            Some(NOW),
            Outcome::Unsettled,
        ),
        (
            Some(account(|a| tokens(a).pending_identity_validation = true)),
            Some(NOW),
            Outcome::Unsettled,
        ),
        (Some(account(|_| {})), None, Outcome::Due),
        (
            Some(account(|a| tokens(a).expires_at = NOW + 180)),
            Some(NOW),
            Outcome::Due,
        ),
    ] {
        let got = usable(stored.as_ref(), now).map(|bearer| bearer.expose().to_string());
        assert_eq!(got, Err(want));
    }
    let fresh = account(|_| {});
    let got = usable(Some(&fresh), Some(NOW)).map(|bearer| bearer.expose().to_string());
    assert_eq!(got, Ok("planted-bearer".to_owned()));
}

#[tokio::test]
async fn the_wrapper_lets_one_catalog_request_through_and_no_other() {
    let http = Counted::new(Script::new([
        Ok(answer(200, b"{}")),
        Ok(answer(200, b"{}")),
    ]));
    assert_eq!(sent(&http, Endpoint::Models(bearer())).await, Ok(200));
    assert_eq!(
        sent(&http, Endpoint::Models(bearer())).await,
        Err(HttpError::Request)
    );
    let issuer = |path: &str| IssuerUrl::parse(&format!("https://auth.openai.com/{path}"));
    let token = GitHubToken::new(Zeroizing::new("ghu_fake".to_owned())).expect("synthetic token");
    for endpoint in [
        Endpoint::Token,
        Endpoint::Discovery,
        Endpoint::Jwks(issuer("jwks").expect("issuer URL")),
        Endpoint::Revocation(issuer("revoke").expect("issuer URL")),
        Endpoint::GitHub(GitHubEndpoint::User {
            token: Secret::new(token),
        }),
    ] {
        assert_eq!(sent(&http, endpoint).await, Err(HttpError::Request));
    }
    assert_eq!(counters(&http), (0, 1, 1));
}

#[tokio::test]
async fn a_failed_attempt_still_spends_the_catalog_request() {
    let http = Counted::new(Script::new([
        Err(HttpError::Connect),
        Ok(answer(200, b"{}")),
    ]));
    assert_eq!(
        sent(&http, Endpoint::Models(bearer())).await,
        Err(HttpError::Connect)
    );
    assert_eq!(
        sent(&http, Endpoint::Models(bearer())).await,
        Err(HttpError::Request)
    );
    assert_eq!(counters(&http), (0, 1, 1));
}

#[tokio::test]
async fn a_failed_request_or_refusal_gives_its_own_code_and_no_file() {
    for (status, want) in [
        (401, Outcome::Unauthorized),
        (403, Outcome::Forbidden),
        (429, Outcome::RateLimited),
        (503, Outcome::ServerError),
        (418, Outcome::Status),
    ] {
        assert_eq!(
            captured(status, b"{\"models\":[]}").await,
            Err(want),
            "{status}"
        );
    }
    for error in [
        HttpError::Timeout,
        HttpError::BodyTooLarge,
        HttpError::Request,
    ] {
        let http = Counted::new(Script::new([Err(error)]));
        assert_eq!(
            capture(&http, bearer(), date()).await,
            Err(Outcome::Request)
        );
        assert_eq!(
            capture(&http, bearer(), date()).await,
            Err(Outcome::Refused)
        );
        assert_eq!(counters(&http), (0, 1, 1));
    }
    let bodies: [&[u8]; 5] = [b"", b"[]", b"\"x\"", b"{\"models\":[", &[0xff, 0xfe]];
    for body in bodies {
        assert_eq!(
            captured(200, body).await,
            Err(Outcome::NotObject),
            "{body:?}"
        );
    }
}

#[test]
fn only_approved_keys_closed_words_and_small_integers_stay() {
    // Keys are in byte order, so serde_json's map order is the same with or without
    // preserve_order.
    let body = json!({
        "models": [{
            "Slug": "x",
            "Supported_Reasoning_Levels": "high",
            "display_name": "GPT X",
            "display_name2": 3,
            "models_": [0, 16, 17, -1, 1.5, 1e30, true, false, null],
            "slug": "gpt-x",
            "supported_reasoning_levels":
                ["Low", "MEDIUM", "xhigh", "Extra_High", MARKER, format!("LOW {MARKER}")],
            "visibility": "list",
        }],
        "slug": "gpt-x",
    });
    let want = json!({
        "models": [{
            "<key-0>": "<string:1>",
            "<key-1>": "high",
            "display_name": "Model 0",
            "<key-3>": 3,
            "<key-4>": [0, 16, -1, -1, -1, -1, true, false, null],
            "slug": "m-0",
            "supported_reasoning_levels":
                ["Low", "MEDIUM", "xhigh", "Extra_High", "<string:16>", "<string:20>"],
            "visibility": "list",
        }],
        "slug": "<string:5>",
    });
    let counts = Counts {
        entries: 1,
        keys: 4,
        strings: 4,
        numbers: 4,
    };
    assert_eq!(scrub(&body), Ok((want, counts)));
}

/// The list's image-input field keeps its name and its text and image words; other words don't.
#[test]
fn input_modalities_stay_with_their_text_and_image_words() {
    let body = json!({ "models": [{
        "display_name": "GPT X",
        "input_modalities": ["text", "IMAGE", "audio", MARKER],
        "slug": "gpt-x",
        "visibility": "list",
    }]});
    let want = json!({ "models": [{
        "display_name": "Model 0",
        "input_modalities": ["text", "IMAGE", "<string:5>", "<string:16>"],
        "slug": "m-0",
        "visibility": "list",
    }]});
    let counts = Counts {
        entries: 1,
        keys: 0,
        strings: 2,
        numbers: 0,
    };
    assert_eq!(scrub(&body), Ok((want, counts)));
}

#[tokio::test]
async fn no_planted_marker_reaches_the_written_bytes() {
    let wide: Vec<String> = (0..65).map(|i| format!("\"{MARKER}{i:02}\":1")).collect();
    let raw = format!(
        r#"{{"models":[{{"{MARKER}":"low","display_name":"{MARKER}","slug":"zqxprivateprompt-1","visibility":"list"}},{{"{MARKER}":{{"effort":"high"}},"description":{{"text":"{MARKER}"}},"display_name":"Name","slug":"{MARKER}!","visibility":"list"}}],"notes":"\u005aqxPrivatePrompt","wide":{{{}}}}}"#,
        wide.join(",")
    );
    let bytes = captured(200, raw.as_bytes()).await.expect("written");

    let text = String::from_utf8(bytes.clone()).expect("UTF-8");
    assert!(text.starts_with("{\n  \""), "pretty-printed");
    let marker = MARKER.to_ascii_lowercase();
    assert!(!text.to_ascii_lowercase().contains(&marker), "{text}");
    let lower = bytes.to_ascii_lowercase();
    assert!(!lower.windows(marker.len()).any(|w| w == marker.as_bytes()));
    for planted in [MARKER, marker.as_str()] {
        let list = format!("{:?}", planted.as_bytes());
        let list = list.trim_start_matches('[').trim_end_matches(']');
        assert!(
            !format!("{bytes:?}").contains(list),
            "{planted} as a byte list"
        );
    }

    let wide: Map<String, Value> = (0..65).map(|i| (format!("<key-{i}>"), json!(1))).collect();
    let want = json!({
        "format": "catalog-provenance/1",
        "captured_utc_date": "2026-11-07",
        "requests": { "auth": 0, "catalog": 1 },
        "body_bytes": raw.len(),
        "scrubbed": { "entries": 2, "keys": 71, "strings": 2, "numbers": 0 },
        "body": {
            "models": [
                { "<key-0>": "low", "display_name": "Model 0", "slug": "m-0", "visibility": "list" },
                {
                    "<key-0>": { "effort": "high" },
                    "<key-1>": { "<key-0>": "<string:16>" },
                    "display_name": "Model 1",
                    "slug": "<invalid>",
                    "visibility": "list",
                },
            ],
            "<key-1>": "<string:16>",
            "<key-2>": wide,
        },
    });
    assert_eq!(serde_json::from_slice::<Value>(&bytes).expect("JSON"), want);
}

#[tokio::test]
async fn each_bound_holds_at_its_limit_and_refuses_one_over() {
    let nested = |depth: usize| {
        let mut value = json!([]);
        for _ in 2..depth {
            value = json!([value]);
        }
        json!({ "models": value })
    };
    assert_eq!(scrub(&nested(32)), Ok((nested(32), Counts::default())));
    assert_eq!(scrub(&nested(33)), Err(Outcome::Bound));

    let flat = |nodes: usize| json!({ "models": vec![0; nodes - 2] });
    assert_eq!(scrub(&flat(65_536)), Ok((flat(65_536), Counts::default())));
    assert_eq!(scrub(&flat(65_537)), Err(Outcome::Bound));

    // Kept words only, so the size is the documented pretty JSON's: 61,665 words come to 1 MiB
    // and 8 bytes, and each "medium" in place of a "maximum" is one byte less.
    let words = |medium: usize| {
        let mut words = vec!["\"maximum\""; 61_665];
        words[..medium].fill("\"medium\"");
        format!("{{\"models\":[{}]}}", words.join(","))
    };
    let exact = captured(200, words(8).as_bytes())
        .await
        .expect("exactly 1 MiB");
    assert_eq!(exact.len(), 1_048_576);
    assert_eq!(
        captured(200, words(7).as_bytes()).await.err(),
        Some(Outcome::Bound)
    );
}

/// Where `catalog::read` found each admitted model, from its numbered name.
async fn admitted(body: &Value, prefix: &str) -> Vec<usize> {
    let http = Script::new([Ok(answer(200, body.to_string().as_bytes()))]);
    let listing = catalog::read(&http, bearer()).await.expect("a model list");
    listing
        .models
        .iter()
        .map(|model| {
            let index = model.display_name.as_str().strip_prefix(prefix);
            index
                .and_then(|index| index.parse().ok())
                .expect("a numbered name")
        })
        .collect()
}

#[tokio::test]
async fn the_scrubbed_body_admits_the_same_models_at_the_same_positions() {
    let entry = |name: &str, slug: Value, visibility: &str| {
        json!({
            "display_name": name,
            "slug": slug,
            "visibility": visibility,
            "supported_reasoning_levels": [{"effort": "low"}],
        })
    };
    let mut models = vec![
        entry("Original 0", json!("gpt-a"), "list"),
        entry("Original 1", json!("gpt-a"), "list"),
        entry("Original 2", json!("GPT_A!"), "list"),
        entry("  ", json!("gpt-b"), "list"),
        entry("Original 4", json!("gpt-c"), "hidden"),
        entry("Original 5", json!(5), "list"),
    ];
    models.extend(
        (6..207).map(|i| entry(&format!("Original {i}"), json!(format!("gpt-{i}")), "list")),
    );
    let original = json!({ "models": models });
    let (scrubbed, _) = scrub(&original).expect("within bounds");
    let want: Vec<usize> = std::iter::once(0).chain(6..205).collect();
    assert_eq!(admitted(&original, "Original ").await, want);
    assert_eq!(admitted(&scrubbed, "Model ").await, want);
}

#[test]
fn the_file_goes_only_beside_a_copied_brain_in_a_numbered_run() {
    #[cfg(not(target_os = "macos"))]
    let (exe, output) = (
        "C:/repo/target/live/1762460000/bin/kumochid.exe",
        "C:/repo/target/live/1762460000/catalog-provenance.json",
    );
    #[cfg(target_os = "macos")]
    let (exe, output) = (
        "/repo/target/live/1762460000/bin/kumochid",
        "/repo/target/live/1762460000/catalog-provenance.json",
    );
    assert_eq!(output_path(Path::new(exe)), Some(PathBuf::from(output)));
    #[cfg(not(target_os = "macos"))]
    let rejected = [
        "C:/repo/target/debug/kumochid.exe",
        "C:/repo/target/live/abc/bin/kumochid.exe",
        "C:/repo/target/live/1/kumochid.exe",
        "C:/repo/target/live/1/debug/kumochid.exe",
        "C:/repo/target/live/1/bin/kumochi.exe",
        "C:/repo/target/runs/1/bin/kumochid.exe",
        "C:/repo/target/live/1/bin/kumochid",
        "C:/repo/target/live/1/bin/kumochid.exe.old",
    ];
    #[cfg(target_os = "macos")]
    let rejected = [
        "/repo/target/debug/kumochid",
        "/repo/target/live/abc/bin/kumochid",
        "/repo/target/live/1/kumochid",
        "/repo/target/live/1/debug/kumochid",
        "/repo/target/live/1/bin/kumochi",
        "/repo/target/runs/1/bin/kumochid",
        "/repo/target/live/1/bin/kumochid.exe",
        "/repo/target/live/1/bin/kumochid.old",
    ];
    for exe in rejected {
        assert_eq!(output_path(Path::new(exe)), None, "{exe}");
    }
}

#[test]
fn the_file_is_written_new_or_not_at_all() {
    let scratch = Scratch::new("provenance");
    fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let out = scratch.dir().join("catalog-provenance.json");
    assert_eq!(write(&out, b"scrubbed"), Outcome::Written);
    assert_eq!(fs::read(&out).expect("written"), b"scrubbed");
    assert_eq!(write(&out, b"second"), Outcome::NotWritten);
    assert_eq!(fs::read(&out).expect("kept"), b"scrubbed");
}
