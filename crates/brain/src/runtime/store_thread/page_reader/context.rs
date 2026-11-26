//! A context build on the page reader: one read transaction, begun after every op accepted
//! before it, so a delete sent first is always seen.

use crate::config::tuning::ContextTuning;
use crate::context::build::{Turn, build};
use crate::context::compact::{compaction, rewrite};
use crate::context::{BOUND, BuildError, Built, PictureRecipient, Route, Supplied};
use crate::coordinator::{
    BuildContext, CompactBuilt, CompactContext, ContextBuilt, JobAsk, JobBuilt, JobKind, ReadJob,
    Unbuilt,
};
use crate::recall::control::READ_TIME;
use crate::store::choice::StoredChoice;
use crate::store::context::Snapshot;
use crate::store::{Side, StoreError};
use kumochi_protocol::message::ConnectionKind;
use std::time::Instant;

fn closed(control: &crate::recall::control::Control) -> StoreError {
    control
        .check()
        .err()
        .map_or(StoreError::Closed, StoreError::from)
}

pub(super) fn read_hold(
    side: Option<&Side>,
    job: crate::coordinator::HoldRead,
    #[cfg(test)] hold: impl FnOnce(),
) -> Result<crate::coordinator::HoldSnapshot, ()> {
    let crate::coordinator::HoldRead {
        control,
        mut snapshot,
    } = job;
    let result = side.map_or(Err(closed(&control)), |side| {
        side.read_until(&control, |conn, control| {
            control
                .until(Instant::now() + READ_TIME)
                .with_handler(conn, |conn, scope| {
                    scope
                        .query(conn, || {
                            let read = Snapshot::begin(conn)?;
                            #[cfg(test)]
                            hold();
                            let stored = match snapshot.thread {
                                Some(thread) => {
                                    snapshot.generation =
                                        Some(read.generation(thread)?.ok_or(StoreError::NoThread)?);
                                    read.stored_choice(thread)?
                                }
                                None => StoredChoice::Absent,
                            };
                            // A hold's run takes this snapshot whole, so its first build reads no choice.
                            snapshot.defaults.choice =
                                stored.execution(snapshot.defaults.choice.take())?;
                            snapshot.defaults.connection =
                                snapshot.arrived.replay(snapshot.defaults.choice.as_ref());
                            read.end()?;
                            Ok(())
                        })
                        .map_err(StoreError::from)
                })
                .map_err(StoreError::from)?
        })
    });
    result.map_err(|_| ())?;
    control.check().map_err(|_| ())?;
    Ok(snapshot)
}

pub(super) fn read_call(
    side: Result<&Side, crate::recall::Error>,
    call: crate::dispatch::ReadCall,
    tuning: ContextTuning,
    #[cfg(test)] hold: impl FnOnce(),
) -> crate::dispatch::ReadFinished {
    let result = side.and_then(|side| {
        side.read_until(&call.control, |conn, control| {
            control
                .until(Instant::now() + READ_TIME)
                .with_handler(conn, |conn, scope| {
                    scope.query(conn, || {
                        let read = Snapshot::begin(conn)?.with_tuning(tuning);
                        #[cfg(test)]
                        hold();
                        let result = read.recall_call(call.origin, &call.query)?;
                        read.end().map_err(crate::recall::Error::from)?;
                        Ok(result)
                    })
                })?
        })
    });
    crate::dispatch::ReadFinished {
        key: call.key,
        build: call.build,
        result,
    }
}

/// Builds `job` on `side`, or fails it when the store closed before its turn (`None`). A run's
/// first build reads its model choice into its snapshot. Failures are logged by kind only.
pub(super) fn read(
    side: Option<&Side>,
    job: BuildContext,
    tuning: ContextTuning,
    #[cfg(test)] hold: impl FnOnce(),
) -> ContextBuilt {
    let BuildContext {
        control,
        key,
        thread,
        snapshot,
        first,
        arrived,
        drawings,
        picture_models,
        key_results,
        ..
    } = job;
    let Some(mut snapshot) = snapshot else {
        tracing::error!("a build reached the store without its snapshot");
        return ContextBuilt {
            key,
            snapshot: None,
            result: Err(Unbuilt::Failed),
        };
    };
    let built = if let Some(side) = side {
        side.read_until(&control, |conn, control| {
            control
                .until(Instant::now() + READ_TIME)
                .with_handler(conn, |conn, scope| {
                    let read = Snapshot::begin(conn)?
                        .controlled(scope.clone())
                        .with_tuning(tuning);
                    #[cfg(test)]
                    hold();
                    if first {
                        snapshot.choice = read
                            .stored_choice(thread)?
                            .execution(snapshot.choice.take())?;
                        snapshot.connection = arrived.replay(snapshot.choice.as_ref());
                    }
                    let answers_chatgpt = snapshot
                        .choice
                        .as_ref()
                        .is_none_or(|choice| choice.connection == ConnectionKind::ChatGpt);
                    if answers_chatgpt && arrived.chatgpt_blocked() {
                        return Err(StoreError::RecoveryContext);
                    }
                    let local_time = read.local_time(key.run)?;
                    let picture = snapshot.choice.as_ref().and_then(|choice| {
                        let route = Route {
                            kind: choice.connection,
                            tag: snapshot.connection,
                        };
                        picture_models
                            .iter()
                            .any(|(found, models)| {
                                *found == route && models.contains(&choice.model)
                            })
                            .then(|| PictureRecipient {
                                route,
                                model: choice.model.clone(),
                            })
                    });
                    let turn = Turn {
                        thread,
                        run: key.run,
                        local_time: &local_time,
                        picture: picture.as_ref(),
                        key_results: &key_results,
                    };
                    let supplied = Supplied {
                        summary: read.summary(thread)?,
                        items: Vec::new(),
                        drawings,
                    };
                    Ok(build(read, &snapshot, turn, supplied, BOUND))
                })
                .map_err(StoreError::from)?
        })
    } else {
        Err(closed(&control))
    };
    let result = match built {
        Ok(Ok(built)) => Ok(built),
        Ok(Err(BuildError::TooLarge { room })) => Err(Unbuilt::TooLarge { room }),
        Ok(Err(BuildError::Store(StoreError::RecallBudget))) | Err(StoreError::RecallBudget) => {
            Err(Unbuilt::TooLarge { room: None })
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, run = key.run.0, "a context build failed");
            Err(Unbuilt::Failed)
        }
        Err(error) => {
            tracing::warn!(%error, run = key.run.0, "a context read failed");
            Err(Unbuilt::Failed)
        }
    };
    ContextBuilt {
        key,
        snapshot: Some(snapshot),
        result,
    }
}

