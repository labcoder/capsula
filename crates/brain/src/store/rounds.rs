//! A stored round's native items and results, each kept as one JSON object in `events.body`
//! with no searchable text. An item or result is written only if the reader's own check takes it,
//! so no row can be stored that fails every later build of its thread; a bad row from an older
//! build still fails the read instead of reaching a request.

use std::collections::BTreeSet;

use kumochi_protocol::message::{ConnectionKind, ModelSlug, RunId, ThreadId};
use serde_json::{Value, json};

use super::{EventKind, NewEvent, Store, StoreError};
use crate::connections::anthropic::request::{literal, members};
use crate::connections::facts;
use crate::context::{Body, ConnectionTag, Route};
use crate::dispatch::ToolOutput;
use crate::engines::anthropic::encode::{KeyHistory, ToolUse};
use crate::engines::{CallId, Opaque};

/// The longest stored result read back: a full tool output, plus its call id, escaped, and the
/// framing around them.
const MAX_RESULT: usize = crate::dispatch::MAX_OUTPUT + 1024;

impl Store {
    /// Appends one round to an active run in one transaction: the text streamed since the last
    /// write, the native items in the order given, each with the route that made it, then each
    /// call's answer. The Anthropic key's rounds go through [`Store::append_key_round`] only.
    ///
    /// # Errors
    /// [`StoreError::RoundRecord`] when an item or a result is one the reader would refuse, or
    /// the route is the key's, writing nothing; and whatever [`Store::append`] fails with.
    #[cfg(test)]
    pub(crate) fn append_round(
        &mut self,
        ids: (ThreadId, RunId),
        connection: Route,
        text: Option<&str>,
        items: &[Opaque],
        results: &[(CallId, ToolOutput)],
    ) -> Result<(), StoreError> {
        self.append_round_events(ids, connection, plain_text(text).as_slice(), items, results)
    }

