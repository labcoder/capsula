//! Synthetic fixtures follow the public Models API reference, checked 2026-11-05:
//! <https://platform.claude.com/docs/en/api/models/list>.
//! Known exact ids use the active-model facts checked on the same date:
//! <https://platform.claude.com/docs/en/models/overview>,
//! <https://platform.claude.com/docs/en/build-with-claude/pdf-support>,
//! <https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming>,
//! <https://platform.claude.com/docs/en/build-with-claude/effort>.
//! Unknown ids stay disabled; streaming support requires eager input streaming on the tool.

use std::time::Duration;
use zeroize::Zeroizing;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{KeyRejection, ModelInfo, ModelName, ModelSlug};
use serde::Deserialize;

use super::key::AnthropicKey;
use crate::connections::http_client::{Endpoint, Http, MAX_BODY_BYTES, Request};

/// Brain-local support flags, false until support has been established.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent provider capability flags"
)]
pub struct Capabilities {
    /// Image input is supported.
    pub images: bool,
    /// PDF input is supported.
    pub pdfs: bool,
    /// Tool arguments can arrive incrementally.
    pub tool_argument_streaming: bool,
    /// The effort parameter is supported.
    pub effort: bool,
}

/// A bounded display model and the brain's conservative capability facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// The existing wire representation, whose Debug redacts names and ids.
    pub info: ModelInfo,
    /// Support known to the brain, never inferred from the id.
    pub capabilities: Capabilities,
}

/// A closed check outcome; no provider error text is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    /// Authentication or authorization was refused.
    #[error("Anthropic key refused")]
    Rejected(KeyRejection),
    /// Keep the key and offer a later check.
    #[error("Anthropic key unconfirmed")]
    Unconfirmed,
}

#[derive(Deserialize)]
struct Body {
    data: Vec<Entry>,
    has_more: bool,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    display_name: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ProviderError,
}

#[derive(Deserialize)]
struct ProviderError {
    #[serde(rename = "type")]
    kind: String,
    message: Secret<Zeroizing<String>>,
}

/// Read one complete catalog, with a 15-second whole-request deadline and a 64-model cap.
///
/// # Errors
/// 401/403 reject the key; a narrowly recognized 400 requires an unsupported workspace header.
/// All other failures leave it unconfirmed.
pub async fn read(
    http: &impl Http,
    key: Secret<AnthropicKey>,
) -> Result<Vec<CatalogModel>, CheckError> {
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        http.execute(Request {
            endpoint: Endpoint::AnthropicModels(key),
            fields: Vec::new(),
        }),
    )
    .await
    .map_err(|_| CheckError::Unconfirmed)?
    .map_err(|_| CheckError::Unconfirmed)?;
    if response.body.expose().len() > MAX_BODY_BYTES {
        return Err(CheckError::Unconfirmed);
    }
    match response.status {
        200 => {}
        401 | 403 => return Err(CheckError::Rejected(KeyRejection::Refused)),
        400 => {
            if serde_json::from_slice::<ErrorBody>(response.body.expose()).is_ok_and(|body| {
                body.error.kind == "invalid_request_error"
                    && body
                        .error
                        .message
                        .expose()
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                        .any(|word| word == "anthropic-workspace-id")
            }) {
                return Err(CheckError::Rejected(KeyRejection::WorkspaceRequired));
            }
            return Err(CheckError::Unconfirmed);
        }
        _ => return Err(CheckError::Unconfirmed),
    }
    let body: Body =
        serde_json::from_slice(response.body.expose()).map_err(|_| CheckError::Unconfirmed)?;
    if body.has_more || body.data.len() > 64 {
        return Err(CheckError::Unconfirmed);
    }
    let mut models: Vec<CatalogModel> = Vec::with_capacity(body.data.len());
    for entry in body.data {
        let slug = ModelSlug::new(&entry.id).map_err(|_| CheckError::Unconfirmed)?;
        let display_name =
            ModelName::new(&entry.display_name).map_err(|_| CheckError::Unconfirmed)?;
        if entry.kind != "model" || models.iter().any(|model| model.info.slug == slug) {
            return Err(CheckError::Unconfirmed);
        }
        let known = matches!(
            entry.id.as_str(),
            "claude-fable-5-1" | "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-haiku-5-5"
        );
        models.push(CatalogModel {
            info: ModelInfo {
                slug,
                display_name,
                supported_efforts: None,
            },
            capabilities: Capabilities {
                images: known,
                pdfs: known,
                tool_argument_streaming: known,
                effort: known,
            },
        });
    }
    Ok(models)
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
