//! The audit of a Claude Code launch against its reviewed facts, in two parts: before any stdin
//! (`initialize` and `mcp_status`) and after the seed (`system:init`). It judges what C6's mapper
//! parsed and returns a closed reason, never what it saw.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kumochi_protocol::constants::MCP_SERVER_NAME;
use kumochi_protocol::message::AuditFailed;
use serde::Deserialize;
use serde_json::Value;

use super::policy::Reviewed;
use super::stream::{ControlBody, ControlResponse, Field, InitFacts};

/// What a reviewed version reports when nothing but Kumochi's server is configured.
#[derive(Debug)]
pub(crate) struct AuditSchema {
    /// Exactly these agents, in any order.
    pub(crate) agents: &'static [&'static str],
    /// Exactly these plugins, each with path `builtin`.
    pub(crate) plugins: &'static [&'static str],
    /// `apiKeySource` values Kumochi accepts.
    pub(crate) api_key_sources: &'static [&'static str],
}

pub(crate) const INITIALIZE_ID: &str = "k-initialize";
pub(crate) const MCP_STATUS_ID: &str = "k-mcp-status";
/// `account.tokenSource` when Claude Code isn't signed in (P0, signed out).
const SIGNED_OUT: &str = "none";
/// `account.tokenSource` naming the stored Claude.ai sign-in; 2.1.306 omits it when that's the
/// only source.
const STORED_SIGN_IN: &str = "claude.ai";
/// `account.apiProvider` when questions go to Anthropic, not a third-party provider.
const FIRST_PARTY: &str = "firstParty";

/// The two `control_request` lines C5b writes to stdin right after spawn, before anything else.
pub(crate) fn control_requests() -> [&'static [u8]; 2] {
    [
        br#"{"type":"control_request","request_id":"k-initialize","request":{"subtype":"initialize"}}
"#,
        br#"{"type":"control_request","request_id":"k-mcp-status","request":{"subtype":"mcp_status"}}
"#,
    ]
}

/// The `mcp_status` request asked again while the server is `pending`; the `n`th repeat's id is
/// [`MCP_STATUS_ID`] with `-n` after it, so a late answer to an earlier ask can't pass for it.
fn mcp_status_again(n: u32) -> Vec<u8> {
    format!(
        "{{\"type\":\"control_request\",\"request_id\":\"{MCP_STATUS_ID}-{n}\",\"request\":{{\"subtype\":\"mcp_status\"}}}}\n"
    )
    .into_bytes()
}

/// Where the audit before input stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// An answer is still due.
    Waiting,
    /// Kumochi's server is still connecting: ask `mcp_status` again with [`mcp_status_again`].
    Pending,
    /// Both passed: the launch may be bound and seeded, but no tool may run yet.
    Ready,
    /// Claude Code isn't signed in: tear down before stdin and show `ClaudeCodeSignedOut`.
    SignedOut,
}

/// The audit before input, one per launch.
#[derive(Debug)]
pub(crate) struct PreInputAudit {
    reviewed: &'static Reviewed,
    tools: BTreeSet<String>,
    initialize: bool,
    mcp: bool,
    /// How many times `mcp_status` was asked again; the answer due is to the latest.
    asked_again: u32,
    /// initialize's `analytics_disabled`, once initialize passed.
    analytics: Option<bool>,
    /// `SignedOut` or a failure, once given.
    refused: Option<Result<Progress, AuditFailed>>,
}

impl PreInputAudit {
    /// `tools` is C5b's frozen `mcp__kumochi__*` set for the launch, never derived from replies.
    pub(crate) fn new(reviewed: &'static Reviewed, tools: BTreeSet<String>) -> Self {
        Self {
            reviewed,
            tools,
            initialize: false,
            mcp: false,
            asked_again: 0,
            analytics: None,
            refused: None,
        }
    }

    /// The line that asks `mcp_status` again, after a `Pending`.
    pub(crate) fn ask_again(&mut self) -> Vec<u8> {
        self.asked_again += 1;
        mcp_status_again(self.asked_again)
    }

    /// initialize's `analytics_disabled` once initialize passed: shown, never judged. `None`
    /// before that, and when it's absent or not a bool.
    pub(crate) fn analytics(&self) -> Option<bool> {
        self.analytics
    }

    /// Judges one control response. A refusal is final: every later response gets it again.
    ///
    /// # Errors
    /// The first check that failed. An error subtype, an unknown or repeated request id, or a
    /// body of the wrong shape is `Malformed`.
    pub(crate) fn control(&mut self, response: &ControlResponse) -> Result<Progress, AuditFailed> {
        if let Some(refusal) = self.refused {
            return refusal;
        }
        let verdict = self.judge(response);
        if !matches!(
            verdict,
            Ok(Progress::Waiting | Progress::Pending | Progress::Ready)
        ) {
            self.refused = Some(verdict);
        }
        verdict
    }

