use std::sync::Mutex;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    Action, Effort, EffortSet, ErrorKey, MAX_MODEL_NAME_CHARS, MAX_MODELS, MAX_SLUG_BYTES,
    ModelChoice, ModelInfo, ModelName, ModelSlug, UserError,
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{CatalogError, Listing, default_model, read};
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response};
use crate::connections::oauth::exchange::OAuthCode;
use crate::test_support::Captured;

/// Answers one request with `reply`, and keeps where it went and the bearer it carried.
struct Once {
    reply: Mutex<Option<Result<Response, HttpError>>>,
    sent: Mutex<Vec<(String, Option<String>, usize)>>,
}

impl Http for Once {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let bearer = match &request.endpoint {
            Endpoint::Models(bearer) => Some(bearer.expose().to_string()),
            _ => None,
        };
        self.sent.lock().expect("log").push((
            request
                .endpoint
                .url()
                .expect("checked endpoint")
                .into_owned(),
            bearer,
            request.fields.len(),
        ));
        std::future::ready(
            self.reply
                .lock()
                .expect("reply")
                .take()
                .expect("one request"),
        )
    }
}

fn answering(status: u16, body: &str) -> Once {
    Once {
        reply: Mutex::new(Some(Ok(Response {
            status,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(body.as_bytes().to_vec())),
        }))),
        sent: Mutex::new(Vec::new()),
    }
}

#[test]
fn route_choice_chatgpt_refresh_keeps_another_connections_default() {
    use kumochi_protocol::message::ConnectionKind;

    let listing = Listing {
        models: vec![info("gpt-6-luna")],
        ..Listing::default()
    };
    for connection in [ConnectionKind::ClaudeCode, ConnectionKind::AnthropicKey] {
        let current = ModelChoice {
            model: ModelSlug::new("same-slug").expect("slug"),
            effort: Effort::High,
            connection,
        };
        assert_eq!(default_model(Some(&current), &listing), None);
    }
}

fn model(slug: &str, name: &str) -> Value {
    json!({
        "slug": slug, "display_name": name, "visibility": "list",
        "supported_reasoning_levels": [{ "effort": "low" }]
    })
}

/// The slugs and names `read` kept from `models`.
async fn kept(models: Vec<Value>) -> Result<Vec<(String, String)>, CatalogError> {
    let http = answering(200, &json!({ "models": models }).to_string());
    let catalog = read(&http, Secret::new(Zeroizing::new("bearer".to_owned()))).await?;
    Ok(catalog
        .models
        .into_iter()
        .map(|model| {
            (
                model.slug.as_str().to_owned(),
                model.display_name.as_str().to_owned(),
            )
        })
        .collect())
}

fn pair(slug: &str, name: &str) -> (String, String) {
    (slug.to_owned(), name.to_owned())
}

#[tokio::test]
async fn the_listed_models_come_in_the_servers_order_with_the_bearer_sent_only_there() {
    let http = answering(
        200,
        &json!({ "object": "list", "models": [
            model("gpt-6.1-sol", "GPT-6.1 Sol"),
            { "slug": "hidden-model", "display_name": "Hidden", "visibility": "hide", "extra": 1 },
            model("gpt-6-luna", "GPT-6 Luna"),
        ]})
        .to_string(),
    );
    let catalog = read(
        &http,
        Secret::new(Zeroizing::new("bearer-token".to_owned())),
    )
    .await
    .expect("read");
    let slugs: Vec<_> = catalog
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .collect();
    assert_eq!(slugs, ["gpt-6.1-sol", "gpt-6-luna"]);
    assert_eq!(catalog.images, []);
    assert_eq!(
        *http.sent.lock().expect("log"),
        [(
            "https://api.openai.com/v1/models".to_owned(),
            Some("bearer-token".to_owned()),
            0
        )]
    );
}

