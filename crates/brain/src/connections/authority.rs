//! The credential authority (OSC-233): a small SQLite file that alone says which write-once
//! generation of each credential is current and which are removed. Credential Manager keeps the
//! bytes, and nothing it returns changes what this file says, so a stale, resurrected or missing
//! name can hide a credential but never authorize one.

pub(crate) mod io;
#[cfg(test)]
mod tests;

use std::fmt;

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, TransactionBehavior, params};

use super::marker::SignOutMarker;
use crate::platform::SessionLock;
use crate::platform::credential::CredentialStoreError;
use io::Verified;

/// The authority's file in the data folder; not the history store, so no purge touches it. Only
/// tests name it: production reaches it through the held lock's `authority_path`.
#[cfg(test)]
pub(crate) const FILE: &str = kumochi_protocol::constants::AUTHORITY_FILE;
/// The newest schema this build knows; a newer file is refused, never downgraded.
const VERSION: i64 = 5;
/// The removed repair schema's number; skipped, so a file it wrote is still refused as newer.
const REPAIR_VERSION: i64 = 4;
/// The sign-out marker's JSON bound, as the vault had it.
const MARKER_BYTES: usize = 200;

/// Version 3. `bytes` is bounded by `MAX_RECORD_BYTES`; Swept rows keep only their identity.
const SCHEMA: &str = "
CREATE TABLE \"heads\" (
    kind TEXT PRIMARY KEY CHECK (kind IN ('host', 'chatgpt', 'revoke', 'anthropic', 'github')),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;
INSERT INTO heads (kind, revision) VALUES ('host', 0), ('chatgpt', 0), ('revoke', 0), ('anthropic', 0), ('github', 0);
CREATE TABLE control (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    lifetime INTEGER NOT NULL CHECK (lifetime >= 0),
    marker TEXT NOT NULL,
    legacy_deleted_in INTEGER,
    legacy_done INTEGER NOT NULL DEFAULT 0 CHECK (legacy_done IN (0, 1))
) STRICT;
";

const GENERATIONS: &str = "
CREATE TABLE generations (
    generation BLOB NOT NULL UNIQUE CHECK (length(generation) = 16),
    kind TEXT NOT NULL REFERENCES heads (kind),
    state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'removed', 'swept')),
    chunks INTEGER CHECK (chunks BETWEEN 1 AND 64),
    bytes INTEGER CHECK (bytes BETWEEN 0 AND 49152),
    deleted_in INTEGER,
    CHECK ((state = 'swept') = (chunks IS NULL)),
    CHECK ((chunks IS NULL) = (bytes IS NULL))
) STRICT;
CREATE UNIQUE INDEX one_active ON generations (kind) WHERE state = 'active';
CREATE UNIQUE INDEX one_pending ON generations (kind) WHERE state = 'pending';
CREATE INDEX removed ON generations (state) WHERE state = 'removed';
";
/// Version 3 to 5: which store holds each generation's chunks. Every earlier row is the brain's
/// own; only an Anthropic paste candidate, never an active one, may be in the shell's staging store.
const STORE: &str = "
ALTER TABLE generations ADD COLUMN store TEXT NOT NULL DEFAULT 'private' CHECK (
    store = 'private' OR (store = 'staging' AND kind = 'anthropic' AND state <> 'active')
);
";
/// Version 1 to 2: the legacy cleanup's progress.
const LEGACY: &str = "
ALTER TABLE control ADD COLUMN legacy_deleted_in INTEGER;
ALTER TABLE control ADD COLUMN legacy_done INTEGER NOT NULL DEFAULT 0 CHECK (legacy_done IN (0, 1));
PRAGMA user_version = 2;
";

