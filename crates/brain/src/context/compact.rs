//! Compaction's request: the previous summary and the oldest runs no summary covers, rendered
//! the way a request renders them, in one read transaction (plans/slice-3.md PR 17;
//! docs/development/engines.md, "Compaction").

use std::collections::BTreeSet;

use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_protocol::message::{EventId, ThreadId, ThreadKind};

use super::bound::{fixed_bytes, item_bytes};
use super::build::{fill, render};
use super::{
    Body, BuildError, Built, InputItem, Item, ModelRequest, Origin, RAW_RUNS, Role, RunSnapshot,
};
use crate::store::Generation;
use crate::store::context::{Snapshot, Speaker};

const SUMMARY_PROMPT: &str = include_str!("../../prompts/summary.md");
/// The uncovered backlog, in estimated tokens, at which a task compacts after a run [guess].
pub(crate) const COMPACT_AT: u64 = 24_000;
/// The most a summary request may carry, prompt and previous summary included [guess].
pub(crate) const SUMMARY_INPUT: usize = 96 * 1024;
/// The longest summary stored, in bytes [guess].
pub(crate) const MAX_SUMMARY: usize = 8 * 1024;
/// Bytes per estimated token, until the catalog carries context sizes [guess].
const BYTES_PER_TOKEN: u64 = 4;

/// A summary request, the events the summary will cover and the deletion generation it was read
/// in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Compaction {
    pub(crate) generation: Generation,
    pub(crate) covers: (EventId, EventId),
    pub(crate) built: Built,
}

/// The summary request for `thread`, once its uncovered backlog reaches `compact_at` estimated
/// tokens. It summarizes the longest prefix of whole runs older than the newest [`RAW_RUNS`]
/// that fits [`SUMMARY_INPUT`], with the previous summary, and covers from the previous
/// summary's start through the last run it carries. `None` when the thread is being deleted or
/// gone, the backlog is under the threshold, no run is old enough, or the oldest one alone
/// doesn't fit.
///
/// # Errors
/// [`BuildError`] when the read fails or the run's origin doesn't own the thread.
pub(crate) fn compaction(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
    compact_at: u64,
) -> Result<Option<Compaction>, BuildError> {
    let (Some(source), Some(generation)) = (snapshot.source(thread)?, snapshot.generation(thread)?)
    else {
        return Ok(None);
    };
    let owns = match run.origin {
        Origin::Kumo => source.kind == ThreadKind::Main,
        Origin::Task(task) => task == thread && source.kind == ThreadKind::Thread,
    };
    if !owns {
        return Err(BuildError::Origin);
    }
    let previous = snapshot.summary(thread)?;
    let after = previous.as_ref().map(|summary| summary.last);
    let uncovered = snapshot.uncovered(thread, after)?;
    let backlog = uncovered.iter().map(|run| run.bytes).sum::<u64>() / BYTES_PER_TOKEN;
    let eligible = &uncovered[..uncovered.len().saturating_sub(RAW_RUNS)];
    let Some(newest) = eligible.last() else {
        return Ok(None);
    };
    if backlog < compact_at {
        return Ok(None);
    }
    let (said, unread) = snapshot.span(thread, after, newest.last, SUMMARY_INPUT)?;
    let mut said = said.into_iter().peekable();

    let instructions = instructions(run);
    let prior = previous.as_ref().map(|summary| InputItem::Message {
        role: Role::Developer,
        text: format!(
            "Summary so far, of events {}–{}:\n{}",
            summary.first.0, summary.last.0, summary.text
        ),
    });
    let closing = InputItem::Message {
        role: Role::Developer,
        text: "Write the new summary now.".to_owned(),
    };
    let mut total = fixed_bytes(&instructions, &[])
        + prior
            .iter()
            .chain([&closing])
            .map(item_bytes)
            .sum::<usize>();
    let mut sources = BTreeSet::from([thread]);
    let mut kept = Vec::new();
    let mut through = None;
    for uncovered in eligible {
        if unread.is_some_and(|unread| unread <= uncovered.last) {
            break;
        }
        let mut items = Vec::new();
        while let Some(next) = said.next_if(|next| next.id <= uncovered.last) {
            items.push(Item {
                id: next.id,
                run: next.run,
                body: match next.speaker {
                    Speaker::User => Body::User(next.text),
                    Speaker::Assistant => Body::Assistant(next.text),
                },
            });
        }
        let mut reached = BTreeSet::new();
        let rendered = render(snapshot, run, &items, None, &mut reached)?;
        let bytes = rendered.iter().map(item_bytes).sum::<usize>();
        if total + bytes > SUMMARY_INPUT {
            break;
        }
        total += bytes;
        sources.append(&mut reached);
        kept.extend(rendered);
        through = Some(uncovered.last);
    }
    // shortcut: one run over SUMMARY_INPUT stalls compaction behind it until a continuation
    // scheme exists; upgrade when a real thread hits it.
    let Some(through) = through else {
        return Ok(None);
    };
    let first = previous.map_or(eligible[0].first, |summary| summary.first);
    Ok(Some(Compaction {
        generation,
        covers: (first, through),
        built: Built {
            request: ModelRequest {
                choice: run.choice.clone(),
                instructions,
                input: prior.into_iter().chain(kept).chain([closing]).collect(),
                tools: Vec::new(),
            },
            sources: sources.into_iter().collect(),
        },
    }))
}

/// The model's answer as a summary: trimmed, cut to [`MAX_SUMMARY`] bytes on a character
/// boundary. `None` when nothing is left.
pub(crate) fn clean_summary(output: &str) -> Option<String> {
    let text = output.trim();
    let mut end = text.len().min(MAX_SUMMARY);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = text[..end].trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The summary prompt with the run's names.
fn instructions(run: &RunSnapshot) -> String {
    let template = SUMMARY_PROMPT
        .lines()
        .filter(|line| run.user.is_some() || !line.contains("{user}"))
        .collect::<Vec<_>>()
        .join("\n");
    fill(
        &template,
        &[
            ("{product}", DISPLAY_NAME),
            ("{assistant}", run.assistant.as_str()),
            ("{user}", run.user.as_ref().map_or("", |user| user.as_str())),
        ],
    )
}
