//! Local issue drafting: checked content, never consent or a network operation.

use kumochi_protocol::message::{IssueBody, IssueTitle, RepoName};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LocalWrite, Prepared, Tool, ToolError, ToolOutput, parameters, parse};
use crate::engines::ToolName;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DraftIssueArgs {
    /// One nonblank line, at most 256 Unicode scalar values; no hidden format characters.
    title: String,
    /// Literal Markdown, normalized to LF. All arguments together must fit 16 KiB,
    /// including JSON keys and escaping; the stored body's separate cap is 32 KiB.
    body: String,
    /// Optional owner/repository; otherwise the current committed default may bind it.
    repo: Option<String>,
}

fn prepare(args: &[u8]) -> Result<Prepared, ToolError> {
    let (draft, hash) = parse::<DraftIssueArgs>("draft_issue", args)?;
    if draft.title.chars().chain(draft.body.chars()).any(|c| {
        matches!(c,
            '\u{00ad}' | '\u{034f}' | '\u{0600}'..='\u{0605}' | '\u{061c}'
            | '\u{06dd}' | '\u{070f}' | '\u{0890}'..='\u{0891}' | '\u{08e2}'
            | '\u{115f}'..='\u{1160}' | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{ffa0}'
            | '\u{fff0}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}'
        )
    }) {
        return Err(ToolError::Invalid);
    }
    let title = IssueTitle::new(&draft.title).map_err(|_| ToolError::Invalid)?;
    let body = IssueBody::new(&draft.body).map_err(|_| ToolError::Invalid)?;
    let repo = draft
        .repo
        .as_deref()
        .map(RepoName::new)
        .transpose()
        .map_err(|_| ToolError::Invalid)?;
    Ok(Prepared::Local {
        hash,
        write: LocalWrite::DraftIssue {
            title,
            body,
            repo,
            account: None,
        },
        output: ToolOutput::Text(
            "Saved a local issue draft for user review. Nothing was filed on GitHub.".to_owned(),
        ),
    })
}

pub(super) const DRAFT_ISSUE: Tool = Tool {
    name: ToolName::DraftIssue,
    wire: "draft_issue",
    description: "Saves a GitHub issue as a draft card in this conversation. Use it when the user \
                  asks to file, open or report an issue; the user then files it from the card. \
                  This tool itself does not file, authorize a send or contact GitHub. Keep the complete serialized arguments \
                  within 16 KiB, including keys and JSON escaping. Do not include hidden format \
                  or direction controls, joiners, fillers or variation selectors. Preview text is \
                  literal; GitHub interprets visible mentions, links and Markdown if later filed.",
    kumo_only: false,
    mcp: false,
    parameters: parameters::<DraftIssueArgs>,
    prepare,
};