const GITHUB: &str = "
CREATE TABLE heads_v3 (
    kind TEXT PRIMARY KEY CHECK (kind IN ('host', 'chatgpt', 'revoke', 'anthropic', 'github')),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;
INSERT INTO heads_v3 SELECT * FROM heads;
INSERT OR IGNORE INTO heads_v3 (kind, revision) VALUES ('github', 0);
DROP TABLE heads;
ALTER TABLE heads_v3 RENAME TO heads;
";

/// Why the authority refused; no name, generation or secret is carried.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthorityError {
    /// SQLite refused or failed an operation.
    #[error("credential authority is unavailable")]
    Unavailable(#[from] rusqlite::Error),
    /// A row is missing or doesn't hold a valid value.
    #[error("credential authority is unreadable")]
    Corrupt,
    /// The file was written by a newer build.
    #[error("credential authority is from a newer build")]
    NewerSchema,
    /// The head moved, or the candidate is no longer pending, since the operation began.
    #[error("credential authority changed since this operation began")]
    Conflict,
    /// The current generation's names didn't all read back whole.
    #[error("credential is not settled")]
    Unsettled,
    /// An expected active chunk was absent on this read.
    #[error("an active credential chunk is missing")]
    MissingChunk,
    /// An active chunk did not match the committed layout.
    #[error("an active credential chunk has the wrong size")]
    WrongChunkLength,
    /// A value exceeds its bound.
    #[error("credential exceeds its bounds")]
    Bounds,
    /// Credential Manager refused an operation.
    #[error("credential operation failed")]
    Store(#[from] CredentialStoreError),
    /// No randomness for a new generation.
    #[error("credential randomness is unavailable")]
    Random(#[from] getrandom::Error),
}

/// One credential, with its own head; the label is its name segment and its SQL key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Host,
    Account,
    Revocations,
    AnthropicKey,
    GitHub,
}

impl Kind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Account => "chatgpt",
            Self::Revocations => "revoke",
            Self::AnthropicKey => "anthropic",
            Self::GitHub => "github",
        }
    }

    fn parse(label: &str) -> Result<Self, AuthorityError> {
        [
            Self::Host,
            Self::Account,
            Self::Revocations,
            Self::AnthropicKey,
            Self::GitHub,
        ]
        .into_iter()
        .find(|kind| kind.label() == label)
        .ok_or(AuthorityError::Corrupt)
    }
}

/// A fresh random identity, never reused, that names one write-once set of chunks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation([u8; 16]);

impl Generation {
    fn fresh() -> Result<Self, AuthorityError> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    fn from_sql(bytes: &[u8]) -> Result<Self, AuthorityError> {
        Ok(Self(bytes.try_into().map_err(|_| AuthorityError::Corrupt)?))
    }

    /// The generation's name segment: 32 lowercase hex digits.
    pub(crate) fn hex(self) -> String {
        kumochi_protocol::constants::generation_hex(&self.0)
    }

    pub(crate) fn stage_id(self) -> kumochi_protocol::message::KeyStageId {
        kumochi_protocol::message::KeyStageId(self.0)
    }

    pub(crate) fn from_stage_id(id: kumochi_protocol::message::KeyStageId) -> Self {
        Self(id.0)
    }
}

impl fmt::Debug for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Generation(***)")
    }
}

/// A value's chunk count and total length, derived from the length alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    chunks: usize,
    bytes: usize,
}

impl fmt::Debug for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Layout(***)")
    }
}

impl Layout {
    /// # Errors
    /// Refuses a value over the record bound.
    pub(crate) fn for_len(bytes: usize) -> Result<Self, AuthorityError> {
        let layout = kumochi_protocol::constants::CredentialLayout::new(bytes)
            .ok_or(AuthorityError::Bounds)?;
        Ok(Self {
            chunks: layout.chunks(),
            bytes,
        })
    }

    pub(crate) fn chunks(self) -> usize {
        self.chunks
    }

    pub(crate) fn bytes(self) -> usize {
        self.bytes
    }

    /// The exact length chunk `index` holds: full chunks, then whatever completes the value.
    pub(crate) fn chunk_len(self, index: usize) -> usize {
        kumochi_protocol::constants::CredentialLayout::new(self.bytes)
            .expect("an authority layout is bounded")
            .chunk_len(index)
    }

