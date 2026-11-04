//! Builds one request from one read transaction.

use std::collections::BTreeSet;

use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_protocol::message::{RunId, ThreadId, ThreadKind};
use serde_json::{Value, json};

use super::bound::{fixed_bytes, item_bytes};
use super::{
    Body, BuildError, Built, HistoryRef, InputItem, Item, ModelRequest, Origin, RAW_RUNS, REMOVED,
    REREAD_CAP, Role, RunSnapshot, Supplied, UNAVAILABLE, tools,
};
use crate::store::StoreError;
use crate::store::context::{Snapshot, Speaker};

const INSTRUCTIONS: &str = include_str!("../../prompts/instructions.md");
const KUMO_ROLE: &str = include_str!("../../prompts/kumo.md");
const TASK_ROLE: &str = include_str!("../../prompts/task.md");

/// The round being built.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Turn<'a> {
    pub(crate) thread: ThreadId,
    pub(crate) run: RunId,
    /// The user's local time, already formatted.
    pub(crate) local_time: &'a str,
}

/// Builds the next round of `turn` within `bound` bytes, then ends `snapshot`, so the request
/// and its sources come from one transaction that's over before the request is returned.
///
/// # Errors
/// [`BuildError`] says why nothing may be sent.
pub(crate) fn build(
    snapshot: Snapshot<'_>,
    run: &RunSnapshot,
    turn: Turn<'_>,
    supplied: Supplied,
    bound: usize,
) -> Result<Built, BuildError> {
    let built = assemble(&snapshot, run, turn, supplied, bound)?;
    snapshot.end()?;
    Ok(built)
}

fn assemble(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    turn: Turn<'_>,
    supplied: Supplied,
    bound: usize,
) -> Result<Built, BuildError> {
    let thread = snapshot
        .source(turn.thread)?
        .filter(|thread| thread.live)
        .ok_or(BuildError::Deleting)?;
    let owns = match run.origin {
        Origin::Kumo => thread.kind == ThreadKind::Main,
        Origin::Task(task) => task == turn.thread && thread.kind == ThreadKind::Thread,
    };
    if !owns {
        return Err(BuildError::Origin);
    }
    let after = supplied.summary.as_ref().map(|summary| summary.last);
    let mut items: Vec<Item> = snapshot
        .conversation(turn.thread, after, bound)?
        .into_iter()
        .map(|said| Item {
            id: said.id,
            run: said.run,
            body: match said.speaker {
                Speaker::User => Body::User(said.text),
                Speaker::Assistant => Body::Assistant(said.text),
            },
        })
        .collect();
    items.extend(
        supplied
            .items
            .into_iter()
            .filter(|item| after.is_none_or(|after| item.id > after)),
    );
    items.sort_by_key(|item| item.id);
    let runs = runs(items, turn.run)?;
    let (current, older) = runs.split_last().ok_or(BuildError::NoTurn)?;

    let instructions = instructions(run);
    let tools = tools(run.origin);
    let mut sources = BTreeSet::from([turn.thread]);
    let summary = supplied.summary.map(|summary| InputItem::Message {
        role: Role::Developer,
        text: format!(
            "Summary of events {}–{}:\n{}",
            summary.first.0, summary.last.0, summary.text
        ),
    });
    let current = render(snapshot, run, current, Some(turn.local_time), &mut sources)?;
    let mut total = fixed_bytes(&instructions, &tools)
        + summary
            .iter()
            .chain(&current)
            .map(item_bytes)
            .sum::<usize>();
    if total > bound {
        return Err(BuildError::TooLarge);
    }
    let mut kept = Vec::new();
    for older in older.iter().rev().take(RAW_RUNS - 1) {
        let mut reached = BTreeSet::new();
        let rendered = render(snapshot, run, older, None, &mut reached)?;
        let bytes = rendered.iter().map(item_bytes).sum::<usize>();
        if total + bytes > bound {
            break;
        }
        total += bytes;
        sources.append(&mut reached);
        kept.push(rendered);
    }
    let input = summary
        .into_iter()
        .chain(kept.into_iter().rev().flatten())
        .chain(current)
        .collect();
    Ok(Built {
        request: ModelRequest {
            choice: run.choice.clone(),
            instructions,
            input,
            tools,
        },
        sources: sources.into_iter().collect(),
    })
}

