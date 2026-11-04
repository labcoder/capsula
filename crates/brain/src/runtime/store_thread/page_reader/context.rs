//! A context build on the page reader: one read transaction, begun after every op accepted
//! before it, so a delete sent first is always seen.

use crate::context::build::{Turn, build};
use crate::context::{BOUND, BuildError, Supplied};
use crate::coordinator::{BuildContext, ContextBuilt, Unbuilt};
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
            Ok(build(read, &snapshot, turn, Supplied::default(), BOUND))
        })
    } else {
        Err(StoreError::Closed)
    };
    let result = match built {
        Ok(Ok(built)) => Ok(built),
        Ok(Err(BuildError::TooLarge)) => Err(Unbuilt::TooLarge),
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
