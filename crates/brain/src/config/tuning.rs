//! `[tuning]` in `config.toml`: optional overrides of built-in limits, read once at start.
//!
//! Each key has a default, the built-in value, and a range. A value of the wrong type uses its
//! default, and one outside its range is clamped to the nearer edge. A group whose values don't
//! fit together uses all its defaults. Logs name only keys this build owns: an unknown key may
//! hold anything, so it's counted.

use std::time::Duration;

use kumochi_protocol::message::{HOLD_DRAIN_TIMEOUT_MS, HOLD_START_TIMEOUT_MS, ShellTuning};
use toml::{Table, Value};

use crate::connections::{anthropic::messages, model, oauth};
use crate::coordinator;
use crate::engines::claude_code::supervisor::MAX_LAUNCH;

const TUNING: &str = "tuning";

/// One tunable value: its key in its group, its built-in default and its allowed range.
pub(crate) struct Limit {
    pub(crate) key: &'static str,
    pub(crate) default: u64,
    pub(crate) min: u64,
    pub(crate) max: u64,
}

const fn limit(key: &'static str, default: u64, min: u64, max: u64) -> Limit {
    Limit {
        key,
        default,
        min,
        max,
    }
}

/// Every group by its name under `[tuning]`, for the docs check.
#[cfg(test)]
pub(crate) const GROUPS: [(&str, &[Limit]); 8] = [
    ("github", &GITHUB),
    ("model", &MODEL),
    ("runs", &RUNS),
    ("oauth", &OAUTH),
    ("tokens", &TOKENS),
    ("drawing", &DRAWING),
    ("context", &CONTEXT),
    ("hold", &HOLD),
];

/// `[tuning.github]`. These bound requests to the user's account, so every build keeps them in
/// range and they only tighten.
pub(crate) const GITHUB: [Limit; 4] = [
    limit("filing_s", 60, 20, 60),
    limit("filing_requests_count", 16, 12, 16),
    limit("listing_s", 60, 20, 60),
    limit("listing_requests_count", 12, 4, 12),
];

/// `[tuning.model]`: waits on a model service's reply, defaulting to the built-in constants. The
/// pool only shortens, because a longer one would reuse connections the service may have closed.
pub(crate) const MODEL: [Limit; 7] = [
    limit(
        "send_s",
        model::SEND_DEADLINE.as_secs(),
        3,
        model::SEND_DEADLINE.as_secs(),
    ),
    limit("idle_s", model::IDLE.as_secs(), 15, 240),
    limit("error_body_s", model::ERROR_BODY_DEADLINE.as_secs(), 3, 40),
    limit("drain_ms", 1000, 250, 4000),
    limit("pool_idle_s", model::conn::POOL_IDLE.as_secs(), 10, 50),
    limit("backpressure_s", messages::BACKPRESSURE.as_secs(), 15, 240),
    limit(
        "content_idle_s",
        messages::CONTENT_IDLE.as_secs(),
        150,
        2400,
    ),
];

/// `[tuning.runs]`. The counts bound model calls, so they only tighten from the built-in ceilings
/// `MAX_ROUNDS` and `MAX_CALLS`, which also size the engine's attempt budget at compile time.
pub(crate) const RUNS: [Limit; 6] = [
    limit("rounds_count", 8, 2, 8),
    limit("calls_count", 16, 2, 16),
    limit("wall_clock_s", coordinator::WALL_CLOCK.as_secs(), 30, 240),
    limit("finding_wait_s", coordinator::FINDING_WAIT.as_secs(), 8, 60),
    limit(
        "send_lease_s",
        coordinator::SEND_DEADLINE.as_secs(),
        model::SEND_DEADLINE.as_secs() + 1,
        coordinator::SEND_DEADLINE.as_secs(),
    ),
    limit(
        "claude_launch_s",
        MAX_LAUNCH.as_secs(),
        60,
        MAX_LAUNCH.as_secs(),
    ),
];

