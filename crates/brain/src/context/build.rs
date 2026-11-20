//! Builds one request from one read transaction.

use std::collections::{BTreeMap, BTreeSet};

use kumochi_protocol::constants::DISPLAY_NAME;
use kumochi_protocol::message::{ConnectionKind, EventId, RunId, ThreadId, ThreadKind};
use serde_json::{Value, json};

use super::bound::{fixed_bytes, item_bytes};
use super::{
    Body, BuildError, Built, ConnectionTag, HistoryRef, InputItem, Item, MAX_NOTES, ModelRequest,
    Origin, RAW_RUNS, REMOVED, REREAD_CAP, Role, RunSnapshot, Supplied, UNAVAILABLE, note_item,
    summary_item, tools,
};
use crate::engines::Opaque;
use crate::engines::anthropic::encode::ToolUse;
use crate::store::StoreError;
use crate::store::context::Snapshot;

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
    let mut items: Vec<Item> = snapshot.conversation(turn.thread, after, bound)?;
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
    let summary = supplied
        .summary
        .map(|summary| summary_item(summary.first, summary.last, &summary.text));
    let mut current = render(snapshot, run, current, Some(turn.local_time), &mut sources)?;
    if let Some(InputItem::Message { text, .. }) = current.iter_mut().find(|item| {
        matches!(
            item,
            InputItem::Message {
                role: Role::User,
                ..
            }
        )
    }) {
        text.push_str("\nDrawings on screen: ");
        let drawings = &supplied.drawings;
        text.push_str(if super::takes_images(run.choice.as_ref()) {
            drawings.as_str()
        } else {
            drawings.off_screen()
        });
    }
    let unsummarized =
        fixed_bytes(&instructions, &tools) + current.iter().map(item_bytes).sum::<usize>();
    let mut total = unsummarized + summary.iter().map(item_bytes).sum::<usize>();
    if total > bound {
        let room = summary
            .is_some()
            .then(|| bound.checked_sub(unsummarized))
            .flatten();
        return Err(BuildError::TooLarge { room });
    }
    // A deletion note follows the last run that began before it, never splitting one, and is
    // dropped with that run; notes older than every run stay only while no run left the window.
    let mut notes = snapshot.notes(turn.thread, after, MAX_NOTES)?;
    let whole = older.len() < RAW_RUNS;
    let mut segments = older.iter().rev().take(RAW_RUNS - 1);
    let mut kept = Vec::new();
    loop {
        let segment = segments.next();
        if segment.is_none() && !whole {
            break;
        }
        let mut reached = BTreeSet::new();
        let mut rendered = match segment {
            Some(segment) => render(snapshot, run, segment, None, &mut reached)?,
            None => Vec::new(),
        };
        let start = segment.map_or(EventId(0), |segment| segment[0].id);
        let at = notes.partition_point(|note| note.id <= start);
        rendered.extend(notes.drain(at..).map(|note| note_item(&note)));
        let bytes = rendered.iter().map(item_bytes).sum::<usize>();
        if total + bytes > bound {
            break;
        }
        total += bytes;
        sources.append(&mut reached);
        kept.push(rendered);
        if segment.is_none() {
            break;
        }
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
                Body::User(_)
                | Body::Assistant(_)
                | Body::Narration { .. }
                | Body::Native { .. }
                | Body::Result { .. }
                | Body::Recall { .. } => {}
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
///
/// A run whose stored rounds all came from this run's route, its kind and tag, and pair up (see
/// [`replayable`]) replays them natively, each round's items then its results, and of its text
/// only what came after its last round, which a `ChatGPT` round's items carry (D13-M1).
/// Local narration follows its matching native draw call; unmatched lines stay in the text.
/// Any other run replays as text only, so no call or result travels without its pair.
pub(super) fn render(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    items: &[Item],
    local_time: Option<&str>,
    sources: &mut BTreeSet<ThreadId>,
) -> Result<Vec<InputItem>, StoreError> {
    // Native items replay only on `ChatGPT` and the Anthropic key; any other route gets text.
    let kind = run
        .choice
        .as_ref()
        .map_or(ConnectionKind::ChatGpt, |choice| choice.connection);
    let native = kind != ConnectionKind::ClaudeCode && replayable(kind, run.connection, items);
    // A `ChatGPT` round's items carry its text; the key's carry only its calls, so its text stays.
    let carried = native && kind == ConnectionKind::ChatGpt;
    let narration = if carried {
        native_narration(items)
    } else {
        BTreeMap::new()
    };
    let after_rounds = items
        .iter()
        .rposition(|item| {
            matches!(
                item.body,
                Body::Native { .. } | Body::Result { .. } | Body::Recall { .. }
            )
        })
        .filter(|_| carried);
    let mut input: Vec<InputItem> = Vec::with_capacity(items.len());
    for (at, item) in items.iter().enumerate() {
        match &item.body {
            Body::Assistant(_) if after_rounds.is_some_and(|last| at < last) => {}
            Body::Narration { call, .. } if narration.contains_key(call.as_str()) => {}
            Body::User(text) => input.push(InputItem::Message {
                role: Role::User,
                text: match local_time {
                    Some(time) => format!("{text}\n\nLocal time: {time}"),
                    None => text.clone(),
                },
            }),
            Body::Assistant(text) | Body::Narration { text, .. } => match input.last_mut() {
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
            Body::Native { item, .. } => {
                if native {
                    input.push(InputItem::Native(item.clone()));
                    if !narration.is_empty()
                        && let Some(call) = called(item)
                        && let Some(text) = narration.get(call.as_str())
                    {
                        input.push(InputItem::Message {
                            role: Role::Assistant,
                            text: text.clone(),
                        });
                    }
                }
            }
            Body::Result { call, output } => {
                if native {
                    input.push(InputItem::FunctionOutput {
                        call: call.clone(),
                        output: output.clone(),
                    });
                }
            }
            Body::Recall { call, selection } => {
                if native {
                    input.push(InputItem::FunctionOutput {
                        call: call.clone(),
                        output: snapshot.render_recall(run.origin, selection, sources)?,
                    });
                }
            }
        }
    }
    Ok(input)
}

fn native_narration(items: &[Item]) -> BTreeMap<&str, String> {
    // shortcut: native replay groups each call's lines at its boundary, so it can't preserve
    // same-call text interleaving; add position evidence if a provider interleaves in practice.
    let mut lines = BTreeMap::<_, String>::new();
    for item in items {
        if let Body::Narration { text, call } = &item.body {
            lines.entry(call.as_str()).or_default().push_str(text);
        }
    }
    if !lines.is_empty() {
        let calls: BTreeSet<_> = items
            .iter()
            .filter_map(|item| match &item.body {
                Body::Native { item, .. } => called(item),
                _ => None,
            })
            .collect();
        lines.retain(|call, _| calls.contains(*call));
    }
    lines
}

/// Whether a run's stored rounds replay natively on `kind` and `tag`: at least one native item,
/// every one from that route, and each result answering a call made before it, once. The key
/// route requires typed calls unique within each group, and a complete group before more text;
/// `ChatGPT` keeps unfinished rounds as before. A foreign or unpaired row makes the whole run text.
fn replayable(kind: ConnectionKind, tag: ConnectionTag, items: &[Item]) -> bool {
    let key_route = kind == ConnectionKind::AnthropicKey;
    let mut calls = BTreeSet::new();
    let mut answered = BTreeSet::new();
    let mut natives = false;
    let mut in_results = false;
    for item in items {
        let fits = match &item.body {
            Body::Native {
                kind: made,
                connection,
                item,
            } => {
                if *made != Some(kind) || *connection != tag {
                    return false;
                }
                if key_route && in_results {
                    if answered.len() != calls.len() {
                        return false;
                    }
                    calls.clear();
                    answered.clear();
                }
                in_results = false;
                natives = true;
                if key_route {
                    ToolUse::stored(item.bytes())
                        .is_some_and(|call| calls.insert(call.id().to_owned()))
                } else {
                    called(item).is_none_or(|call| calls.insert(call))
                }
            }
            Body::Result { call, .. } | Body::Recall { call, .. } => {
                in_results = true;
                calls.contains(call.as_str()) && answered.insert(call.as_str())
            }
            _ => !key_route || answered.len() == calls.len(),
        };
        if !fits {
            return false;
        }
    }
    natives && (!key_route || answered.len() == calls.len())
}

/// The call a stored item of `kind` makes, if it makes one.
fn called(item: &Opaque) -> Option<String> {
    let item: Value = serde_json::from_slice(item.bytes()).ok()?;
    let call = item["call_id"]
        .as_str()
        .filter(|_| item["type"] == "function_call")?;
    Some(call.to_owned())
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
