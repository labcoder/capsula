//! Compaction's request: the previous summary and the oldest runs no summary covers, rendered
//! the way a request renders them, in one read transaction (plans/slice-3.md PR 17;
//! docs/development/engines.md, "Compaction").

use std::collections::BTreeSet;

use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_protocol::message::{EventId, ThreadId, ThreadKind};

use super::bound::{escaped, fixed_bytes, item_bytes};
use super::build::{fill, render};
use super::{
    Body, BuildError, Built, InputItem, Item, ModelRequest, Origin, Role, RunSnapshot, Summary,
    note_item, summary_item,
};
use crate::store::Generation;
use crate::store::context::{Snapshot, Speaker};

const SUMMARY_PROMPT: &str = include_str!("../../prompts/summary.md");
/// The uncovered backlog, in estimated tokens, at which a task compacts after a run [guess].
pub(crate) const COMPACT_AT: u64 = 24_000;
/// The default background compaction input budget, in estimated bytes.
#[cfg(test)]
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

/// The background summary request for `thread`, once its uncovered backlog reaches `compact_at`
/// estimated tokens. It takes the longest prefix of eligible whole runs that fits the input
/// estimate: JSON-escaped strings plus fixed body and item allowances, not provider wire bytes.
/// The estimate includes instructions, the previous summary and the closing message. Coverage
/// starts at the previous summary or the first included run and ends at the last included run.
/// `None` when the thread is being deleted or gone, the backlog is under the threshold, or no
/// eligible run fits.
///
/// # Errors
/// [`BuildError`] when the read fails or the run's origin doesn't own the thread.
pub(crate) fn compaction(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
    compact_at: u64,
) -> Result<Option<Compaction>, BuildError> {
    let Some(generation) = owned(snapshot, run, thread)? else {
        return Ok(None);
    };
    let previous = snapshot.summary(thread)?;
    let after = previous.as_ref().map(|summary| summary.last);
    let uncovered = snapshot.uncovered(thread, after)?;
    let backlog = uncovered.iter().map(|run| run.bytes).sum::<u64>() / BYTES_PER_TOKEN;
    let tuning = snapshot.tuning();
    let eligible = &uncovered[..uncovered.len().saturating_sub(tuning.raw_runs)];
    let Some(newest) = eligible.last() else {
        return Ok(None);
    };
    if backlog < compact_at {
        return Ok(None);
    }
    let (said, unread) = snapshot.span(thread, after, newest.last, tuning.summary_input)?;
    let mut said = said.into_iter().peekable();
    let mut notes = snapshot
        .notes(thread, after, tuning.notes)?
        .into_iter()
        .peekable();

    let instructions = instructions(run);
    let prior = previous.as_ref().map(so_far);
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
        // Deletion notes go at the run's edges, as a request places them, so the summary covers
        // every note through its last event.
        let mut reached = BTreeSet::new();
        let mut rendered: Vec<InputItem> =
            std::iter::from_fn(|| notes.next_if(|note| note.id < uncovered.first))
                .map(|note| note_item(&note))
                .collect();
        rendered.extend(render(
            snapshot,
            run,
            &items,
            None,
            None,
            &[],
            &mut reached,
        )?);
        rendered.extend(
            std::iter::from_fn(|| notes.next_if(|note| note.id <= uncovered.last))
                .map(|note| note_item(&note)),
        );
        let bytes = rendered.iter().map(item_bytes).sum::<usize>();
        if total + bytes > tuning.summary_input {
            break;
        }
        total += bytes;
        sources.append(&mut reached);
        kept.extend(rendered);
        through = Some(uncovered.last);
    }
    // shortcut: one run over the input budget stalls background compaction until a continuation
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

/// The model's answer as a summary: trimmed, cut to `max` bytes on a character boundary. `None`
/// when nothing is left.
pub(crate) fn clean_summary(output: &str, max: usize) -> Option<String> {
    let text = output.trim();
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = text[..end].trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Inline compaction's answer as a summary: cleaned to [`MAX_SUMMARY`] bytes, then cut on a
/// character boundary so its text adds at most `allowance` bytes to its item once JSON-escaped,
/// the way the bound counts it. `None` when nothing is left.
pub(crate) fn fit_summary(output: &str, allowance: usize) -> Option<String> {
    let text = clean_summary(output, MAX_SUMMARY)?;
    let mut added = 0;
    let end = text
        .char_indices()
        .find_map(|(at, c)| {
            added += escaped(c.encode_utf8(&mut [0; 4])) - 2;
            (added > allowance).then_some(at)
        })
        .unwrap_or(text.len());
    let text = text[..end].trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Inline compaction's request (B5), when a run's current turn fits without the thread's summary
/// but not with it: the summary rewritten to fit the `room` the bound leaves. It covers what the
/// summary covered. The allowance returned with it is what the new text may add to its item,
/// counted escaped, for [`fit_summary`]. The background input budget doesn't apply. `None` when
/// the thread is being deleted or gone, has no summary, or leaves no room.
///
/// # Errors
/// [`BuildError`] when the read fails or the run's origin doesn't own the thread.
pub(crate) fn rewrite(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
    room: usize,
) -> Result<Option<(Compaction, usize)>, BuildError> {
    let Some(generation) = owned(snapshot, run, thread)? else {
        return Ok(None);
    };
    let Some(previous) = snapshot.summary(thread)? else {
        return Ok(None);
    };
    let wrapper = item_bytes(&summary_item(previous.first, previous.last, ""));
    let allowance = room.saturating_sub(wrapper);
    if allowance == 0 {
        return Ok(None);
    }
    let limit = allowance.min(MAX_SUMMARY);
    let request = ModelRequest {
        choice: run.choice.clone(),
        instructions: instructions(run),
        input: vec![
            so_far(&previous),
            InputItem::Message {
                role: Role::Developer,
                text: format!("Write the new summary now, in at most {limit} bytes."),
            },
        ],
        tools: Vec::new(),
    };
    Ok(Some((
        Compaction {
            generation,
            covers: (previous.first, previous.last),
            built: Built {
                request,
                sources: vec![thread],
            },
        },
        allowance,
    )))
}

/// The thread's deletion generation, while it's live; the run's origin must own it.
fn owned(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
) -> Result<Option<Generation>, BuildError> {
    let (Some(source), Some(generation)) = (snapshot.source(thread)?, snapshot.generation(thread)?)
    else {
        return Ok(None);
    };
    let owns = match run.origin {
        Origin::Kumo => source.kind == ThreadKind::Main,
        Origin::Task(task) => task == thread && source.kind == ThreadKind::Thread,
    };
    if owns {
        Ok(Some(generation))
    } else {
        Err(BuildError::Origin)
    }
}

/// The previous summary, as a summary request carries it.
fn so_far(summary: &Summary) -> InputItem {
    InputItem::Message {
        role: Role::Developer,
        text: format!(
            "Summary so far, of events {}–{}:\n{}",
            summary.first.0, summary.last.0, summary.text
        ),
    }
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
