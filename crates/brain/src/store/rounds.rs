//! A stored round's native items and results, each kept as one JSON object in `events.body`
//! with no searchable text. Each is checked whole when it's written and again, with its bound,
//! when it's read back, so a bad row fails the read instead of reaching a request.

use kumochi_protocol::message::{RunId, ThreadId};
use serde_json::{Value, json};

use super::{EventKind, NewEvent, Store, StoreError};
use crate::context::{Body, ConnectionTag};
use crate::dispatch::ToolOutput;
use crate::engines::{CallId, Opaque};

/// The longest stored result read back: a full tool output, plus its call id, escaped, and the
/// framing around them.
const MAX_RESULT: usize = crate::dispatch::MAX_OUTPUT + 1024;

impl Store {
    /// Appends one round to an active run in one transaction: the text streamed since the last
    /// write, the native items in the order given, then each call's answer.
    ///
    /// # Errors
    /// [`StoreError::RoundRecord`] when an item isn't a JSON object, and whatever
    /// [`Store::append`] fails with.
    pub(crate) fn append_round(
        &mut self,
        (thread, run): (ThreadId, RunId),
        connection: ConnectionTag,
        text: Option<&str>,
        items: &[Opaque],
        results: &[(CallId, ToolOutput)],
    ) -> Result<(), StoreError> {
        let natives = items
            .iter()
            .map(|item| native_body(connection, item))
            .collect::<Result<Vec<_>, _>>()?;
        let results: Vec<String> = results
            .iter()
            .map(|(call, output)| result_body(call, output))
            .collect();
        let text = text.map(|text| NewEvent {
            kind: EventKind::AssistantText,
            text: Some(text),
            body: None,
        });
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
        let events: Vec<NewEvent<'_>> = text.into_iter().chain(natives).chain(results).collect();
        self.append(thread, run, &events)
    }
}

/// A native item's body: the connection that made it, and the item as the same JSON value.
fn native_body(connection: ConnectionTag, item: &Opaque) -> Result<String, StoreError> {
    let item: Value = serde_json::from_slice(item.bytes()).map_err(|_| StoreError::RoundRecord)?;
    if !item.is_object() {
        return Err(StoreError::RoundRecord);
    }
    Ok(json!({ "connection": connection.0, "item": item }).to_string())
}

/// A result's body: the call it answers and its tagged output.
fn result_body(call: &CallId, output: &ToolOutput) -> String {
    json!({ "call": call.as_str(), "output": output.json() }).to_string()
}

pub(super) fn native(body: &str) -> Result<Body, StoreError> {
    let mut value: Value = serde_json::from_str(body).map_err(|_| StoreError::RoundRecord)?;
    let connection = value["connection"]
        .as_u64()
        .ok_or(StoreError::RoundRecord)?;
    let item = value["item"].take();
    if !item.is_object() {
        return Err(StoreError::RoundRecord);
    }
    let bytes = serde_json::to_vec(&item).map_err(|_| StoreError::RoundRecord)?;
    Ok(Body::Native {
        connection: ConnectionTag(connection),
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