    pub(crate) fn append_round_events(
        &mut self,
        ids: (ThreadId, RunId),
        connection: Route,
        text: &[NewEvent<'_>],
        items: &[Opaque],
        results: &[(CallId, ToolOutput)],
    ) -> Result<(), StoreError> {
        if connection.kind == ConnectionKind::AnthropicKey {
            return Err(StoreError::RoundRecord);
        }
        let natives = items
            .iter()
            .map(|item| {
                let body = native_body(connection, item)?;
                native(&body)?;
                Ok(body)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        self.write_round(ids, text, &natives, results)
    }

    /// Appends one of the Anthropic key's rounds as [`Store::append_round`] does, its items only
    /// its calls, each read back as the replay will read it before anything is written.
    ///
    /// # Errors
    /// As [`Store::append_round`], or [`StoreError::RoundRecord`] unless the calls and results
    /// match one-to-one by id.
    #[cfg(test)]
    pub(crate) fn append_key_round(
        &mut self,
        ids: (ThreadId, RunId),
        tag: ConnectionTag,
        text: Option<&str>,
        calls: &[ToolUse],
        results: &[(CallId, ToolOutput)],
    ) -> Result<(), StoreError> {
        self.append_key_round_events(ids, tag, plain_text(text).as_slice(), calls, results, None)
    }

    pub(crate) fn append_key_round_events(
        &mut self,
        ids: (ThreadId, RunId),
        tag: ConnectionTag,
        text: &[NewEvent<'_>],
        calls: &[ToolUse],
        results: &[(CallId, ToolOutput)],
        screen_model: Option<&ModelSlug>,
    ) -> Result<(), StoreError> {
        let mut pending: BTreeSet<_> = calls.iter().map(ToolUse::id).collect();
        if pending.len() != calls.len()
            || results
                .iter()
                .any(|(call, _)| !pending.remove(call.as_str()))
            || !pending.is_empty()
        {
            return Err(StoreError::RoundRecord);
        }
        let kind = facts::of(ConnectionKind::AnthropicKey).stored;
        let natives = calls
            .iter()
            .map(|call| {
                let body = if let Some(model) = screen_model {
                    format!(
                        r#"{{"connection":{},"kind":"{kind}","screen_model":{},"item":{}}}"#,
                        tag.0,
                        json!(model),
                        call.clone().scrubbed(KeyHistory::Scrubbed).json(),
                    )
                } else {
                    format!(
                        r#"{{"connection":{},"kind":"{kind}","item":{}}}"#,
                        tag.0,
                        call.json(),
                    )
                };
                match native(&body)? {
                    Body::Native {
                        kind: Some(ConnectionKind::AnthropicKey),
                        ..
                    } => Ok(body),
                    _ => Err(StoreError::RoundRecord),
                }
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        self.write_round(ids, text, &natives, results)
    }

    fn write_round(
        &mut self,
        (thread, run): (ThreadId, RunId),
        text: &[NewEvent<'_>],
        natives: &[String],
        results: &[(CallId, ToolOutput)],
    ) -> Result<(), StoreError> {
        let results = results
            .iter()
            .map(|(call, output)| {
                let body = result_body(call, output);
                result(&body)?;
                Ok(body)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let natives = natives.iter().map(|body| NewEvent {
            kind: EventKind::OutputItem,
            text: None,
            body: Some(body),
        });
        let results = results.iter().map(|body| NewEvent {
            kind: EventKind::ToolResult,
            text: None,
            body: Some(body),
        });
        let events: Vec<NewEvent<'_>> =
            text.iter().copied().chain(natives).chain(results).collect();
        self.append(thread, run, &events)
    }
}

#[cfg(test)]
fn plain_text(text: Option<&str>) -> Option<NewEvent<'_>> {
    text.map(|text| NewEvent {
        kind: EventKind::AssistantText,
        text: Some(text),
        body: None,
    })
}

/// A native item's body: the route that made it, its tag and kind, and the item as the same
/// JSON value.
fn native_body(route: Route, item: &Opaque) -> Result<String, StoreError> {
    let item: Value = serde_json::from_slice(item.bytes()).map_err(|_| StoreError::RoundRecord)?;
    if !item.is_object() {
        return Err(StoreError::RoundRecord);
    }
    let kind = facts::of(route.kind).stored;
    Ok(json!({ "connection": route.tag.0, "kind": kind, "item": item }).to_string())
}

/// A result's body: the call it answers and its tagged output.
fn result_body(call: &CallId, output: &ToolOutput) -> String {
    json!({ "call": call.as_str(), "output": output.json() }).to_string()
}

/// A stored native row, read with no member repeated. A key row's item is kept byte for byte, and
/// one that isn't a call the encoder takes back reads as no route, so its run replays as text.
pub(super) fn native(body: &str) -> Result<Body, StoreError> {
    let mut value = literal(body.as_bytes()).map_err(|_| StoreError::RoundRecord)?;
    let connection = value["connection"]
        .as_u64()
        .ok_or(StoreError::RoundRecord)?;
    // A row written before kinds were kept is `ChatGPT`'s, the only route that stored rounds; any
    // other kind, or a kind that isn't one of these, never replays.
    let mut kind = match value.get("kind") {
        None => Some(ConnectionKind::ChatGpt),
        Some(kind) => kind.as_str().and_then(facts::stored),
    };
    let item = value["item"].take();
    if !item.is_object() {
        return Err(StoreError::RoundRecord);
    }
    let screen_model = value.get("screen_model").and_then(|model| {
        let model = model.as_str().and_then(|model| ModelSlug::new(model).ok());
        if model.is_none() || kind != Some(ConnectionKind::AnthropicKey) {
            kind = None;
            return None;
        }
        model
    });
    let bytes = if kind == Some(ConnectionKind::AnthropicKey) {
        let written = members(body)
            .and_then(|members| members.into_iter().find(|(name, _)| name == "item"))
            .map(|(_, item)| item.as_bytes().to_vec())
            .ok_or(StoreError::RoundRecord)?;
        if ToolUse::stored(&written).is_none() {
            kind = None;
        }
        written
    } else {
        serde_json::to_vec(&item).map_err(|_| StoreError::RoundRecord)?
    };
    Ok(Body::Native {
        kind,
        connection: ConnectionTag(connection),
        screen_model,
        item: Opaque::new(bytes).map_err(|_| StoreError::RoundRecord)?,
    })
}

pub(super) fn result(body: &str) -> Result<Body, StoreError> {
    if body.len() > MAX_RESULT {
        return Err(StoreError::RoundRecord);
    }
    let value: Value = serde_json::from_str(body).map_err(|_| StoreError::RoundRecord)?;
    let call = value["call"].as_str().ok_or(StoreError::RoundRecord)?;
    let call = CallId::new(call).map_err(|_| StoreError::RoundRecord)?;
    let output = match &value["output"] {
        // A result written before outputs were tagged (PR 13b-2) is the model's text already.
        Value::String(text) => text.clone(),
        tagged => match ToolOutput::from_stored(&tagged.to_string()) {
            Some(ToolOutput::Text(text)) => text,
            Some(ToolOutput::Recall(_)) => {
                let selection =
                    crate::recall::Selection::decode_round(body).ok_or(StoreError::RoundRecord)?;
                return Ok(Body::Recall { call, selection });
            }
            // shortcut: history replays as its references' JSON, not the re-read ranges, until
            // the first history tool (PR 15) builds them into native rounds.
            Some(other) => other.stored(),
            None => return Err(StoreError::RoundRecord),
        },
    };
    Ok(Body::Result { call, output })
}
