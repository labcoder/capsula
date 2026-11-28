//! A local task's request: the app's instructions and the task's own plain turns and answers,
//! read before anything else a build adds (summaries, notes, time, drawings, screens, tools).

use kumochi_protocol::message::{ConnectionKind, RunId, ThreadId, ThreadKind};

use super::bound::{fixed_bytes, item_bytes};
use super::build::{instructions, runs};
use super::{Body, BuildError, Built, InputItem, ModelRequest, Origin, Role, RunSnapshot};
use crate::store::choice::StoredChoice;
use crate::store::context::Snapshot;

/// Builds the next request of `run` in local task `thread` within `bound` bytes, keeping the
/// newest whole runs that fit, then ends `snapshot`.
///
/// # Errors
/// [`BuildError::Unbound`] unless the thread is a live task whose own saved route is local;
/// [`BuildError::LocalContent`] when it holds anything but plain text; `TooLarge` with no room
/// when the current run alone is over `bound`, since nothing summarizes a local task.
pub(crate) fn build_local(
    snapshot: Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
    current: RunId,
    bound: usize,
) -> Result<Built, BuildError> {
    let built = assemble(&snapshot, run, thread, current, bound)?;
    snapshot.end()?;
    Ok(built)
}

fn assemble(
    snapshot: &Snapshot<'_>,
    run: &RunSnapshot,
    thread: ThreadId,
    current: RunId,
    bound: usize,
) -> Result<Built, BuildError> {
    let source = snapshot
        .source(thread)?
        .filter(|source| source.live)
        .ok_or(BuildError::Deleting)?;
    if run.origin != Origin::Task(thread) || source.kind != ThreadKind::Thread {
        return Err(BuildError::Origin);
    }
    if !matches!(
        snapshot.stored_choice(thread)?,
        StoredChoice::Valid {
            connection: ConnectionKind::LocalChat,
            ..
        }
    ) {
        return Err(BuildError::Unbound);
    }
    let items = snapshot
        .plain_conversation(thread, bound)?
        .ok_or(BuildError::LocalContent)?;
    let instructions = instructions(run);
    let mut total = fixed_bytes(&instructions, &[]);
    let mut kept: Vec<Vec<InputItem>> = Vec::new();
    for segment in runs(items, current)?.iter().rev() {
        let mut input: Vec<InputItem> = Vec::new();
        for item in segment {
            match &item.body {
                Body::User(text) => input.push(InputItem::Message {
                    role: Role::User,
                    text: text.clone(),
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
                Body::Narration { .. }
                | Body::Call { .. }
                | Body::History { .. }
                | Body::Native { .. }
                | Body::Result { .. }
                | Body::Recall { .. } => return Err(BuildError::LocalContent),
            }
        }
        let bytes = input.iter().map(item_bytes).sum::<usize>();
        if total + bytes > bound {
            if kept.is_empty() {
                return Err(BuildError::TooLarge { room: None });
            }
            break;
        }
        total += bytes;
        kept.push(input);
    }
    Ok(Built {
        request: ModelRequest {
            choice: run.choice.clone(),
            instructions,
            input: kept.into_iter().rev().flatten().collect(),
            tools: Vec::new(),
        },
        sources: vec![thread],
    })
}