    fn from_sql(chunks: Option<i64>, bytes: Option<i64>) -> Result<Self, AuthorityError> {
        let (Some(chunks), Some(bytes)) = (chunks, bytes) else {
            return Err(AuthorityError::Corrupt);
        };
        let layout = Self::for_len(usize::try_from(bytes).map_err(|_| AuthorityError::Corrupt)?)
            .map_err(|_| AuthorityError::Corrupt)?;
        if usize::try_from(chunks).ok() != Some(layout.chunks) {
            return Err(AuthorityError::Corrupt);
        }
        Ok(layout)
    }

    fn sql(self) -> Result<(i64, i64), AuthorityError> {
        let convert = |value: usize| i64::try_from(value).map_err(|_| AuthorityError::Bounds);
        Ok((convert(self.chunks)?, convert(self.bytes)?))
    }
}

/// The one generation of a kind that may be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveRef {
    pub(crate) kind: Kind,
    pub(crate) generation: Generation,
    pub(crate) layout: Layout,
}

/// A kind's head: its revision, which every change bumps, and its Active generation, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Current {
    pub(crate) revision: u64,
    pub(crate) active: Option<ActiveRef>,
}

/// Which store holds a generation's chunks, recorded when the generation is minted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Store {
    /// The brain's own store: every saved credential and its candidates.
    Private,
    /// The store the shell writes a pasted Anthropic key into; never active.
    Staging,
}

impl Store {
    const fn label(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Staging => "staging",
        }
    }

    /// A staging row must be an Anthropic paste of a valid key length, whatever the file says.
    fn parse(label: &str, kind: Kind, layout: Layout) -> Result<Self, AuthorityError> {
        match label {
            "private" => Ok(Self::Private),
            "staging"
                if kind == Kind::AnthropicKey
                    && kumochi_protocol::message::ANTHROPIC_KEY_BYTES.contains(&layout.bytes) =>
            {
                Ok(Self::Staging)
            }
            _ => Err(AuthorityError::Corrupt),
        }
    }
}

/// A Pending generation's one chance to be written; not Clone, so it can't be written twice.
#[derive(Debug)]
#[must_use = "an unwritten plan stays Pending until the next open retires it"]
pub(crate) struct WritePlan {
    kind: Kind,
    generation: Generation,
    layout: Layout,
    base: u64,
    store: Store,
}

impl WritePlan {
    pub(crate) fn store(&self) -> Store {
        self.store
    }

    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }

    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    pub(crate) fn base(&self) -> u64 {
        self.base
    }
}

/// What a sweep of a retired generation's names found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observed {
    /// Every name read absent after its delete.
    Absent,
    /// A delete failed or a name still read present.
    Remains,
}

/// A Removed generation whose names a sweep still owes; only [`Authority::retired`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retired {
    kind: Kind,
    generation: Generation,
    layout: Layout,
    store: Store,
}

impl Retired {
    pub(crate) fn store(&self) -> Store {
        self.store
    }
}

/// What an open does when the authority file is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Missing {
    /// Create it: a profile's first start, and fixtures.
    Create,
    /// Fail, so a lost authority never comes back as an empty one.
    Refuse,
}

/// The open authority, held by the credential worker under its session lock.
pub(crate) struct Authority {
    conn: Connection,
    /// This open's count; a sweep in a later one is what turns Removed into Swept.
    lifetime: i64,
}

impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Authority")
    }
}