/// Reads an inline compaction's request on `side`, or fails it when the store closed before its
/// turn (`None`): the thread's summary, rewritten to fit the room its run's turn leaves, in one
/// read transaction. Failures are logged by kind only.
pub(super) fn read_compact(
    side: Option<&Side>,
    job: CompactContext,
    #[cfg(test)] hold: impl FnOnce(),
) -> CompactBuilt {
    let CompactContext {
        control,
        key,
        thread,
        snapshot,
        room,
    } = job;
    let Some(snapshot) = snapshot else {
        tracing::error!("an inline compaction reached the store without its snapshot");
        return CompactBuilt {
            key,
            result: Err(Unbuilt::Failed),
        };
    };
    let read = side.map_or(Err(closed(&control)), |side| {
        side.read_until(&control, |conn, control| {
            control
                .until(Instant::now() + READ_TIME)
                .with_handler(conn, |conn, scope| {
                    let read = Snapshot::begin(conn)?.controlled(scope.clone());
                    #[cfg(test)]
                    hold();
                    let found = rewrite(&read, &snapshot, thread, room);
                    read.end()?;
                    Ok(found)
                })
                .map_err(StoreError::from)?
        })
    });
    let result = match read {
        Ok(Err(BuildError::Store(StoreError::RecallBudget))) | Err(StoreError::RecallBudget) => {
            Err(Unbuilt::TooLarge { room: None })
        }
        Ok(Ok(found)) => Ok(found.map(|(found, allowance)| {
            (
                JobAsk {
                    generation: found.generation,
                    built: found.built,
                    covers: Some(found.covers),
                },
                allowance,
            )
        })),
        Ok(Err(error)) => {
            tracing::warn!(%error, run = key.run.0, "an inline compaction's read failed");
            Err(Unbuilt::Failed)
        }
        Err(error) => {
            tracing::warn!(%error, run = key.run.0, "an inline compaction's read failed");
            Err(Unbuilt::Failed)
        }
    };
    CompactBuilt { key, result }
}

/// Reads a background job's request on `side`, or fails it when the store closed before its turn
/// (`None`), in one read transaction that first takes the thread's model choice into the job's
/// snapshot. A title job reads the task's first ask; compaction reads the thread's summary and
/// oldest uncovered runs. Failures are logged by kind only.
pub(super) fn read_job(
    side: Option<&Side>,
    job: ReadJob,
    tuning: ContextTuning,
    #[cfg(test)] hold: impl FnOnce(),
) -> JobBuilt {
    let ReadJob {
        control,
        key,
        kind,
        snapshot,
        compact_at,
        ..
    } = job;
    let Some(mut snapshot) = snapshot else {
        tracing::error!("a job's read reached the store without its snapshot");
        return JobBuilt {
            key,
            result: Err(Unbuilt::Failed),
        };
    };
    let read = side.map_or(Err(closed(&control)), |side| {
        side.read_until(&control, |conn, control| {
            control
                .until(Instant::now() + READ_TIME)
                .with_handler(conn, |conn, scope| {
                    let read = Snapshot::begin(conn)?
                        .controlled(scope.clone())
                        .with_tuning(tuning);
                    #[cfg(test)]
                    hold();
                    snapshot.choice = read
                        .stored_choice(key.thread)?
                        .execution(snapshot.choice.take())?;
                    let ask = match kind {
                        JobKind::Title => Ok(read.first_ask(key.thread)?.map(|ask| JobAsk {
                            generation: ask.generation,
                            built: Built {
                                request: crate::jobs::title_request(&snapshot, &ask.text),
                                sources: vec![key.thread],
                            },
                            covers: None,
                        })),
                        JobKind::Compact => compaction(&read, &snapshot, key.thread, compact_at)
                            .map(|found| {
                                found.map(|found| JobAsk {
                                    generation: found.generation,
                                    built: found.built,
                                    covers: Some(found.covers),
                                })
                            }),
                    };
                    read.end()?;
                    Ok(ask)
                })
                .map_err(StoreError::from)?
        })
    });
    let result = match read {
        Ok(Err(BuildError::Store(StoreError::RecallBudget))) | Err(StoreError::RecallBudget) => {
            Err(Unbuilt::TooLarge { room: None })
        }
        Ok(Ok(ask)) => Ok(ask),
        Ok(Err(error)) => {
            tracing::warn!(%error, thread = key.thread.0, ?kind, "a job's read failed");
            Err(Unbuilt::Failed)
        }
        Err(error) => {
            tracing::warn!(%error, thread = key.thread.0, ?kind, "a job's read failed");
            Err(Unbuilt::Failed)
        }
    };
    JobBuilt { key, result }
}