/// A real list: the catalog-provenance live check's capture of this account's models on
/// 2026-11-13, trimmed to the six keys that capture keeps, with its scrubbed slugs, names and
/// strings as it wrote them. Two entries aren't listed.
#[tokio::test]
async fn a_captured_list_gives_its_listed_models_and_their_image_input() {
    let http = answering(200, include_str!("captured_models.json"));
    let catalog = read(&http, Secret::new(Zeroizing::new("bearer".to_owned())))
        .await
        .expect("the captured list reads");
    let models: Vec<_> = catalog
        .models
        .iter()
        .map(|model| pair(model.slug.as_str(), model.display_name.as_str()))
        .collect();
    assert_eq!(
        models,
        [
            pair("m-0", "Model 0"),
            pair("m-1", "Model 1"),
            pair("m-2", "Model 2"),
            pair("m-3", "Model 3"),
            pair("m-5", "Model 5"),
            pair("m-6", "Model 6"),
            pair("m-7", "Model 7"),
            pair("m-8", "Model 8"),
        ]
    );
    let images: Vec<_> = catalog.images.iter().map(ModelSlug::as_str).collect();
    assert_eq!(
        images,
        ["m-0", "m-1", "m-2", "m-3", "m-5", "m-6", "m-7", "m-8"]
    );
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| model.supported_efforts)
            .collect::<Vec<_>>(),
        vec![
            EffortSet::new([
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::XHigh,
                Effort::Max
            ]);
            8
        ]
    );
}

#[test]
fn the_captured_effort_fields_match_the_literal_scrubbed_shape() {
    let root: Value =
        serde_json::from_str(include_str!("captured_models.json")).expect("captured JSON");
    let rows = root["models"].as_array().expect("model array");
    assert_eq!(root.as_object().expect("root object").len(), 1);
    assert_eq!(rows.len(), 10);
    let defaults = [
        "low", "medium", "low", "medium", "medium", "medium", "low", "medium", "medium", "medium",
    ];
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.as_object().expect("model object").len(), 6);
        assert_eq!(row["slug"], format!("m-{index}"));
        assert_eq!(row["display_name"], format!("Model {index}"));
        assert_eq!(
            row["visibility"],
            if [4, 9].contains(&index) {
                "<string:4>"
            } else {
                "list"
            }
        );
        assert_eq!(row["input_modalities"], json!(["text", "image"]));
        assert_eq!(row["default_reasoning_level"], defaults[index]);
        let levels = row["supported_reasoning_levels"]
            .as_array()
            .expect("effort rows");
        let mut expected = vec!["low", "medium", "high", "xhigh", "max"];
        if [0, 1, 2, 3, 6, 7].contains(&index) {
            expected.push("<string:5>");
        }
        assert_eq!(levels.len(), expected.len());
        for (level, effort) in levels.iter().zip(expected) {
            assert_eq!(level.as_object().expect("effort object").len(), 2);
            assert_eq!(level["effort"], effort);
            let redacted = level["<key-0>"].as_str().expect("scrubbed unknown string");
            let length = redacted
                .strip_prefix("<string:")
                .and_then(|s| s.strip_suffix('>'));
            assert!(length.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())));
        }
    }
}

/// Image input comes only from an entry's own `input_modalities` naming `"image"`, and whatever
/// that field holds, the shown list is the same.
#[tokio::test]
async fn image_input_is_read_from_input_modalities_without_changing_the_list() {
    let with = |slug: &str, modalities: Value| {
        let mut entry = model(slug, "Model");
        entry["input_modalities"] = modalities;
        entry
    };
    let http = answering(
        200,
        &json!({ "models": [
            with("m-text-image", json!(["text", "image"])),
            with("m-text", json!(["text"])),
            model("m-absent", "Model"),
            with("m-string", json!("image")),
            with("m-number", json!(7)),
            with("m-upper", json!(["IMAGE"])),
            with("m-nested", json!([["image"]])),
            with("m-text-image", json!(["image"])),
            { "slug": "m-hidden", "display_name": "Hidden", "visibility": "hide",
              "input_modalities": ["image"] },
        ]})
        .to_string(),
    );
    let listing = read(&http, Secret::new(Zeroizing::new("bearer".to_owned())))
        .await
        .expect("read");
    let slugs: Vec<_> = listing
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .collect();
    assert_eq!(
        slugs,
        [
            "m-text-image",
            "m-text",
            "m-absent",
            "m-string",
            "m-number",
            "m-upper",
            "m-nested"
        ]
    );
    assert_eq!(
        listing.images,
        [ModelSlug::new("m-text-image").expect("slug")]
    );
}

#[tokio::test]
async fn at_most_200_models_are_kept() {
    let models = |count: usize| {
        (0..count)
            .map(|n| model(&format!("m-{n}"), &format!("Model {n}")))
            .collect::<Vec<_>>()
    };
    let exact = kept(models(MAX_MODELS)).await.expect("read");
    assert_eq!(exact.len(), MAX_MODELS);
    let mut over = models(MAX_MODELS + 1);
    over.insert(
        0,
        json!({"slug": "unknown", "display_name": "Unknown", "visibility": "list"}),
    );
    let over = kept(over).await.expect("read");
    assert_eq!(over.len(), MAX_MODELS);
    assert_eq!(over.last(), Some(&pair("m-199", "Model 199")));
}

