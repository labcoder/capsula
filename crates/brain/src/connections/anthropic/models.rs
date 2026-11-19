//! Synthetic fixtures follow the public Models API reference, checked 2026-11-05:
//! <https://platform.claude.com/docs/en/api/models/list>.
//! Known exact ids use the active-model facts checked on the same date:
//! <https://platform.claude.com/docs/en/models/overview>,
//! <https://platform.claude.com/docs/en/build-with-claude/pdf-support>,
//! <https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming>,
//! <https://platform.claude.com/docs/en/build-with-claude/effort>.
//! Unknown ids stay disabled; streaming support requires eager input streaming on the tool.

use zeroize::Zeroizing;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{KeyRejection, ModelInfo, ModelName, ModelSlug};
use serde::Deserialize;

use super::key::AnthropicKey;
use super::tls::{AnthropicConnect, HOST};
use crate::connections::http_client::MAX_BODY_BYTES;
use crate::connections::model::{
    IDLE,
    conn::{ModelConn, Upload},
    wire::Body as WireBody,
};

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

pub(super) async fn read_fenced(
    connect: &impl AnthropicConnect,
    key: &Secret<AnthropicKey>,
) -> Result<Vec<CatalogModel>, CheckError> {
    let head =
        format!("GET /v1/models?limit=65 HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer ");
    let parts: [&[u8]; 3] = [
        head.as_bytes(),
        key.expose().as_str().as_bytes(),
        b"\r\nanthropic-version: 2023-06-01\r\nAccept: application/json\r\n\
          Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
    ];
    let mut request = Zeroizing::new(Vec::with_capacity(
        parts.iter().map(|part| part.len()).sum(),
    ));
    for part in parts {
        request.extend_from_slice(part);
    }
    let mut conn = ModelConn::new(
        connect
            .connect()
            .await
            .map_err(|_| CheckError::Unconfirmed)?,
    );
    let head = match conn
        .upload(&request)
        .await
        .map_err(|_| CheckError::Unconfirmed)?
    {
        Upload::Flushed => conn.head(IDLE).await.map_err(|_| CheckError::Unconfirmed)?,
        Upload::Refused(head) => head,
    };
    drop(request);
    if !matches!(head.status, 200 | 400) {
        return catalog(head.status, &[]);
    }
    let mut wire = WireBody::new(head.framing);
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(len) = conn
        .next(&mut wire, IDLE)
        .await
        .map_err(|_| CheckError::Unconfirmed)?
    {
        if len > MAX_BODY_BYTES - bytes.len() {
            return Err(CheckError::Unconfirmed);
        }
        bytes.extend_from_slice(&conn.buf[..len]);
        conn.buf.drain(..len);
    }
    catalog(head.status, &bytes)
}

fn catalog(status: u16, bytes: &[u8]) -> Result<Vec<CatalogModel>, CheckError> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(CheckError::Unconfirmed);
    }
    match status {
        200 => {}
        401 | 403 => return Err(CheckError::Rejected(KeyRejection::Refused)),
        400 => {
            if serde_json::from_slice::<ErrorBody>(bytes).is_ok_and(|body| {
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
    let body: Body = serde_json::from_slice(bytes).map_err(|_| CheckError::Unconfirmed)?;
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
