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
use super::stream::{ControlResponse, InitFacts};

/// What a reviewed version reports when nothing but Kumochi's server is configured.
#[derive(Debug)]
pub(crate) struct AuditSchema {
    /// Exactly these agents, in any order.
    pub(crate) agents: &'static [&'static str],
    /// `account.tokenSource` values that are a sign-in Kumochi accepts.
    pub(crate) token_sources: &'static [&'static str],
    /// Exactly these plugins, each with path `builtin`.
    pub(crate) plugins: &'static [&'static str],
    /// `apiKeySource` values Kumochi accepts.
    pub(crate) api_key_sources: &'static [&'static str],
}

pub(crate) const INITIALIZE_ID: &str = "k-initialize";
pub(crate) const MCP_STATUS_ID: &str = "k-mcp-status";
/// `account.tokenSource` when Claude Code isn't signed in (P0, signed out).
const SIGNED_OUT: &str = "none";

/// The two `control_request` lines C5b writes to stdin right after spawn, before anything else.
pub(crate) fn control_requests() -> [&'static [u8]; 2] {
    [
        br#"{"type":"control_request","request_id":"k-initialize","request":{"subtype":"initialize"}}
"#,
        br#"{"type":"control_request","request_id":"k-mcp-status","request":{"subtype":"mcp_status"}}
"#,
    ]
}

/// Where the audit before input stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// An answer is still due.
    Waiting,
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
            refused: None,
        }
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
        if !matches!(verdict, Ok(Progress::Waiting | Progress::Ready)) {
            self.refused = Some(verdict);
        }
        verdict
    }

    /// Marks a response done only once it passed.
    fn judge(&mut self, response: &ControlResponse) -> Result<Progress, AuditFailed> {
        if response.subtype != "success" {
            return Err(AuditFailed::Malformed);
        }
        match response.request_id.as_str() {
            INITIALIZE_ID if !self.initialize => {
                if !self.initialize_passes(&response.response)? {
                    return Ok(Progress::SignedOut);
                }
                self.initialize = true;
            }
            MCP_STATUS_ID if !self.mcp => {
                self.mcp_passes(&response.response)?;
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
    fn initialize_passes(&self, body: &Value) -> Result<bool, AuditFailed> {
        let body = Initialize::deserialize(body).map_err(|_| AuditFailed::Malformed)?;
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
        let token = body.account.token_source.as_str();
        if token == SIGNED_OUT {
            return Ok(false);
        }
        if !schema.token_sources.contains(&token) {
            return Err(AuditFailed::TokenSource);
        }
        Ok(true)
    }

    fn mcp_passes(&self, body: &Value) -> Result<(), AuditFailed> {
        let body = McpStatus::deserialize(body).map_err(|_| AuditFailed::Malformed)?;
        let [server] = body.servers.as_slice() else {
            return Err(AuditFailed::McpServer);
        };
        if server.name != MCP_SERVER_NAME || server.status != "connected" {
            return Err(AuditFailed::McpServer);
        }
        let prefix = format!("mcp__{MCP_SERVER_NAME}__");
        let names: Vec<String> = server
            .tools
            .iter()
            .map(|tool| format!("{prefix}{}", tool.name))
            .collect();
        if !exactly(
            names.iter().map(String::as_str),
            self.tools.iter().map(String::as_str),
        ) {
            return Err(AuditFailed::Tools);
        }
        Ok(())
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
    account: Account,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "tokenSource")]
    token_source: String,
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
    tools: Vec<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

const PENDING: u8 = 0;
const PASSED: u8 = 1;
const FAILED: u8 = 2;

/// Whether this launch may admit a `tools/call`: C5b writes it, C4b's listener reads it.
/// It starts pending; failure is terminal, so a pass that races a failure loses.
#[derive(Clone)]
pub(crate) struct AuditGate(Arc<AtomicU8>);

impl AuditGate {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(PENDING)))
    }

    /// Only after this launch's post-seed audit passed; nothing once failed.
    pub(crate) fn pass(&self) {
        self.0
            .compare_exchange(PENDING, PASSED, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_default();
    }

    /// Failure or revocation, from any state. A tool already admitted isn't undone; a call still
    /// waiting for admission is refused.
    pub(crate) fn fail(&self) {
        self.0.store(FAILED, Ordering::Release);
    }

    pub(crate) fn may_call(&self) -> bool {
        self.0.load(Ordering::Acquire) == PASSED
    }
}

impl fmt::Debug for AuditGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.0.load(Ordering::Acquire) {
            PENDING => "Pending",
            PASSED => "Passed",
            _ => "Failed",
        };
        f.debug_tuple("AuditGate").field(&state).finish()
    }
}

#[cfg(test)]
mod tests;
