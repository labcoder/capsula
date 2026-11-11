//! Local issue drafting: checked content, never consent or a network operation.

use kumochi_protocol::message::{IssueBody, IssueTitle, RepoName};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LocalWrite, Prepared, Tool, ToolError, ToolOutput, parameters, parse};
use crate::engines::ToolName;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DraftIssueArgs {
    /// One nonblank line, at most 256 Unicode scalar values.
    title: String,
    /// Markdown body, normalized to LF and at most 32 KiB of UTF-8.
    body: String,
    /// Optional owner/repository; otherwise the current committed default may bind it.
    repo: Option<String>,
}

fn prepare(args: &[u8]) -> Result<Prepared, ToolError> {
    let (draft, hash) = parse::<DraftIssueArgs>("draft_issue", args)?;
    let title = IssueTitle::new(&draft.title).map_err(|_| ToolError::Invalid)?;
    let body = IssueBody::new(&draft.body).map_err(|_| ToolError::Invalid)?;
    let repo = draft.repo.as_deref().map(RepoName::new).transpose()
        .map_err(|_| ToolError::Invalid)?;
    Ok(Prepared::Local {
        hash,
        write: LocalWrite::DraftIssue { title, body, repo, account: None },
        output: ToolOutput::Text("Saved a local issue draft for user review. Nothing was filed on GitHub.".to_owned()),
    })
}

pub(super) const DRAFT_ISSUE: Tool = Tool {
    name: ToolName::DraftIssue,
    wire: "draft_issue",
    description: "Saves a local GitHub issue draft in this conversation for the user to review. Does not file an issue, authorize a send or contact GitHub. The user must review and explicitly confirm filing.",
    kumo_only: false,
    mcp: false,
    parameters: parameters::<DraftIssueArgs>,
    prepare,
};
