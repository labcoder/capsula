//! Bounded recall selectors: persisted decisions, never copied source text.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "14b-2 activates asynchronous model recall")
)]

use std::fmt;

use serde::{Deserialize, Serialize};

pub(crate) const TEXT_BYTES: usize = 32 * 1024;
pub(crate) const SNIPPET_BYTES: usize = 4096;
pub(crate) const QUERY_BYTES: usize = 1024;
pub(crate) const QUERY_WORDS: usize = 32;
pub(crate) const SEARCH_HITS: usize = 10;
pub(crate) const READ_EVENTS: usize = 20;
pub(crate) const TASKS: usize = 100;
pub(crate) const CUT: &str = "The rest of this cut event cannot be read with read_history.";
pub(crate) const MORE: &str = "More matches may exist; narrow the words or name a task.";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Query {
    Search { query: String, task: Option<u64> },
    Read { task: u64, from: u64, to: u64 },
    Tasks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("invalid history arguments")]
    Invalid,
    #[error("not available here")]
    NotHere,
    #[error("no such task")]
    NoTask,
    #[error("can't read history")]
    Read,
}

pub(crate) fn id(value: u64) -> bool {
    value > 0 && i64::try_from(value).is_ok()
}

pub(crate) fn words(query: &str) -> Result<String, Error> {
    let words: Vec<_> = query.split_whitespace().collect();
    if query.len() > QUERY_BYTES
        || query.chars().any(char::is_control)
        || words.is_empty()
        || words.len() > QUERY_WORDS
        || words
            .iter()
            .any(|word| !word.chars().any(char::is_alphanumeric))
    {
        return Err(Error::Invalid);
    }
    Ok(words
        .iter()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR "))
}

impl Query {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let valid = match self {
            Self::Search { query, task } => words(query).is_ok() && task.is_none_or(id),
            Self::Read { task, from, to } => id(*task) && id(*from) && id(*to) && from <= to,
            Self::Tasks => true,
        };
        valid.then_some(()).ok_or(Error::Invalid)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hit {
    pub(crate) task: u64,
    pub(crate) event: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Selection {
    Search {
        query: String,
        hits: Vec<Hit>,
        more: bool,
    },
    Read {
        task: u64,
        first_shown: Option<u64>,
        last_shown: Option<u64>,
        cut: Option<usize>,
        next: Option<u64>,
    },
    Tasks {
        tasks: Vec<u64>,
        truncated: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    recall: Selection,
}

impl Selection {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let valid = match self {
            Self::Search { query, hits, more } => {
                words(query).is_ok()
                    && hits.len() <= SEARCH_HITS
                    && (!more || hits.len() == SEARCH_HITS)
                    && hits.iter().all(|hit| id(hit.task) && id(hit.event))
                    && hits
                        .iter()
                        .enumerate()
                        .all(|(at, hit)| !hits[..at].iter().any(|other| other.event == hit.event))
            }
            Self::Read {
                task,
                first_shown,
                last_shown,
                cut,
                next,
            } => {
                id(*task)
                    && match (first_shown, last_shown) {
                        (None, None) => cut.is_none() && next.is_none(),
                        (Some(first), Some(last)) => {
                            id(*first)
                                && id(*last)
                                && first <= last
                                && cut.is_none_or(|bytes| bytes > 0 && bytes <= TEXT_BYTES)
                                && next.is_none_or(|next| id(next) && *last < next)
                        }
                        _ => false,
                    }
            }
            Self::Tasks { tasks, .. } => {
                tasks.len() <= TASKS
                    && tasks.iter().copied().all(id)
                    && tasks
                        .iter()
                        .enumerate()
                        .all(|(at, task)| !tasks[..at].contains(task))
            }
        };
        valid.then_some(()).ok_or(Error::Invalid)
    }

    pub(crate) fn decode(stored: &str) -> Option<Self> {
        if stored.len() > crate::dispatch::MAX_OUTPUT {
            return None;
        }
        let envelope: Envelope = serde_json::from_str(stored).ok()?;
        envelope.recall.validate().ok()?;
        Some(envelope.recall)
    }

    pub(crate) fn decode_round(body: &str) -> Option<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Round {
            #[serde(rename = "call")]
            _call: String,
            output: Envelope,
        }
        let round: Round = serde_json::from_str(body).ok()?;
        round.output.recall.validate().ok()?;
        Some(round.output.recall)
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Search { .. } => "Search",
            Self::Read { .. } => "Read",
            Self::Tasks => "Tasks",
        })
    }
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Search { .. } => "SearchRefs",
            Self::Read { .. } => "ReadRef",
            Self::Tasks { .. } => "TaskRefs",
        })
    }
}

#[cfg(test)]
mod tests;
