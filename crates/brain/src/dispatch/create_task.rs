//! `create_task`: Kumo starts an empty task the user asked for, and may move their input there
//! (docs/development/tools.md).

use kumochi_protocol::message::{ThreadId, ThreadTitle};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LocalWrite, Prepared, Tool, ToolError, ToolOutput, parameters, parse};
use crate::engines::ToolName;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateTaskArgs {
    /// The task's title: one line of 1 to 80 characters.
    title: String,
    /// Move the user's next message to the new task.
    focus: bool,
}

fn prepare(args: &[u8]) -> Result<Prepared, ToolError> {
    let (create, hash) = parse::<CreateTaskArgs>("create_task", args)?;
    let title = ThreadTitle::new(&create.title).map_err(|_| ToolError::Invalid)?;
    Ok(Prepared::Local {
        hash,
        write: LocalWrite::CreateTask {
            thread: ThreadId(0),
            title,
            focus: create.focus,
        },
        output: answer(ThreadId(0), create.focus),
    })
}

/// What the model reads once the task exists; the id is the one the coordinator reserved.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the tool's own focus argument, read once"
)]
pub(super) fn answer(thread: ThreadId, focus: bool) -> ToolOutput {
    let id = thread.0;
    ToolOutput::Text(if focus {
        format!(
            "Created task {id}. The user's next message should go to it, unless they have \
             already moved elsewhere."
        )
    } else {
        format!("Created task {id}.")
    })
}

pub(super) const CREATE_TASK: Tool = Tool {
    name: ToolName::CreateTask,
    wire: "create_task",
    description: "Creates an empty task, a separate conversation, with no reply in it. Only when \
                  the user explicitly asks for a separate task or conversation; \"I have a task \
                  for you\" is not such an ask.",
    kumo_only: true,
    mcp: false,
    parameters: parameters::<CreateTaskArgs>,
    prepare,
};
