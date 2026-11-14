//! The account's model catalog (design §6, "PR 10's catalog contract"): one bearer GET of the
//! model list, keeping the entries meant for display in the server's order. An entry that breaks
//! a rule is skipped and counted, never logged by value.

#[cfg(test)]
mod tests;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    Action, Effort, ErrorKey, MAX_MODELS, ModelChoice, ModelInfo, ModelName, ModelSlug, UserError,
};
use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

use super::error::Recovery;
use super::http_client::{Endpoint, Http, HttpError, Request};
use super::oauth::exchange::OAuthCode;

/// The default model's slug when the catalog lists it (owner, plan §9.1).
pub(crate) const DEFAULT_SLUG: &str = "gpt-6-luna";

/// The default model `models` calls for when `current` is unset or no longer listed:
/// [`DEFAULT_SLUG`] at `Low` if listed, else the first model at `Low`. `None` keeps `current`;
/// an empty catalog changes nothing.
pub(crate) fn default_model(
    current: Option<&ModelChoice>,
    models: &[ModelInfo],
) -> Option<ModelChoice> {
    if current.is_some_and(|choice| {
        choice.connection != kumochi_protocol::message::ConnectionKind::ChatGpt
            || models.iter().any(|model| model.slug == choice.model)
    }) {
        return None;
    }
    let model = models
        .iter()
        .find(|model| model.slug.as_str() == DEFAULT_SLUG)
        .or(models.first())?;
    Some(ModelChoice {
        model: model.slug.clone(),
        effort: Effort::Low,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    })
}

/// Why the catalog wasn't read; nothing the provider wrote is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// The request didn't complete.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// The provider answered with something other than 200.
    #[error("model list refused ({status}, {code:?})")]
    Refused {
        /// The provider's status.
        status: u16,
        /// Its error code, `Other` when there's none this build knows.
        code: OAuthCode,
    },
    /// A 200 whose body isn't a model list.
    #[error("invalid model list")]
    Response,
}

impl CatalogError {
    /// What the user is told.
    #[must_use]
    pub(crate) fn failure(self) -> UserError {
        let (key, retry) = match self {
            Self::Http(HttpError::TrustStoreUnavailable) => (ErrorKey::TrustStoreUnavailable, true),
            // The only unavailable plan, which arms Erase on sign-out.
            Self::Refused {
                status: 403,
                code: OAuthCode::NotEligible,
            } => (ErrorKey::PlanUnavailable, false),
            // A busy or failing server offers Retry whatever code it sent. A rejected bearer's
            // set is marked due, so Retry refreshes it.
            Self::Refused {
                status: 401 | 429 | 500..,
                ..
            }
            | Self::Http(_)
            | Self::Response => (ErrorKey::AccountOffline, true),
            Self::Refused { status, code } => match code.recovery(status) {
                Recovery::RetryLater => (ErrorKey::AccountOffline, true),
                Recovery::Unavailable | Recovery::SignInAgain | Recovery::FixRequest => {
                    (ErrorKey::AccountMisconfigured, false)
                }
            },
        };
        let actions = if retry {
            vec![Action::Retry]
        } else {
            Vec::new()
        };
        UserError::new(key, Vec::new(), actions).expect("one action is within the limit")
    }
}

#[derive(Deserialize)]
struct Body {
    models: Vec<Value>,
}

#[derive(Deserialize)]
struct Entry {
    slug: String,
    display_name: String,
    visibility: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorObject,
}

#[derive(Deserialize)]
struct ErrorObject {
    code: Option<String>,
}

/// A read catalog: the models shown, and which of them take image input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The listed models, in the server's order.
    pub models: Vec<ModelInfo>,
    /// The listed models whose entry's `input_modalities` names `"image"`. It changes nothing
    /// shown; absent or malformed means no.
    pub images: Vec<ModelSlug>,
}

/// Reads the catalog with `bearer`: the listed models, at most [`MAX_MODELS`], in the server's
/// order.
///
/// # Errors
/// [`CatalogError`] for a failed request, a refusal, or a body that isn't a model list.
pub async fn read(
    http: &impl Http,
    bearer: Secret<Zeroizing<String>>,
) -> Result<Listing, CatalogError> {
    let response = http
        .execute(Request {
            endpoint: Endpoint::Models(bearer),
            fields: Vec::new(),
        })
        .await?;
    let bytes = response.body.expose();
    if response.status != 200 {
        let code = serde_json::from_slice::<ErrorBody>(bytes)
            .ok()
            .and_then(|body| body.error.code)
            .map_or(OAuthCode::Other, |code| OAuthCode::parse(&code));
        return Err(CatalogError::Refused {
            status: response.status,
            code,
        });
    }
    let body: Body = serde_json::from_slice(bytes).map_err(|_| CatalogError::Response)?;
    let mut listing = Listing::default();
    let mut skipped = 0_usize;
    for value in body.models {
        let images = value
            .get("input_modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| modalities.iter().any(|modality| modality == "image"));
        let Ok(entry) = serde_json::from_value::<Entry>(value) else {
            skipped += 1;
            continue;
        };
        if entry.visibility != "list" {
            continue;
        }
        match (
            ModelSlug::new(&entry.slug),
            ModelName::new(&entry.display_name),
        ) {
            (Ok(slug), Ok(display_name))
                if listing.models.len() < MAX_MODELS
                    && listing.models.iter().all(|model| model.slug != slug) =>
            {
                if images {
                    listing.images.push(slug.clone());
                }
                listing.models.push(ModelInfo { slug, display_name });
            }
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "model list entries were skipped");
    }
    Ok(listing)
}
