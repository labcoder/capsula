//! The account's model catalog (design §6, "PR 10's catalog contract"): one bearer GET of the
//! model list, keeping the entries meant for display in the server's order. An entry that breaks
//! a rule is skipped and counted, never logged by value.

#[cfg(test)]
mod tests;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    Action, ConnectionKind, Effort, EffortSet, ErrorKey, MAX_MODELS, ModelChoice, ModelInfo,
    ModelName, ModelSlug, UserError,
};
use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

use super::error::Recovery;
use super::http_client::{Endpoint, Http, HttpError, Request};
use super::oauth::exchange::OAuthCode;

/// The default model's slug when the catalog lists it (owner, plan §9.1).
pub(crate) const DEFAULT_SLUG: &str = "gpt-6-luna";

/// Repairs an unset or missing default, preferring Luna at Low if offered, else the first
/// sendable model at its lowest effort. A still-listed model keeps its choice even if it can't send.
pub(crate) fn default_model(
    current: Option<&ModelChoice>,
    listing: &Listing,
) -> Option<ModelChoice> {
    if current.is_some_and(|choice| {
        choice.connection != ConnectionKind::ChatGpt
            || listing.withheld_incomplete
            || listing.withheld.contains(&choice.model)
            || listing
                .models
                .iter()
                .any(|model| model.slug == choice.model)
    }) {
        return None;
    }
    let eligible = || {
        listing
            .models
            .iter()
            .filter_map(|model| Some((model, model.supported_efforts?)))
    };
    let (model, supported) = eligible()
        .find(|(model, efforts)| {
            model.slug.as_str() == DEFAULT_SLUG && efforts.contains(Effort::Low)
        })
        .or_else(|| eligible().next())?;
    Some(ModelChoice {
        model: model.slug.clone(),
        effort: supported.lowest(),
        connection: ConnectionKind::ChatGpt,
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
    /// A 200 with a malformed list or no known effort support on any listable row.
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
    /// The sendable models, in the server's order.
    pub models: Vec<ModelInfo>,
    /// The listed models whose entry's `input_modalities` names `"image"`. It changes nothing
    /// shown; absent or malformed means no.
    pub images: Vec<ModelSlug>,
    /// Bounded membership of rows withheld from the offer, for saved-default repair only.
    withheld: Vec<ModelSlug>,
    /// A further distinct withheld slug didn't fit, so absence can't authorize a repair.
    withheld_incomplete: bool,
}

impl Listing {
    #[cfg(test)]
    pub(crate) fn fixture(models: Vec<ModelInfo>, images: Vec<ModelSlug>) -> Self {
        Self {
            models,
            images,
            ..Self::default()
        }
    }

    fn withhold(&mut self, slug: ModelSlug) {
        if self.withheld.contains(&slug) {
            return;
        }
        if self.withheld.len() == MAX_MODELS {
            self.withheld_incomplete = true;
        } else {
            self.withheld.push(slug);
        }
    }
}

fn supported(value: &Value) -> Option<EffortSet> {
    let levels = value.get("supported_reasoning_levels")?.as_array()?;
    EffortSet::new(
        levels
            .iter()
            .filter_map(|level| match level.get("effort")?.as_str()? {
                "low" => Some(Effort::Low),
                "medium" => Some(Effort::Medium),
                "high" => Some(Effort::High),
                "xhigh" => Some(Effort::XHigh),
                "max" => Some(Effort::Max),
                _ => None,
            }),
    )
}

/// Reads the catalog with `bearer`: the listed models, at most [`MAX_MODELS`], in the server's
/// order.
///
/// # Errors
/// [`CatalogError`] for a failed request, a refusal, an invalid catalog envelope or row shape,
/// or a listable catalog without any known effort support.
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
    if bytes.trim_ascii_start().first() != Some(&b'{') {
        return Err(CatalogError::Response);
    }
    let body: Body = serde_json::from_slice(bytes).map_err(|_| CatalogError::Response)?;
    let mut listing = Listing::default();
    let mut skipped = 0_usize;
    let mut unknown = 0_usize;
    for value in body.models {
        if !value.is_object() {
            return Err(CatalogError::Response);
        }
        let efforts = supported(&value);
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
            (Ok(slug), Ok(display_name)) => {
                let Some(efforts) = efforts else {
                    unknown += 1;
                    listing.withhold(slug);
                    continue;
                };
                if listing.models.iter().any(|model| model.slug == slug) {
                    skipped += 1;
                    continue;
                }
                if listing.models.len() == MAX_MODELS {
                    skipped += 1;
                    listing.withhold(slug);
                    continue;
                }
                if images {
                    listing.images.push(slug.clone());
                }
                listing.models.push(ModelInfo {
                    slug,
                    display_name,
                    supported_efforts: Some(efforts),
                });
            }
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "model list entries were skipped");
    }
    if unknown != 0 {
        tracing::warn!(skipped = unknown, "model list entries have no known effort");
        if listing.models.is_empty() {
            return Err(CatalogError::Response);
        }
    }
    Ok(listing)
}
