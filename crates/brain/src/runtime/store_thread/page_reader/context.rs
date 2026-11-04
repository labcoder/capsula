//! A context build on the page reader: one read transaction, begun after every op accepted
//! before it, so a delete sent first is always seen.

use crate::context::build::{Turn, build};
use crate::context::compact::{compaction, rewrite};
use crate::context::{BOUND, BuildError, Built, Supplied};
use crate::coordinator::{
    BuildContext, CompactBuilt, CompactContext, ContextBuilt, JobAsk, JobBuilt, JobKind, ReadJob,
    Unbuilt,
};
use crate::store::context::Snapshot;
use crate::store::{Side, StoreError};

/// Builds `job` on `side`, or fails it when the store closed before its turn (`None`). A run's
/// first build reads its model choice into its snapshot. Failures are logged by kind only.
pub(super) fn read(
    side: Option<&Side>,
    job: BuildContext,
    #[cfg(test)] hold: impl FnOnce(),
) -> ContextBuilt {
    let BuildContext {
        key,
        thread,
        snapshot,
        first,
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
        side.read(|conn| {
            let read = Snapshot::begin(conn)?;
            #[cfg(test)]
            hold();
            if first {
                snapshot.choice = read.choice(thread)?.or(snapshot.choice.take());
            }
            let local_time = read.local_time()?;
            let turn = Turn {
                thread,
                run: key.run,
                local_time: &local_time,
            };
            let supplied = Supplied {
                summary: read.summary(thread)?,
                items: Vec::new(),
            };
            Ok(build(read, &snapshot, turn, supplied, BOUND))
        })
    } else {
        Err(StoreError::Closed)
    };
    let result = match built {
        Ok(Ok(built)) => Ok(built),
        Ok(Err(BuildError::TooLarge { room })) => Err(Unbuilt::TooLarge { room }),
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
    let read = side.map_or(Err(StoreError::Closed), |side| {
        side.read(|conn| {
            let read = Snapshot::begin(conn)?;
            #[cfg(test)]
            hold();
            let found = rewrite(&read, &snapshot, thread, room);
            read.end()?;
            Ok(found)
        })
    });
    let result = match read {
        Ok(Ok(found)) => Ok(found.map(|(found, limit)| {
            (
                JobAsk {
                    generation: found.generation,
                    built: found.built,
                    covers: Some(found.covers),
                },
                limit,
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
    #[cfg(test)] hold: impl FnOnce(),
) -> JobBuilt {
    let ReadJob {
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
    let read = side.map_or(Err(StoreError::Closed), |side| {
        side.read(|conn| {
            let read = Snapshot::begin(conn)?;
            #[cfg(test)]
            hold();
            snapshot.choice = read.choice(key.thread)?.or(snapshot.choice.take());
            let ask = match kind {
                JobKind::Title => Ok(read.first_ask(key.thread)?.map(|ask| JobAsk {
                    generation: ask.generation,
                    built: Built {
                        request: crate::jobs::title_request(&snapshot, &ask.text),
                        sources: vec![key.thread],
                    },
                    covers: None,
                })),
                JobKind::Compact => {
                    compaction(&read, &snapshot, key.thread, compact_at).map(|found| {
                        found.map(|found| JobAsk {
                            generation: found.generation,
                            built: found.built,
                            covers: Some(found.covers),
                        })
                    })
                }
            };
            read.end()?;
            Ok(ask)
        })
    });
    let result = match read {
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
