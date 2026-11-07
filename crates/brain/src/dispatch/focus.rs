//! `focus`: moves the user's typed input to a task, or back to Kumo
//! (docs/development/tools.md).

use kumochi_protocol::message::ThreadId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LocalWrite, Prepared, Tool, ToolError, ToolOutput, parameters, parse};
use crate::engines::ToolName;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FocusArgs {
    /// The task's id, or Kumo's own.
    task: u64,
}

fn prepare(args: &[u8]) -> Result<Prepared, ToolError> {
    let (focus, hash) = parse::<FocusArgs>("focus", args)?;
    Ok(Prepared {
        hash,
        action: super::Action::Write {
            write: LocalWrite::Focus(ThreadId(focus.task)),
            output: ToolOutput::Text(ANSWER.to_owned()),
        },
    })
}

/// What the model reads once the task is confirmed live; the shell may still keep the user
/// where they chose to be.
pub(super) const ANSWER: &str =
    "The user's next message should go to that task, unless they have already moved elsewhere.";

pub(super) const FOCUS: Tool = Tool {
    name: ToolName::Focus,
    wire: "focus",
    description: "Moves the user's typed input to a task, or back to Kumo, when the user asks to \
                  go there. From a task, only Kumo.",
    kumo_only: false,
    mcp: false,
    parameters: parameters::<FocusArgs>,
    prepare,
};