    /// Marks a response done only once it passed.
    fn judge(&mut self, response: &ControlResponse) -> Result<Progress, AuditFailed> {
        if response.subtype != "success" {
            return Err(AuditFailed::Malformed);
        }
        let mcp_id = match self.asked_again {
            0 => MCP_STATUS_ID.to_owned(),
            n => format!("{MCP_STATUS_ID}-{n}"),
        };
        match response.request_id.as_str() {
            INITIALIZE_ID if !self.initialize => {
                if !self.initialize_passes(&response.response)? {
                    return Ok(Progress::SignedOut);
                }
                self.initialize = true;
                self.analytics = response.response.rest["analytics_disabled"].as_bool();
            }
            id if id == mcp_id && !self.mcp => {
                if !self.mcp_passes(&response.response.rest)? {
                    return Ok(Progress::Pending);
                }
                self.mcp = true;
            }
            _ => return Err(AuditFailed::Malformed),
        }
        Ok(if self.initialize && self.mcp {
            Progress::Ready
        } else {
            Progress::Waiting
        })
    }

    /// `Ok(false)` when every check passed but Claude Code is signed out.
    fn initialize_passes(&self, whole: &ControlBody) -> Result<bool, AuditFailed> {
        let body = Initialize::deserialize(&whole.rest).map_err(|_| AuditFailed::Malformed)?;
        let schema = &self.reviewed.audit;
        if body.claude_code_version.parse() != Ok(self.reviewed.version) {
            return Err(AuditFailed::Version);
        }
        if !body.commands.is_empty() {
            return Err(AuditFailed::Commands);
        }
        let agents = body.agents.iter().map(|agent| agent.name.as_str());
        if !exactly(agents, schema.agents.iter().copied()) {
            return Err(AuditFailed::Agents);
        }
        let account = whole.account.as_ref().ok_or(AuditFailed::Malformed)?;
        let Field::Text(provider) = &account.api_provider else {
            return Err(AuditFailed::Malformed);
        };
        let token = match &account.token_source {
            Field::Absent => None,
            Field::Text(token) => Some(token.as_str()),
            Field::Null | Field::Other => return Err(AuditFailed::Malformed),
        };
        let subscribed = match account.subscription_type {
            Field::Absent => false,
            Field::Text(_) => true,
            Field::Null | Field::Other => return Err(AuditFailed::Malformed),
        };
        if provider != FIRST_PARTY {
            return Err(AuditFailed::TokenSource);
        }
        // Present at all, null included: a key source Kumochi didn't set.
        if account.api_key_source != Field::Absent {
            return Err(AuditFailed::KeySource);
        }
        match (token, subscribed) {
            (Some(SIGNED_OUT), false) => Ok(false),
            (None | Some(STORED_SIGN_IN), true) => Ok(true),
            _ => Err(AuditFailed::TokenSource),
        }
    }

    /// `Ok(false)` while Kumochi's one server is still `pending`, which Claude Code reports until
    /// the server has connected (2.1.306 starts connecting it at start-up).
    fn mcp_passes(&self, body: &Value) -> Result<bool, AuditFailed> {
        let body = McpStatus::deserialize(body).map_err(|_| AuditFailed::Malformed)?;
        let [server] = body.servers.as_slice() else {
            return Err(AuditFailed::McpServer);
        };
        if server.name != MCP_SERVER_NAME {
            return Err(AuditFailed::McpServer);
        }
        match server.status.as_str() {
            "connected" => {}
            "pending" => return Ok(false),
            _ => return Err(AuditFailed::McpServer),
        }
        let tools = server.tools.as_ref().ok_or(AuditFailed::Malformed)?;
        let prefix = format!("mcp__{MCP_SERVER_NAME}__");
        let names: Vec<String> = tools
            .iter()
            .map(|tool| format!("{prefix}{}", tool.name))
            .collect();
        if !exactly(
            names.iter().map(String::as_str),
            self.tools.iter().map(String::as_str),
        ) {
            return Err(AuditFailed::Tools);
        }
        Ok(true)
    }
}

/// The audit after the seed, one per launch.
#[derive(Debug)]
pub(crate) struct PostSeedAudit {
    reviewed: &'static Reviewed,
    tools: BTreeSet<String>,
    seen: Option<InitFacts>,
}

/// What init reported that's shown and never judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InitRecord {
    pub(crate) analytics_disabled: Option<bool>,
}

impl PostSeedAudit {
    /// `tools` is the same frozen set the audit before input judged.
    pub(crate) fn new(reviewed: &'static Reviewed, tools: BTreeSet<String>) -> Self {
        Self {
            reviewed,
            tools,
            seen: None,
        }
    }

