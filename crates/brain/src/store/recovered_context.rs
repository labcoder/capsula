//! Immutable history cutoffs for `ChatGPT` replacements.

use kumochi_protocol::message::{RunId, ThreadId};
use rusqlite::{Connection, OptionalExtension as _};

use super::StoreError;

#[cfg(test)]
impl super::Store {
    /// Records `tag`'s cutoff at the history as it stands, for fixtures.
    pub(crate) fn cut_history(&mut self, tag: u64) -> Boundary {
        assert_ne!(tag, 0, "a cutoff's tag");
        self.reconnect().expect("reconnect");
        self.write_cutoff(tag).expect("a fresh tag's cutoff")
    }

    fn write_cutoff(&mut self, tag: u64) -> Result<Boundary, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        super::schema::migrate_recovery_context(&tx)?;
        let (last_thread, last_run) = cutoff_sequences(&tx)?;
        let inserted = tx.execute(
            "INSERT INTO chatgpt_recovery_context (tag,last_thread,last_run)
             VALUES (?1,?2,?3)
             ON CONFLICT(tag) DO NOTHING",
            rusqlite::params![tag.to_be_bytes(), last_thread, last_run],
        )?;
        if inserted != 1 {
            return Err(StoreError::RecoveryContext);
        }
        tx.commit()?;
        Ok(Boundary {
            tag,
            last_thread,
            last_run,
        })
    }
}

#[cfg(test)]
fn cutoff_sequences(conn: &Connection) -> Result<(i64, i64), StoreError> {
    let (thread, run): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT (SELECT max(id) FROM threads), (SELECT max(id) FROM runs)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let sequence = |name| {
        conn.query_one(
            "SELECT seq FROM sqlite_sequence WHERE name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
    };
    let last_thread = sequence("threads")?
        .filter(|&seq| seq >= thread.unwrap_or(0).max(1))
        .ok_or(StoreError::RecoveryContext)?;
    let last_run = match sequence("runs")? {
        None if run.is_none() => 0,
        Some(seq) if seq >= run.unwrap_or(0).max(0) => seq,
        _ => return Err(StoreError::RecoveryContext),
    };
    Ok((last_thread, last_run))
}

/// A recovered connection's history cutoff: its tag and the last thread and run before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    tag: u64,
    last_thread: i64,
    last_run: i64,
}

impl Boundary {
    pub(super) fn read(conn: &Connection, tag: u64) -> Result<Option<Self>, StoreError> {
        if tag == 0 {
            return Ok(None);
        }
        let found: Option<(i64, i64)> = conn
            .query_one(
                "SELECT last_thread,last_run FROM chatgpt_recovery_context WHERE tag=?1",
                [tag.to_be_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        found
            .map(|(last_thread, last_run)| {
                if last_thread <= 0 || last_run < 0 {
                    return Err(StoreError::RecoveryContext);
                }
                Ok(Self {
                    tag,
                    last_thread,
                    last_run,
                })
            })
            .transpose()
    }

    /// A boundary no store wrote, for fixtures that need one that differs from the row.
    #[cfg(test)]
    pub(crate) fn fixture(tag: u64, last_thread: i64, last_run: i64) -> Self {
        Self {
            tag,
            last_thread,
            last_run,
        }
    }

    pub(crate) fn tag(self) -> u64 {
        self.tag
    }

    pub(crate) fn bounds(self) -> (i64, i64) {
        (self.last_thread, self.last_run)
    }

    pub(crate) fn permits_thread(self, thread: ThreadId) -> bool {
        i64::try_from(thread.0).is_ok_and(|id| id > self.last_thread)
    }

    pub(crate) fn permits_run(self, run: RunId) -> bool {
        i64::try_from(run.0).is_ok_and(|id| id > self.last_run)
    }
}