/// Groups ordered items into whole runs, the current one last. Each call is followed by its
/// result within its run, and no item repeats.
fn runs(items: Vec<Item>, current: RunId) -> Result<Vec<Vec<Item>>, BuildError> {
    if items.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(BuildError::InvalidCut);
    }
    let mut runs: Vec<Vec<Item>> = Vec::new();
    for item in items {
        match runs.last_mut() {
            Some(open) if open[0].run == item.run => open.push(item),
            _ => {
                if runs.iter().any(|done| done[0].run == item.run) {
                    return Err(BuildError::InvalidCut);
                }
                runs.push(vec![item]);
            }
        }
    }
    for run in &runs {
        let mut open = None;
        for item in run {
            match &item.body {
                Body::Call { call, .. } if open.is_none() => open = Some(call),
                Body::History { call, .. } if open == Some(call) => open = None,
                Body::Call { .. } | Body::History { .. } => return Err(BuildError::InvalidCut),
                Body::User(_) | Body::Assistant(_) | Body::Reasoning { .. } => {}
            }
        }
        if open.is_some() {
            return Err(BuildError::InvalidCut);
        }
    }
    let Some(last) = runs.last() else {
        return Err(BuildError::NoTurn);
    };
    if last[0].run != current {
        return Err(if runs.iter().any(|run| run[0].run == current) {
            BuildError::InvalidCut
        } else {
            BuildError::NoTurn
        });
    }
    if !last.iter().any(|item| matches!(item.body, Body::User(_))) {
        return Err(BuildError::NoTurn);
    }
    Ok(runs)
}

/// One run's input items. Its history results are re-read, adding each task they show text
/// from to `sources`; `local_time` is added to the current run's turn.
fn render(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    items: &[Item],
    local_time: Option<&str>,
    sources: &mut BTreeSet<ThreadId>,
) -> Result<Vec<InputItem>, StoreError> {
    let mut input: Vec<InputItem> = Vec::with_capacity(items.len());
    for item in items {
        match &item.body {
            Body::User(text) => input.push(InputItem::Message {
                role: Role::User,
                text: match local_time {
                    Some(time) => format!("{text}\n\nLocal time: {time}"),
                    None => text.clone(),
                },
            }),
            Body::Assistant(text) => match input.last_mut() {
                Some(InputItem::Message {
                    role: Role::Assistant,
                    text: open,
                }) => open.push_str(text),
                _ => input.push(InputItem::Message {
                    role: Role::Assistant,
                    text: text.clone(),
                }),
            },
            Body::Call {
                call,
                name,
                arguments,
            } => input.push(InputItem::FunctionCall {
                call: call.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            Body::History { call, refs } => {
                let quoted = refs
                    .iter()
                    .map(|reference| quote(snapshot, run.origin, *reference, sources))
                    .collect::<Result<Vec<_>, _>>()?;
                input.push(InputItem::FunctionOutput {
                    call: call.clone(),
                    output: json!({ "quoted_history": quoted }).to_string(),
                });
            }
            Body::Reasoning { connection, item } => {
                if *connection == run.connection {
                    input.push(InputItem::Reasoning(item.clone()));
                }
            }
        }
    }
    Ok(input)
}

/// Re-reads one reference as quoted data, only where `origin` may reach. A task that's gone or
/// being deleted renders as removed; a store failure stays a failure.
fn quote(
    snapshot: &Snapshot<'_>,
    origin: Origin,
    reference: HistoryRef,
    sources: &mut BTreeSet<ThreadId>,
) -> Result<Value, StoreError> {
    let task = reference.thread.0;
    if matches!(origin, Origin::Task(own) if own != reference.thread) {
        return Ok(json!({ "task": task, "note": UNAVAILABLE }));
    }
    let Some(source) = snapshot
        .source(reference.thread)?
        .filter(|source| source.live)
    else {
        return Ok(json!({ "task": task, "note": REMOVED }));
    };
    if origin == Origin::Kumo && source.kind != ThreadKind::Thread {
        return Ok(json!({ "task": task, "note": UNAVAILABLE }));
    }
    // Its title is shown even for an empty range, so it's a source either way.
    sources.insert(reference.thread);
    let excerpt = snapshot.excerpt(
        reference.thread,
        reference.first,
        reference.last,
        REREAD_CAP,
    )?;
    Ok(json!({
        "task": task,
        "title": source.title,
        "events": excerpt.shown.map(|(first, last)| [first.0, last.0]),
        "text": excerpt.text,
        "truncated": excerpt.cut || excerpt.more.is_some(),
        "continue_from": excerpt.more.map(|id| id.0),
    }))
}

/// The instructions for `run`: its names and role, read once when the run started.
fn instructions(run: &RunSnapshot) -> String {
    let role = match run.origin {
        Origin::Kumo => KUMO_ROLE,
        Origin::Task(_) => TASK_ROLE,
    };
    let template = INSTRUCTIONS
        .lines()
        .filter(|line| run.user.is_some() || !line.contains("{user}"))
        .collect::<Vec<_>>()
        .join("\n");
    let role = role.lines().collect::<Vec<_>>().join("\n");
    fill(
        &template,
        &[
            ("{product}", DISPLAY_NAME),
            ("{assistant}", run.assistant.as_str()),
            ("{user}", run.user.as_ref().map_or("", |user| user.as_str())),
            ("{role}", role.trim_end()),
        ],
    )
}

/// Replaces each `{key}` in `template` in one pass, so a value is never read as a key.
pub(crate) fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some((key, value)) = values.iter().find(|(key, _)| tail.starts_with(key)) {
            out.push_str(value);
            rest = &tail[key.len()..];
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}