#[tokio::test]
async fn an_entry_that_breaks_a_rule_is_skipped_and_counted_but_never_logged() {
    let captured = Captured::default();
    let _logging = captured.install();
    let long_slug = "a".repeat(MAX_SLUG_BYTES);
    let long_name = "界".repeat(MAX_MODEL_NAME_CHARS);
    let kept = kept(vec![
        model(&long_slug, &long_name),
        model(&"a".repeat(MAX_SLUG_BYTES + 1), "Too long a slug"),
        model("zqx-long-name", &"n".repeat(MAX_MODEL_NAME_CHARS + 1)),
        model("Zqx-Upper", "Bad slug"),
        model("zqx-empty-name", ""),
        model("zqx-two-lines", "Two\nlines"),
        json!({ "slug": 7, "display_name": "Mistyped", "visibility": "list" }),
        json!({ "slug": "zqx-no-visibility", "display_name": "Missing" }),
        json!("zqx-not-an-object"),
        model("gpt-6-luna", "GPT-6 Luna"),
        model("gpt-6-luna", "Zqx duplicate"),
    ])
    .await
    .expect("read");
    assert_eq!(
        kept,
        [
            pair(&long_slug, &long_name),
            pair("gpt-6-luna", "GPT-6 Luna")
        ]
    );
    let logs = captured.text();
    assert!(logs.contains("skipped=9"), "{logs}");
    assert!(!logs.to_lowercase().contains("zqx"), "{logs}");
}

#[tokio::test]
async fn an_empty_list_is_read_and_a_body_that_isnt_one_is_refused() {
    assert_eq!(kept(Vec::new()).await, Ok(Vec::new()));
    assert_eq!(
        kept(vec![json!({
            "slug": "hidden", "display_name": "Hidden", "visibility": "hide",
            "supported_reasoning_levels": [],
        })])
        .await,
        Ok(Vec::new())
    );
    for body in [
        "",
        "{}",
        "{\"models\":[",
        "{\"models\":{}}",
        "{\"models\":null}",
        "<html>",
    ] {
        let http = answering(200, body);
        let result = read(&http, Secret::new(Zeroizing::new("bearer".to_owned()))).await;
        assert_eq!(result, Err(CatalogError::Response), "{body:?}");
    }
}

#[test]
fn an_absent_or_mistyped_effort_array_confers_no_support() {
    for entry in [
        json!({"default_reasoning_level": "low"}),
        json!({"supported_reasoning_levels": []}),
        json!({"supported_reasoning_levels": "low"}),
        json!({"supported_reasoning_levels": {"effort": "low"}}),
        json!({"supported_reasoning_levels": [null, {}, {"effort": ["low"]}]}),
    ] {
        assert_eq!(supported(&entry), None);
    }
}

fn refusal(status: u16, code: Option<&str>) -> String {
    match code {
        Some(code) => json!({ "error": { "code": code, "message": "zqx" } }).to_string(),
        None => format!("<html>{status}</html>"),
    }
}

#[tokio::test]
async fn refusals_keep_only_status_and_code_and_say_what_to_do() {
    let error = |key, actions: &[Action]| {
        UserError::new(key, Vec::new(), actions.to_vec()).expect("within limits")
    };
    let retry = [Action::Retry];
    let cases = [
        (
            403,
            Some("subscription_sharing_user_not_eligible"),
            OAuthCode::NotEligible,
            error(ErrorKey::PlanUnavailable, &[]),
        ),
        (
            401,
            Some("subscription_sharing_invalid_user"),
            OAuthCode::Other,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            401,
            None,
            OAuthCode::Other,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            503,
            Some("subscription_sharing_usage_unavailable"),
            OAuthCode::PlanBusy,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            403,
            Some("chatpass_v2_scope_not_authorized"),
            OAuthCode::GrantRefused,
            error(ErrorKey::AccountMisconfigured, &[]),
        ),
        (
            403,
            Some("subscription_sharing_route_not_supported"),
            OAuthCode::Other,
            error(ErrorKey::AccountMisconfigured, &[]),
        ),
        (
            500,
            None,
            OAuthCode::Other,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            429,
            None,
            OAuthCode::Other,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            503,
            Some("subscription_sharing_user_not_eligible"),
            OAuthCode::NotEligible,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            503,
            Some("invalid_client"),
            OAuthCode::InvalidClient,
            error(ErrorKey::AccountOffline, &retry),
        ),
        (
            400,
            Some("subscription_sharing_user_not_eligible"),
            OAuthCode::NotEligible,
            error(ErrorKey::AccountMisconfigured, &[]),
        ),
    ];
    for (status, code, parsed, told) in cases {
        let http = answering(status, &refusal(status, code));
        let failed = read(&http, Secret::new(Zeroizing::new("bearer".to_owned())))
            .await
            .expect_err("refused");
        assert_eq!(
            failed,
            CatalogError::Refused {
                status,
                code: parsed
            },
            "{status} {code:?}"
        );
        assert_eq!(failed.failure(), told, "{status} {code:?}");
    }
    let http = answering(200, "{}");
    *http.reply.lock().expect("reply") = Some(Err(HttpError::Transport));
    let failed = read(&http, Secret::new(Zeroizing::new("bearer".to_owned())))
        .await
        .expect_err("failed");
    assert_eq!(failed, CatalogError::Http(HttpError::Transport));
    assert_eq!(failed.failure(), error(ErrorKey::AccountOffline, &retry));
    assert_eq!(
        CatalogError::Http(HttpError::TrustStoreUnavailable).failure(),
        error(ErrorKey::TrustStoreUnavailable, &retry)
    );
}