const OAUTH: [Limit; 5] = [
    limit(
        "sign_in_s",
        oauth::ATTEMPT_LIMIT.as_secs(),
        75,
        oauth::ATTEMPT_LIMIT.as_secs(),
    ),
    limit(
        "refresh_s",
        oauth::refresh::DEADLINE.as_secs(),
        8,
        oauth::refresh::DEADLINE.as_secs(),
    ),
    limit("revoke_s", oauth::revoke::BUDGET.as_secs(), 3, 40),
    limit("first_backoff_ms", 500, 125, 2000),
    limit("second_backoff_ms", 1000, 250, 4000),
];

const TOKENS: [Limit; 2] = [
    limit("github_refresh_lead_s", 60, 30, 60),
    limit("chatgpt_refresh_lead_s", 60, 30, 60),
];

const DRAWING: [Limit; 5] = [
    limit("calls_count", 16, 1, 16),
    limit("rejected_count", 8, 1, 8),
    limit("ready_wait_ms", 1000, 250, 1000),
    limit("first_retry_ms", 500, 250, 2000),
    limit("last_retry_ms", 30_000, 1000, 60_000),
];

const CONTEXT: [Limit; 6] = [
    limit("compact_at_token_count", 24_000, 24_000, 48_000),
    limit("summary_input_bytes", 98_304, 49_152, 98_304),
    limit("raw_runs_count", 8, 2, 8),
    limit("notes_count", 32, 16, 32),
    limit("reread_bytes", 32_768, 16_384, 32_768),
    limit("read_events_count", 20, 10, 20),
];

const HOLD: [Limit; 2] = [
    limit(
        "start_ms",
        HOLD_START_TIMEOUT_MS,
        ShellTuning::MIN_START_MS,
        HOLD_START_TIMEOUT_MS,
    ),
    limit(
        "drain_ms",
        HOLD_DRAIN_TIMEOUT_MS,
        ShellTuning::MIN_DRAIN_MS,
        HOLD_DRAIN_TIMEOUT_MS,
    ),
];

pub(super) fn shell_tuning(table: &Table) -> ShellTuning {
    let [start, drain] = read(table, "hold", &HOLD, |_| true);
    ShellTuning::from_ms(start, drain).expect("the hold table uses the protocol bounds")
}

/// Requests a filing makes besides its listing: the user, two match pages and the create.
const FILING_EXTRA_REQUESTS: u64 = 4;

/// GitHub's filing and listing budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GitHubTuning {
    /// One whole filing or review rearm, retries included; local settlement may finish after it.
    pub(crate) filing_deadline: Duration,
    /// Counted requests in one filing, its listing included.
    pub(crate) filing_requests: usize,
    /// One repository listing.
    pub(crate) listing_budget: Duration,
    /// Page requests in one repository listing.
    pub(crate) listing_requests: usize,
}

impl Default for GitHubTuning {
    fn default() -> Self {
        Self::from_values(defaults(&GITHUB))
    }
}

impl GitHubTuning {
    /// `[tuning.github]` from the settings table, or the defaults.
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(
            table,
            "github",
            &GITHUB,
            |[filing_s, filing, listing_s, listing]| {
                filing >= listing + FILING_EXTRA_REQUESTS && listing_s <= filing_s
            },
        ))
    }

    fn from_values([filing_s, filing_requests, listing_s, listing_requests]: [u64; 4]) -> Self {
        Self {
            filing_deadline: Duration::from_secs(filing_s),
            filing_requests: count(filing_requests),
            listing_budget: Duration::from_secs(listing_s),
            listing_requests: count(listing_requests),
        }
    }
}

/// How long a model request may wait at each step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelTuning {
    /// Connecting and uploading one request.
    pub(crate) send: Duration,
    /// Each read of the reply.
    pub(crate) idle: Duration,
    /// Reading a refusal's body.
    pub(crate) error_body: Duration,
    pub(crate) drain: Duration,
    /// How long an unused connection stays reusable.
    pub(crate) pool_idle: Duration,
    /// Anthropic: waiting for room to pass the reply on.
    pub(crate) backpressure: Duration,
    /// Anthropic: a reply with no new content, though bytes keep arriving.
    pub(crate) content_idle: Duration,
}

