//! A request's size, counted as its JSON body would be: every string escaped, plus an allowance
//! for each item's and the body's own fields [guess]. 13a checks the serialised body again.

use super::{InputItem, ModelRequest, ToolSchema};
use crate::engines::CallId;

/// The fields around each item, and its call id at its longest.
const ITEM_OVERHEAD: usize = 64 + CallId::MAX;
/// The body's own fields: model, effort, flags and the item list's brackets.
const BODY_OVERHEAD: usize = 512;

/// `text` as a JSON string.
fn escaped(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len().saturating_mul(6), |json| json.len())
}

pub(super) fn item_bytes(item: &InputItem) -> usize {
    ITEM_OVERHEAD
        + match item {
            InputItem::Message { text, .. } => escaped(text),
            InputItem::FunctionCall {
                name, arguments, ..
            } => escaped(name) + escaped(arguments),
            InputItem::FunctionOutput { output, .. } => escaped(output),
            InputItem::Reasoning(item) => item.size(),
        }
}

fn tool_bytes(tool: &ToolSchema) -> usize {
    ITEM_OVERHEAD
        + escaped(tool.name)
        + escaped(tool.description)
        + tool.parameters.to_string().len()
}

/// The part of a request that never shrinks: its body, instructions and tools.
pub(super) fn fixed_bytes(instructions: &str, tools: &[ToolSchema]) -> usize {
    BODY_OVERHEAD + escaped(instructions) + tools.iter().map(tool_bytes).sum::<usize>()
}

/// A whole request's size.
pub(crate) fn request_bytes(request: &ModelRequest) -> usize {
    fixed_bytes(&request.instructions, &request.tools)
        + request.input.iter().map(item_bytes).sum::<usize>()
}