fn info(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).expect("valid"),
        display_name: ModelName::new("A model").expect("valid"),
        supported_efforts: EffortSet::new([Effort::Low]),
    }
}

fn at(slug: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(slug).expect("valid"),
        effort,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

#[test]
fn the_default_model_is_set_only_when_unset_or_no_longer_listed() {
    let with_luna = [info("gpt-6-sol"), info("gpt-6-luna")];
    let without = [info("gpt-6-sol"), info("gpt-6-terra")];
    let listed = at("gpt-6-terra", Effort::High);
    let gone = at("gpt-5-old", Effort::High);
    let cases: [(Option<&ModelChoice>, &[ModelInfo], Option<ModelChoice>); 6] = [
        (None, &with_luna, Some(at("gpt-6-luna", Effort::Low))),
        (None, &without, Some(at("gpt-6-sol", Effort::Low))),
        (None, &[], None),
        (Some(&listed), &without, None),
        (Some(&gone), &with_luna, Some(at("gpt-6-luna", Effort::Low))),
        (Some(&gone), &[], None),
    ];
    for (case, (current, models, expected)) in cases.into_iter().enumerate() {
        let listing = Listing {
            models: models.to_vec(),
            ..Listing::default()
        };
        assert_eq!(default_model(current, &listing), expected, "case {case}");
    }
}

#[tokio::test]
async fn sparse_supported_models_do_not_replace_a_listed_unsendable_default() {
    let listing = json!({ "models": [
        { "slug": "saved", "display_name": "Saved", "visibility": "list", "input_modalities": ["image"],
          "default_reasoning_level": "low", "supported_efforts": ["low"],
          "supported_reasoning_levels": ["low", {"effort": "LOW"}, {"effort": 1},
                                          {"effort": "x-high"}, {"effort": "<string:5>"}] },
        { "slug": "first", "display_name": "First", "visibility": "list",
          "supported_reasoning_levels": [{"effort": "medium"}, {"effort": "max"}, {"effort": "medium"}] },
        { "slug": "gpt-6-luna", "display_name": "Luna", "visibility": "list",
          "supported_reasoning_levels": [{"effort": "high"}] },
        { "slug": "first", "display_name": "Duplicate", "visibility": "list", "input_modalities": ["image"],
          "supported_reasoning_levels": [{"effort": "low"}] },
    ]});
    let http = answering(200, &listing.to_string());
    let catalog = read(&http, Secret::new(Zeroizing::new("bearer".to_owned())))
        .await
        .expect("mixed catalog");
    assert_eq!(catalog.unknown_effort, [at("saved", Effort::Low).model]);
    assert_eq!(catalog.images, []);
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| (model.slug.as_str(), model.supported_efforts))
            .collect::<Vec<_>>(),
        [
            ("first", EffortSet::new([Effort::Medium, Effort::Max])),
            ("gpt-6-luna", EffortSet::new([Effort::High]))
        ]
    );
    assert_eq!(
        default_model(Some(&at("saved", Effort::High)), &catalog),
        None
    );
    assert_eq!(
        default_model(None, &catalog),
        Some(at("first", Effort::Medium))
    );
    assert_eq!(
        default_model(Some(&at("gone", Effort::Low)), &catalog),
        Some(at("first", Effort::Medium))
    );
}