impl Default for ModelTuning {
    fn default() -> Self {
        Self::from_values(defaults(&MODEL))
    }
}

impl ModelTuning {
    /// `[tuning.model]` from the settings table, or the defaults.
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(
            table,
            "model",
            &MODEL,
            |[_, idle_s, .., content_idle_s]| content_idle_s >= idle_s,
        ))
    }

    fn from_values(
        [
            send_s,
            idle_s,
            error_body_s,
            drain_ms,
            pool_idle_s,
            backpressure_s,
            content_idle_s,
        ]: [u64; 7],
    ) -> Self {
        Self {
            send: Duration::from_secs(send_s),
            idle: Duration::from_secs(idle_s),
            error_body: Duration::from_secs(error_body_s),
            drain: Duration::from_millis(drain_ms),
            pool_idle: Duration::from_secs(pool_idle_s),
            backpressure: Duration::from_secs(backpressure_s),
            content_idle: Duration::from_secs(content_idle_s),
        }
    }
}

/// A run's budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunTuning {
    /// Model rounds that may ask for tools.
    pub(crate) rounds: u32,
    /// Tool calls in one run.
    pub(crate) calls: u32,
    /// The whole run.
    pub(crate) wall_clock: Duration,
    /// Waiting for Claude Code to be found before a typed turn on it gives up.
    pub(crate) finding_wait: Duration,
    pub(crate) send_lease: Duration,
    pub(crate) claude_launch: Duration,
}

impl Default for RunTuning {
    fn default() -> Self {
        Self::from_values(defaults(&RUNS))
    }
}