    /// Judges a `system:init`. The first must match the reviewed facts; a later one must equal
    /// the first.
    ///
    /// # Errors
    /// The first check that failed; a later init that differs from the first is `Malformed`.
    pub(crate) fn init(&mut self, facts: InitFacts) -> Result<InitRecord, AuditFailed> {
        let record = InitRecord {
            analytics_disabled: facts.analytics_disabled,
        };
        if let Some(first) = &self.seen {
            return if *first == facts {
                Ok(record)
            } else {
                Err(AuditFailed::Malformed)
            };
        }
        let schema = &self.reviewed.audit;
        if facts.claude_code_version.parse() != Ok(self.reviewed.version) {
            return Err(AuditFailed::Version);
        }
        let tools = facts.tools.iter().map(String::as_str);
        if !exactly(tools, self.tools.iter().map(String::as_str)) {
            return Err(AuditFailed::Tools);
        }
        let [server] = facts.mcp_servers.as_slice() else {
            return Err(AuditFailed::McpServer);
        };
        if server.name != MCP_SERVER_NAME || server.status != "connected" {
            return Err(AuditFailed::McpServer);
        }
        let builtin = facts.plugins.iter().all(|plugin| plugin.path == "builtin");
        let plugins = facts.plugins.iter().map(|plugin| plugin.name.as_str());
        if !builtin || !exactly(plugins, schema.plugins.iter().copied()) {
            return Err(AuditFailed::Plugins);
        }
        if !facts.skills.is_empty() || !facts.slash_commands.is_empty() {
            return Err(AuditFailed::Skills);
        }
        let agents = facts.agents.iter().map(String::as_str);
        if !exactly(agents, schema.agents.iter().copied()) {
            return Err(AuditFailed::Agents);
        }
        if !schema
            .api_key_sources
            .contains(&facts.api_key_source.as_str())
        {
            return Err(AuditFailed::KeySource);
        }
        self.seen = Some(facts);
        Ok(record)
    }
}

/// The same names as `expected`, in any order, with no duplicates.
fn exactly<'a>(
    names: impl Iterator<Item = &'a str>,
    expected: impl Iterator<Item = &'a str>,
) -> bool {
    let names: Vec<&str> = names.collect();
    let set: BTreeSet<&str> = names.iter().copied().collect();
    set.len() == names.len() && set == expected.collect()
}

#[derive(Deserialize)]
struct Initialize {
    claude_code_version: String,
    commands: Vec<Value>,
    agents: Vec<Named>,
}

#[derive(Deserialize)]
struct McpStatus {
    #[serde(rename = "mcpServers")]
    servers: Vec<StatusServer>,
}

#[derive(Deserialize)]
struct StatusServer {
    name: String,
    status: String,
    /// Absent while the server is `pending`.
    tools: Option<Vec<Named>>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

const PENDING: u8 = 0;
const PASSED: u8 = 1;
const FAILED: u8 = 2;

/// Whether this launch may admit a `tools/call`: C5b writes it, C4b's listener reads it, and the
/// core fails it when it revokes the launch's binding. It starts pending; failure is terminal, so
/// a pass that races a failure loses.
#[derive(Clone)]
pub(crate) struct AuditGate(Arc<Shared>);

struct Shared {
    state: AtomicU8,
    /// Woken once the gate fails, for the waits that must end on it.
    failed: tokio::sync::Notify,
}

/// A gate's state, from one read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The post-seed audit hasn't passed yet.
    Pending,
    Passed,
    /// Failed or revoked, for good.
    Failed,
}

impl AuditGate {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Shared {
            state: AtomicU8::new(PENDING),
            failed: tokio::sync::Notify::new(),
        }))
    }

    /// Only after this launch's post-seed audit passed; nothing once failed.
    pub(crate) fn pass(&self) {
        self.0
            .state
            .compare_exchange(PENDING, PASSED, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_default();
    }

    /// Failure or revocation, from any state. A tool already admitted isn't undone; a call still
    /// waiting for admission is refused, and every wait on [`Self::failed`] wakes.
    pub(crate) fn fail(&self) {
        self.0.state.store(FAILED, Ordering::Release);
        self.0.failed.notify_waiters();
    }

    pub(crate) fn standing(&self) -> Standing {
        match self.0.state.load(Ordering::Acquire) {
            PENDING => Standing::Pending,
            PASSED => Standing::Passed,
            _ => Standing::Failed,
        }
    }

    pub(crate) fn may_call(&self) -> bool {
        self.standing() == Standing::Passed
    }

    /// Resolves once the gate has failed, at once when it already has. The wait registers
    /// before it reads the state, so a failure between the two still wakes it.
    pub(crate) async fn failed(&self) {
        loop {
            let failed = self.0.failed.notified();
            tokio::pin!(failed);
            failed.as_mut().enable();
            if self.standing() == Standing::Failed {
                return;
            }
            failed.await;
        }
    }
}

impl fmt::Debug for AuditGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.standing() {
            Standing::Pending => "Pending",
            Standing::Passed => "Passed",
            Standing::Failed => "Failed",
        };
        f.debug_tuple("AuditGate").field(&state).finish()
    }
}

#[cfg(test)]
mod tests;
