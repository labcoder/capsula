use std::sync::Mutex;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    Action, Effort, ErrorKey, MAX_MODEL_NAME_CHARS, MAX_MODELS, MAX_SLUG_BYTES, ModelChoice,
    ModelInfo, ModelName, ModelSlug, UserError,
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{CatalogError, default_model, read};
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
            request.endpoint.url().to_owned(),
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
            body: Secret::new(Zeroizing::new(body.as_bytes().to_vec())),
        }))),
        sent: Mutex::new(Vec::new()),
    }
}

fn model(slug: &str, name: &str) -> Value {
    json!({ "slug": slug, "display_name": name, "visibility": "list" })
}

/// The slugs and names `read` kept from `models`.
async fn kept(models: Vec<Value>) -> Result<Vec<(String, String)>, CatalogError> {
    let http = answering(200, &json!({ "models": models }).to_string());
    let catalog = read(&http, Secret::new(Zeroizing::new("bearer".to_owned()))).await?;
    Ok(catalog
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
        &json!({ "models": [
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
    let slugs: Vec<_> = catalog.iter().map(|model| model.slug.as_str()).collect();
    assert_eq!(slugs, ["gpt-6.1-sol", "gpt-6-luna"]);
    assert_eq!(
        *http.sent.lock().expect("log"),
        [(
            "https://api.openai.com/v1/models".to_owned(),
            Some("bearer-token".to_owned()),
            0
        )]
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
    let over = kept(models(MAX_MODELS + 1)).await.expect("read");
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
    }
}

fn at(slug: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(slug).expect("valid"),
        effort,
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
        assert_eq!(default_model(current, models), expected, "case {case}");
    }
}