impl Authority {
    /// Opens the file, creating it only when `missing` allows, then retires every Pending
    /// generation: a candidate from an earlier process is never activated. The file is the one
    /// beside `session`'s lock, which proves the caller owns the account session. A present file
    /// still at version 0 gets its schema.
    ///
    /// # Errors
    /// Fails on a SQLite failure, a missing file it may not create, a file from a newer build, or
    /// missing or invalid rows.
    pub(crate) fn open(session: &SessionLock, missing: Missing) -> Result<Self, AuthorityError> {
        let create = match missing {
            Missing::Create => OpenFlags::SQLITE_OPEN_CREATE,
            Missing::Refuse => OpenFlags::empty(),
        };
        let mut conn = Connection::open_with_flags(
            session.authority_path(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | create | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(AuthorityError::Corrupt);
        }
        conn.execute_batch(
            "PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if matches!(version, 1 | 2) {
            conn.pragma_update(None, "foreign_keys", false)?;
        }
        let opening = (|| -> Result<i64, AuthorityError> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            match version {
                0 => {
                    tx.execute_batch(SCHEMA)?;
                    tx.execute_batch(GENERATIONS)?;
                    tx.execute_batch(STORE)?;
                    tx.execute(
                        "INSERT INTO control (id, lifetime, marker) VALUES (1, 0, ?1)",
                        [marker_json(SignOutMarker::default())?],
                    )?;
                }
                1 => {
                    tx.execute_batch(LEGACY)?;
                    tx.execute_batch(GITHUB)?;
                    tx.execute_batch(STORE)?;
                }
                2 => {
                    tx.execute_batch(GITHUB)?;
                    tx.execute_batch(STORE)?;
                }
                3 => tx.execute_batch(STORE)?,
                VERSION => {}
                REPAIR_VERSION => return Err(AuthorityError::NewerSchema),
                newer if newer > VERSION => return Err(AuthorityError::NewerSchema),
                _ => return Err(AuthorityError::Corrupt),
            }
            if version < VERSION {
                let violations: i64 =
                    tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                        row.get(0)
                    })?;
                if violations != 0 {
                    return Err(AuthorityError::Corrupt);
                }
                tx.pragma_update(None, "user_version", VERSION)?;
            }
            let lifetime = tx
                .query_row(
                    "UPDATE control SET lifetime = lifetime + 1 WHERE id = 1 RETURNING lifetime",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(AuthorityError::Corrupt)?;
            tx.execute(
                "UPDATE generations SET state = 'removed' WHERE state = 'pending'",
                [],
            )?;
            tx.commit()?;
            Ok(lifetime)
        })();
        conn.pragma_update(None, "foreign_keys", true)?;
        let lifetime = opening?;
        Ok(Self { conn, lifetime })
    }

    /// Whether `ChatGPT`'s account can only be an ordinary one: the authority is at the ordinary
    /// schema. A connection still inside a transaction answers nothing.
    pub(in crate::connections) fn ordinary_account(&self) -> Result<bool, AuthorityError> {
        if !self.conn.is_autocommit() {
            return Err(AuthorityError::Conflict);
        }
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(version == VERSION)
    }

    /// # Errors
    /// Fails on a SQLite failure or a missing or invalid row.
    pub(crate) fn current(&self, kind: Kind) -> Result<Current, AuthorityError> {
        current(&self.conn, kind)
    }

    pub(super) fn observe(
        &self,
        kind: Kind,
    ) -> Result<(u64, Result<Option<ActiveRef>, AuthorityError>), AuthorityError> {
        observe(&self.conn, kind)
    }

    /// Commits a fresh Pending generation for a value of `len` bytes, retiring any older
    /// candidate of the kind. The committed head is unchanged until [`Self::activate`].
    ///
    /// # Errors
    /// Conflict when the head's revision isn't `expected`; Bounds for an oversized value.
    pub(crate) fn begin(
        &mut self,
        kind: Kind,
        expected: u64,
        len: usize,
    ) -> Result<WritePlan, AuthorityError> {
        self.begin_in(kind, expected, len, Store::Private)
    }

    /// `Self::begin` for a pasted Anthropic key, whose chunks the shell writes into the staging
    /// store. The plan is only ever read, never activated.
    ///
    /// # Errors
    /// Bounds for a length outside a key's; otherwise as `Self::begin`.
    pub(crate) fn begin_paste(
        &mut self,
        expected: u64,
        len: usize,
    ) -> Result<WritePlan, AuthorityError> {
        if !kumochi_protocol::message::ANTHROPIC_KEY_BYTES.contains(&len) {
            return Err(AuthorityError::Bounds);
        }
        self.begin_in(Kind::AnthropicKey, expected, len, Store::Staging)
    }

    fn begin_in(
        &mut self,
        kind: Kind,
        expected: u64,
        len: usize,
        store: Store,
    ) -> Result<WritePlan, AuthorityError> {
        let layout = Layout::for_len(len)?;
        let (chunks, bytes) = layout.sql()?;
        let generation = Generation::fresh()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if current(&tx, kind)?.revision != expected {
            return Err(AuthorityError::Conflict);
        }
        tx.execute(
            "UPDATE generations SET state = 'removed' WHERE kind = ?1 AND state = 'pending'",
            [kind.label()],
        )?;
        tx.execute(
            "INSERT INTO generations (generation, kind, state, chunks, bytes, store)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5)",
            params![
                generation.0.as_slice(),
                kind.label(),
                chunks,
                bytes,
                store.label()
            ],
        )?;
        tx.commit()?;
        Ok(WritePlan {
            kind,
            generation,
            layout,
            base: expected,
            store,
        })
    }

    /// Makes a verified candidate the kind's only Active generation and retires its predecessor,
    /// with the marker, if given, in the same transaction.
    ///
    /// # Errors
    /// Conflict when the head moved or the candidate was retired since `begin`.
    pub(crate) fn activate(
        &mut self,
        verified: Verified,
        marker: Option<SignOutMarker>,
    ) -> Result<(ActiveRef, u64), AuthorityError> {
        let plan = verified.into_plan();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = activate_plan(&tx, &plan)?;
        if let Some(marker) = marker {
            write_marker(&tx, marker)?;
        }
        let revision = head_revision(&tx, active.kind)?;
        tx.commit()?;
        Ok((active, revision))
    }

    /// Removes every Active and Pending generation of these kinds, for good, with the marker, if
    /// given, in the same transaction. Nothing is read first, so an unsettled store never blocks it.
    ///
    /// # Errors
    /// Fails on a SQLite failure or a missing head.
    pub(crate) fn remove(
        &mut self,
        kinds: &[Kind],
        marker: Option<SignOutMarker>,
        expected: Option<(Kind, u64)>,
    ) -> Result<(), AuthorityError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((kind, revision)) = expected
            && current(&tx, kind)?.revision != revision
        {
            return Err(AuthorityError::Conflict);
        }
        for kind in kinds {
            tx.execute(
                "UPDATE generations SET state = 'removed'
                 WHERE kind = ?1 AND state IN ('active', 'pending')",
                [kind.label()],
            )?;
            bump(&tx, *kind)?;
        }
        if let Some(marker) = marker {
            write_marker(&tx, marker)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The sign-out marker; a malformed one fails closed, and a missing row is never the default.
    ///
    /// # Errors
    /// Fails on a SQLite failure or a missing control row.
    pub(crate) fn marker(&self) -> Result<SignOutMarker, AuthorityError> {
        read_marker(&self.conn)
    }

    /// # Errors
    /// Fails on a SQLite failure, a missing control row or an oversized marker.
    pub(crate) fn set_marker(&mut self, marker: SignOutMarker) -> Result<(), AuthorityError> {
        write_marker(&self.conn, marker)
    }

    /// Whether the names earlier builds wrote still need deleting.
    ///
    /// # Errors
    /// Fails on a SQLite failure or a missing control row.
    pub(crate) fn legacy_owed(&self) -> Result<bool, AuthorityError> {
        let done: i64 = self
            .conn
            .query_row("SELECT legacy_done FROM control WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or(AuthorityError::Corrupt)?;
        Ok(done == 0)
    }

    /// Records this open's legacy pass and how many of its deletes were refused. A clean pass
    /// in any open after the first pass's open ends the cleanup, as a clean sweep in a later
    /// open ends a generation's; a second pass in the same open never does.
    ///
    /// # Errors
    /// Fails on a SQLite failure.
    pub(crate) fn legacy_deleted(&mut self, refused: usize) -> Result<(), AuthorityError> {
        self.conn.execute(
            "UPDATE control SET legacy_done = (?1 = 0 AND legacy_deleted_in IS NOT NULL
             AND legacy_deleted_in < ?2), legacy_deleted_in = ?2 WHERE id = 1",
            params![
                i64::try_from(refused).map_err(|_| AuthorityError::Bounds)?,
                self.lifetime
            ],
        )?;
        Ok(())
    }

    /// At most `limit` Removed generations this open hasn't swept yet, so a pass is bounded
    /// however many were ever made and the next pass reaches the ones after. One that fails or
    /// comes back waits for the next open. Never-attempted ones come first, then the longest
    /// waiting, so failures that never clear can't hold back the rest across opens.
    ///
    /// # Errors
    /// Fails on a SQLite failure or an invalid row.
    pub(crate) fn retired(&self, limit: usize) -> Result<Vec<Retired>, AuthorityError> {
        let limit = i64::try_from(limit).map_err(|_| AuthorityError::Bounds)?;
        let mut statement = self.conn.prepare(
            "SELECT generation, kind, chunks, bytes, store FROM generations
             WHERE state = 'removed' AND (deleted_in IS NULL OR deleted_in < ?1)
             ORDER BY deleted_in IS NOT NULL, deleted_in, rowid LIMIT ?2",
        )?;
        let rows = statement.query_map(params![self.lifetime, limit], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (generation, kind, chunks, bytes, store) = row?;
            let kind = Kind::parse(&kind)?;
            let layout = Layout::from_sql(chunks, bytes)?;
            Ok(Retired {
                kind,
                generation: Generation::from_sql(&generation)?,
                layout,
                store: Store::parse(&store, kind, layout)?,
            })
        })
        .collect()
    }

    pub(crate) fn retired_kind(&self, kind: Kind) -> Result<Vec<Retired>, AuthorityError> {
        let mut statement = self.conn.prepare(
            "SELECT generation, chunks, bytes, store FROM generations
             WHERE state = 'removed' AND kind = ?1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([kind.label()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (generation, chunks, bytes, store) = row?;
            let layout = Layout::from_sql(chunks, bytes)?;
            Ok(Retired {
                kind,
                generation: Generation::from_sql(&generation)?,
                layout,
                store: Store::parse(&store, kind, layout)?,
            })
        })
        .collect()
    }

    /// Records a sweep of `retired`: absent names, deleted in an earlier open, make it Swept for
    /// good; anything else records this open as its latest delete.
    ///
    /// # Errors
    /// Conflict when it's no longer Removed; otherwise a SQLite failure.
    pub(crate) fn observed(
        &mut self,
        retired: &Retired,
        observed: Observed,
    ) -> Result<(), AuthorityError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted_in: Option<i64> = tx
            .query_row(
                "SELECT deleted_in FROM generations WHERE generation = ?1 AND state = 'removed'",
                [retired.generation.0.as_slice()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(AuthorityError::Conflict)?;
        if observed == Observed::Absent && deleted_in.is_some_and(|deleted| deleted < self.lifetime)
        {
            tx.execute(
                "UPDATE generations SET state = 'swept', chunks = NULL, bytes = NULL
                 WHERE generation = ?1",
                [retired.generation.0.as_slice()],
            )?;
        } else {
            tx.execute(
                "UPDATE generations SET deleted_in = ?1 WHERE generation = ?2",
                params![self.lifetime, retired.generation.0.as_slice()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn activate_plan(conn: &Connection, plan: &WritePlan) -> Result<ActiveRef, AuthorityError> {
    let pending = conn
        .query_row(
            "SELECT 1 FROM generations
             WHERE generation = ?1 AND kind = ?2 AND state = 'pending' AND store = 'private'",
            params![plan.generation.0.as_slice(), plan.kind.label()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !pending || current(conn, plan.kind)?.revision != plan.base {
        return Err(AuthorityError::Conflict);
    }
    retire_active(conn, plan.kind)?;
    conn.execute(
        "UPDATE generations SET state = 'active' WHERE generation = ?1",
        [plan.generation.0.as_slice()],
    )?;
    bump(conn, plan.kind)?;
    Ok(ActiveRef {
        kind: plan.kind,
        generation: plan.generation,
        layout: plan.layout,
    })
}

fn retire_active(conn: &Connection, kind: Kind) -> Result<(), AuthorityError> {
    conn.execute(
        "UPDATE generations SET state = 'removed' WHERE kind = ?1 AND state = 'active'",
        [kind.label()],
    )?;
    Ok(())
}

fn current(conn: &Connection, kind: Kind) -> Result<Current, AuthorityError> {
    let (revision, active) = observe(conn, kind)?;
    Ok(Current {
        revision,
        active: active?,
    })
}

fn observe(
    conn: &Connection,
    kind: Kind,
) -> Result<(u64, Result<Option<ActiveRef>, AuthorityError>), AuthorityError> {
    let (revision, generation, chunks, bytes) = conn
        .query_row(
            "SELECT heads.revision, g.generation, g.chunks, g.bytes
             FROM heads LEFT JOIN generations g ON g.kind = heads.kind AND g.state = 'active'
             WHERE heads.kind = ?1",
            [kind.label()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(AuthorityError::Corrupt)?;
    let revision = u64::try_from(revision).map_err(|_| AuthorityError::Corrupt)?;
    let active = generation
        .map(|generation| {
            Ok::<_, AuthorityError>(ActiveRef {
                kind,
                generation: Generation::from_sql(&generation)?,
                layout: Layout::from_sql(chunks, bytes)?,
            })
        })
        .transpose();
    Ok((revision, active))
}

fn head_revision(conn: &Connection, kind: Kind) -> Result<u64, AuthorityError> {
    let revision: i64 = conn
        .query_row(
            "SELECT revision FROM heads WHERE kind = ?1",
            [kind.label()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(AuthorityError::Corrupt)?;
    u64::try_from(revision).map_err(|_| AuthorityError::Corrupt)
}

fn bump(conn: &Connection, kind: Kind) -> Result<(), AuthorityError> {
    let changed = conn.execute(
        "UPDATE heads SET revision = revision + 1 WHERE kind = ?1",
        [kind.label()],
    )?;
    if changed != 1 {
        return Err(AuthorityError::Corrupt);
    }
    Ok(())
}

fn marker_json(marker: SignOutMarker) -> Result<String, AuthorityError> {
    let json = serde_json::to_string(&marker).map_err(|_| AuthorityError::Bounds)?;
    if json.len() > MARKER_BYTES {
        return Err(AuthorityError::Bounds);
    }
    Ok(json)
}

fn read_marker(conn: &Connection) -> Result<SignOutMarker, AuthorityError> {
    let json: String = conn
        .query_row("SELECT marker FROM control WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or(AuthorityError::Corrupt)?;
    if json.len() > MARKER_BYTES {
        return Ok(SignOutMarker::fail_closed());
    }
    Ok(match serde_json::from_str::<SignOutMarker>(&json) {
        Ok(marker) if marker.valid() => marker,
        _ => SignOutMarker::fail_closed(),
    })
}

fn write_marker(conn: &Connection, marker: SignOutMarker) -> Result<(), AuthorityError> {
    let changed = conn.execute(
        "UPDATE control SET marker = ?1 WHERE id = 1",
        [marker_json(marker)?],
    )?;
    if changed != 1 {
        return Err(AuthorityError::Corrupt);
    }
    Ok(())
}