impl RunTuning {
    /// `[tuning.runs]` from the settings table, or the defaults.
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(table, "runs", &RUNS, |_| true))
    }

    fn from_values(
        [
            rounds,
            calls,
            wall_clock_s,
            finding_wait_s,
            send_lease_s,
            claude_launch_s,
        ]: [u64; 6],
    ) -> Self {
        let count = |value: u64| u32::try_from(value).expect("a run count is at most 16");
        Self {
            rounds: count(rounds),
            calls: count(calls),
            wall_clock: Duration::from_secs(wall_clock_s),
            finding_wait: Duration::from_secs(finding_wait_s),
            send_lease: Duration::from_secs(send_lease_s),
            claude_launch: Duration::from_secs(claude_launch_s),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OAuthTuning {
    pub(crate) sign_in: Duration,
    pub(crate) refresh: Duration,
    pub(crate) revoke: Duration,
    pub(crate) backoff: [Duration; 2],
}

impl Default for OAuthTuning {
    fn default() -> Self {
        Self::from_values(defaults(&OAUTH))
    }
}

impl OAuthTuning {
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(
            table,
            "oauth",
            &OAUTH,
            |[_, _, revoke_s, first_ms, second_ms]| {
                first_ms <= second_ms && first_ms + second_ms < revoke_s * 1000
            },
        ))
    }

    fn from_values([sign_in_s, refresh_s, revoke_s, first_ms, second_ms]: [u64; 5]) -> Self {
        Self {
            sign_in: Duration::from_secs(sign_in_s),
            refresh: Duration::from_secs(refresh_s),
            revoke: Duration::from_secs(revoke_s),
            backoff: [first_ms, second_ms].map(Duration::from_millis),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TokenTuning {
    pub(crate) github_refresh_lead: Duration,
    pub(crate) chatgpt_refresh_lead: Duration,
}

impl Default for TokenTuning {
    fn default() -> Self {
        Self::from_values(defaults(&TOKENS))
    }
}

impl TokenTuning {
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(table, "tokens", &TOKENS, |_| true))
    }

    fn from_values([github, chatgpt]: [u64; 2]) -> Self {
        Self {
            github_refresh_lead: Duration::from_secs(github),
            chatgpt_refresh_lead: Duration::from_secs(chatgpt),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrawingTuning {
    pub(crate) calls: usize,
    pub(crate) rejected: u8,
    pub(crate) ready_wait: Duration,
    pub(crate) first_retry: Duration,
    pub(crate) last_retry: Duration,
}

impl Default for DrawingTuning {
    fn default() -> Self {
        Self::from_values(defaults(&DRAWING))
    }
}

impl DrawingTuning {
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(
            table,
            "drawing",
            &DRAWING,
            |[_, _, _, first, last]| first <= last,
        ))
    }

    fn from_values([calls, rejected, ready, first, last]: [u64; 5]) -> Self {
        Self {
            calls: count(calls),
            rejected: u8::try_from(rejected).expect("rejected count is bounded to eight"),
            ready_wait: Duration::from_millis(ready),
            first_retry: Duration::from_millis(first),
            last_retry: Duration::from_millis(last),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextTuning {
    pub(crate) compact_at: u64,
    pub(crate) summary_input: usize,
    pub(crate) raw_runs: usize,
    pub(crate) notes: usize,
    pub(crate) reread: usize,
    pub(crate) read_events: usize,
}

impl Default for ContextTuning {
    fn default() -> Self {
        Self::from_values(defaults(&CONTEXT))
    }
}

impl ContextTuning {
    pub(crate) fn from_table(table: &Table) -> Self {
        Self::from_values(read(table, "context", &CONTEXT, |_| true))
    }

    fn from_values([compact_at, summary, raw_runs, notes, reread, read_events]: [u64; 6]) -> Self {
        Self {
            compact_at,
            summary_input: count(summary),
            raw_runs: count(raw_runs),
            notes: count(notes),
            reread: count(reread),
            read_events: count(read_events),
        }
    }
}

fn count(value: u64) -> usize {
    usize::try_from(value).expect("bounded tuning values fit usize")
}

fn defaults<const N: usize>(limits: &[Limit; N]) -> [u64; N] {
    limits.each_ref().map(|limit| limit.default)
}

/// `[tuning.<name>]`'s values in `limits`' order, or every default when the group isn't a table
/// or its values don't `fit` together.
fn read<const N: usize>(
    table: &Table,
    name: &'static str,
    limits: &[Limit; N],
    fit: impl Fn([u64; N]) -> bool,
) -> [u64; N] {
    let Some(group) = group(table, name) else {
        return defaults(limits);
    };
    let unknown = group
        .keys()
        .filter(|key| limits.iter().all(|limit| limit.key != key.as_str()))
        .count();
    if unknown > 0 {
        tracing::warn!(group = name, unknown, "unknown tuning keys; ignoring them");
    }
    let values = limits.each_ref().map(|limit| value(group, limit));
    if !fit(values) {
        tracing::warn!(
            group = name,
            "tuning values don't fit together; using the group's defaults"
        );
        return defaults(limits);
    }
    values
}

/// `[tuning.<name>]` when it's a table.
fn group<'a>(table: &'a Table, name: &'static str) -> Option<&'a Table> {
    let Value::Table(tuning) = table.get(TUNING)? else {
        tracing::warn!(key = TUNING, "tuning isn't a table; using the defaults");
        return None;
    };
    let Value::Table(group) = tuning.get(name)? else {
        tracing::warn!(
            group = name,
            "tuning group isn't a table; using its defaults"
        );
        return None;
    };
    Some(group)
}

fn value(group: &Table, limit: &Limit) -> u64 {
    let Some(value) = group.get(limit.key) else {
        return limit.default;
    };
    let Some(value) = value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
    else {
        tracing::warn!(
            key = limit.key,
            "tuning value isn't a whole number; using its default"
        );
        return limit.default;
    };
    if !(limit.min..=limit.max).contains(&value) {
        tracing::warn!(
            key = limit.key,
            "tuning value is out of range; using the nearer edge"
        );
    }
    value.clamp(limit.min, limit.max)
}

#[cfg(test)]
#[path = "tuning_tests.rs"]
mod tests;
