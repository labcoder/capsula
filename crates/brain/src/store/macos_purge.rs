use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rusqlite::{Connection, OpenFlags, params};

use crate::platform::{PurgeDirectory, PurgeFileId, PurgeIdentity, PurgeInode, RenameMode};

use super::purge::{CLEAN, OLD, SNAPSHOT, Swap, missing, sidecar};
use super::{Store, StoreError, checkpoint_now, owes, schema};

const INTENT: &str = ".purge-intent";
const STAGED: &str = ".purge-intent.tmp";
const MAGIC: [u8; 4] = *b"KPG2";
const SIZE: usize = MAGIC.len() + 3 * 16 + 24;
const STAMP_SCHEMA: &str = "
CREATE TABLE mac_test_purge_generation (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_id BLOB NOT NULL CHECK (length(store_id) = 16),
    generation_id BLOB NOT NULL CHECK (length(generation_id) = 16)
) STRICT;";

type SchemaObject = (String, String, String, String);
static REQUIRED_SCHEMA: OnceLock<Result<Vec<SchemaObject>, ErrorKind>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    store: [u8; 16],
    generation: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Intent {
    old: Stamp,
    new: Stamp,
    original: PurgeFileId,
}

impl Intent {
    fn encode(self) -> [u8; SIZE] {
        let mut bytes = [0; SIZE];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..20].copy_from_slice(&self.old.store);
        bytes[20..36].copy_from_slice(&self.old.generation);
        bytes[36..52].copy_from_slice(&self.new.generation);
        bytes[52..].copy_from_slice(&self.original);
        bytes
    }

    fn decode(bytes: &[u8; SIZE]) -> io::Result<Self> {
        if bytes[..4] != MAGIC || bytes[20..36] == bytes[36..52] {
            return Err(invalid());
        }
        let store = bytes[4..20].try_into().map_err(|_| invalid())?;
        Ok(Self {
            old: Stamp {
                store,
                generation: bytes[20..36].try_into().map_err(|_| invalid())?,
            },
            new: Stamp {
                store,
                generation: bytes[36..52].try_into().map_err(|_| invalid())?,
            },
            original: bytes[52..].try_into().map_err(|_| invalid())?,
        })
    }
}

#[derive(Debug)]
pub(super) struct CopiedMac {
    intent: Intent,
    clean: PurgeIdentity,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ActiveSwap {
    intent: Intent,
    canonical: PurgeIdentity,
    role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Old,
    Successor,
}

struct Files<'a> {
    dir: PurgeDirectory,
    db: &'a Path,
    old: PathBuf,
    clean: PathBuf,
    intent: PathBuf,
    staged: PathBuf,
}

struct Recovery<'a> {
    files: Files<'a>,
    attempt: ActiveSwap,
    record: PurgeInode,
    current: PurgeInode,
    alias: Option<PurgeInode>,
    clean: Option<PurgeInode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    Closed,
    Link,
    Rename,
    Flush,
    Reopen,
    Unlink,
    Retire,
}

type TestLink<'a> = Box<dyn FnOnce(&Path, &Path) -> io::Result<()> + 'a>;

impl<'a> Files<'a> {
    fn open(db: &'a Path) -> io::Result<Option<Self>> {
        let parent = db.parent().ok_or(ErrorKind::InvalidInput)?;
        let dir = match PurgeDirectory::open(parent) {
            Ok(Some(dir)) => dir,
            Ok(None) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(Self {
            dir,
            db,
            old: sidecar(db, OLD),
            clean: sidecar(db, CLEAN),
            intent: sidecar(db, INTENT),
            staged: sidecar(db, STAGED),
        }))
    }

    fn no_journals(&self, path: &Path) -> io::Result<()> {
        for suffix in ["-wal", "-shm", "-journal"] {
            if !self.dir.absent(&sidecar(path, suffix))? {
                return Err(invalid());
            }
        }
        Ok(())
    }

    fn hold_optional(&self, path: &Path) -> io::Result<Option<PurgeInode>> {
        match self.dir.hold(path) {
            Ok(inode) => Ok(Some(inode)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn load(&self) -> io::Result<Option<(Intent, PurgeInode)>> {
        if !self.dir.absent(&self.staged)? {
            return Err(invalid());
        }
        let Some(mut file) = self.hold_optional(&self.intent)? else {
            return Ok(None);
        };
        self.dir.verify(&self.intent, &file, 1)?;
        let mut bytes = [0; SIZE];
        file.read_exact(&mut bytes)?;
        Ok(Some((Intent::decode(&bytes)?, file)))
    }

    fn publish_intent(&self, intent: Intent) -> io::Result<()> {
        let held = self.dir.stage_intent(&self.staged, &intent.encode())?;
        self.dir.verify(&self.staged, &held, 1)?;
        self.dir
            .rename(&self.staged, &self.intent, RenameMode::NoReplace)?;
        self.dir.verify(&self.intent, &held, 1)?;
        self.dir.durable(&held)
    }

    fn publish_linked(
        &self,
        old: &PurgeInode,
        clean: &PurgeInode,
        mut reach: impl FnMut(Boundary) -> io::Result<()>,
    ) -> io::Result<()> {
        self.dir.verify(self.db, old, 2)?;
        self.dir.verify(&self.old, old, 2)?;
        reach(Boundary::Link)?;
        self.dir.durable(old)?;
        self.dir.verify(&self.clean, clean, 1)?;
        self.dir.rename(&self.clean, self.db, RenameMode::Replace)?;
        reach(Boundary::Rename)?;
        self.dir.verify(self.db, clean, 1)?;
        self.dir.verify(&self.old, old, 1)?;
        self.dir.durable(clean)?;
        reach(Boundary::Flush)
    }

    fn immutable_stamp(
        &self,
        path: &Path,
        held: &PurgeInode,
        links: u16,
    ) -> io::Result<(Stamp, bool)> {
        if !self.dir.absent(&sidecar(path, "-journal"))? {
            return Err(invalid());
        }
        let wal = sidecar(path, "-wal");
        if let Some(held) = self.hold_optional(&wal)? {
            self.dir.verify(&wal, &held, 1)?;
            if !held.is_empty()? {
                return Err(invalid());
            }
        }
        let shm = sidecar(path, "-shm");
        if let Some(held) = self.hold_optional(&shm)? {
            self.dir.verify(&shm, &held, 1)?;
        }
        self.dir.verify(path, held, links)?;
        let mut uri = url::Url::from_file_path(path).map_err(|()| invalid())?;
        uri.query_pairs_mut().append_pair("immutable", "1");
        let db = Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|_| invalid())?;
        let stamp = checked_stamp(&db).map_err(|_| invalid())?;
        let pending = owes(&db).map_err(|_| invalid())?;
        verify_live_schema(&db)?;
        db.close().map_err(|_| invalid())?;
        self.dir.verify(path, held, links)?;
        Ok((stamp, pending))
    }
}

fn invalid() -> io::Error {
    ErrorKind::InvalidData.into()
}

fn schema_objects(conn: &Connection) -> rusqlite::Result<Vec<SchemaObject>> {
    let mut query = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema
         WHERE sql IS NOT NULL ORDER BY type, name",
    )?;
    query
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect()
}

fn verify_live_schema(conn: &Connection) -> io::Result<()> {
    let expected = REQUIRED_SCHEMA
        .get_or_init(|| {
            let mut reference = Connection::open_in_memory().map_err(|_| ErrorKind::InvalidData)?;
            schema::migrate(&mut reference).map_err(|_| ErrorKind::InvalidData)?;
            reference
                .execute_batch(STAMP_SCHEMA)
                .map_err(|_| ErrorKind::InvalidData)?;
            schema_objects(&reference).map_err(|_| ErrorKind::InvalidData)
        })
        .as_ref()
        .map_err(|kind| io::Error::from(*kind))?;
    let actual = schema_objects(conn).map_err(|_| invalid())?;
    let integrity: String = conn
        .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
        .map_err(|_| invalid())?;
    if integrity != "ok" || expected.iter().any(|required| !actual.contains(required)) {
        return Err(invalid());
    }
    Ok(())
}

fn random_id() -> io::Result<[u8; 16]> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|_| io::Error::other("purge identity unavailable"))?;
    Ok(id)
}

fn stamp(conn: &Connection) -> rusqlite::Result<Stamp> {
    conn.query_row(
        "SELECT store_id, generation_id FROM mac_test_purge_generation WHERE singleton = 1",
        [],
        |row| {
            Ok(Stamp {
                store: row.get(0)?,
                generation: row.get(1)?,
            })
        },
    )
}

fn checked_stamp(conn: &Connection) -> Result<Stamp, StoreError> {
    let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if usize::try_from(schema_version) != Ok(schema::MIGRATIONS.len()) {
        return Err(StoreError::Purge(invalid()));
    }
    Ok(stamp(conn)?)
}

fn seed_stamp(conn: &mut Connection) -> Result<(), StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
          WHERE type = 'table' AND name = 'mac_test_purge_generation')",
        [],
        |row| row.get(0),
    )?;
    if exists {
        checked_stamp(conn)?;
    } else {
        let store = random_id().map_err(StoreError::Purge)?;
        let generation = random_id().map_err(StoreError::Purge)?;
        let tx = conn.transaction()?;
        tx.execute_batch(STAMP_SCHEMA)?;
        tx.execute(
            "INSERT INTO mac_test_purge_generation VALUES (1, ?1, ?2)",
            params![store, generation],
        )?;
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn stamp_snapshot(path: &Path) -> Result<Option<Intent>, StoreError> {
    let Some(files) = Files::open(path).map_err(StoreError::Purge)? else {
        return Ok(None);
    };
    let canonical = files.dir.hold(path).map_err(StoreError::Purge)?;
    files
        .dir
        .verify(path, &canonical, 1)
        .map_err(StoreError::Purge)?;
    let snapshot = sidecar(path, SNAPSHOT);
    files.no_journals(&snapshot).map_err(StoreError::Purge)?;
    let held = files.dir.hold(&snapshot).map_err(StoreError::Purge)?;
    files
        .dir
        .verify(&snapshot, &held, 1)
        .map_err(StoreError::Purge)?;
    let conn = open_snapshot(&snapshot)?;
    let old = checked_stamp(&conn)?;
    let generation = random_id().map_err(StoreError::Purge)?;
    if generation == old.generation || owes(&conn)? {
        return Err(StoreError::Purge(invalid()));
    }
    conn.execute(
        "UPDATE mac_test_purge_generation SET generation_id = ?1 WHERE singleton = 1",
        [generation],
    )?;
    conn.close().map_err(|(_, err)| err)?;
    files
        .dir
        .verify(&snapshot, &held, 1)
        .map_err(StoreError::Purge)?;
    Ok(Some(Intent {
        old,
        new: Stamp {
            store: old.store,
            generation,
        },
        original: canonical.file_id(),
    }))
}

fn open_snapshot(path: &Path) -> Result<Connection, StoreError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| sanitize(StoreError::Sql(error)))
}

fn open_canonical(path: &Path) -> Result<Connection, StoreError> {
    super::connect(path, OpenFlags::SQLITE_OPEN_NOFOLLOW).map_err(sanitize)
}

pub(super) fn sanitize(error: StoreError) -> StoreError {
    match error {
        StoreError::Sql(rusqlite::Error::SqliteFailure(error, _)) => StoreError::Purge(
            io::Error::other(format!("SQLite {:?} ({})", error.code, error.extended_code)),
        ),
        StoreError::Sql(_) => StoreError::Purge(invalid()),
        other => other,
    }
}

pub(super) fn sanitize_open(error: StoreError) -> StoreError {
    match &error {
        StoreError::Sql(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::CannotOpen =>
        {
            sanitize(error)
        }
        StoreError::Sql(rusqlite::Error::InvalidPath(_)) => sanitize(error),
        _ => error,
    }
}

pub(super) fn bind_copy(path: &Path, intent: Intent) -> Result<CopiedMac, StoreError> {
    let files = Files::open(path)
        .map_err(StoreError::Purge)?
        .ok_or_else(|| StoreError::Purge(invalid()))?;
    let held = files.dir.hold(&files.clean).map_err(StoreError::Purge)?;
    if files
        .immutable_stamp(&files.clean, &held, 1)
        .map_err(StoreError::Purge)?
        != (intent.new, false)
    {
        return Err(StoreError::Purge(invalid()));
    }
    Ok(CopiedMac {
        intent,
        clean: held.identity(),
    })
}

pub(super) fn evidence_exists(path: &Path) -> bool {
    [INTENT, STAGED]
        .iter()
        .any(|suffix| !missing(&sidecar(path, suffix)))
}

pub(super) fn cleanup_preintent(path: &Path) -> io::Result<bool> {
    let Some(files) = Files::open(path)? else {
        return Ok(false);
    };
    if files.load()?.is_some() {
        return Err(invalid());
    }
    let snapshot = sidecar(path, SNAPSHOT);
    let candidates = [&snapshot, &files.clean, &files.old];
    for candidate in candidates {
        files.no_journals(candidate)?;
    }
    if candidates
        .iter()
        .all(|path| matches!(files.dir.absent(path), Ok(true)))
    {
        return Ok(true);
    }
    let current = files.dir.hold(path)?;
    if !files.dir.absent(&sidecar(path, "-journal"))? {
        return Err(invalid());
    }
    let live = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| invalid())?;
    let stamp = checked_stamp(&live).map_err(|_| invalid())?;
    drop(live);
    files.dir.verify(path, &current, 1)?;
    let mut owned = Vec::new();
    for candidate in candidates {
        if let Some(held) = files.hold_optional(candidate)? {
            // A stopped VACUUM INTO leaves an empty file: no content, so no owner to confirm.
            if !held.is_empty()?
                && files.immutable_stamp(candidate, &held, 1)?.0.store != stamp.store
            {
                return Err(invalid());
            }
            owned.push((candidate, held));
        }
    }
    for (candidate, held) in owned {
        files.dir.remove(candidate, &held, 1)?;
    }
    files.dir.durable(&current)?;
    Ok(true)
}

pub(super) fn promote(
    store: &mut Store,
    writes: u64,
    copied: &CopiedMac,
    link: Option<TestLink<'_>>,
) -> Result<Swap, StoreError> {
    promote_with(
        store,
        writes,
        copied,
        |files| match link {
            Some(link) => link(files.db, &files.old),
            None => files.dir.link(files.db, &files.old),
        },
        |_| Ok(()),
    )
}

pub(super) fn fail_reopen(store: &mut Store, writes: u64, copied: &CopiedMac) -> PathBuf {
    let journal = sidecar(&store.path, "-journal");
    let result = promote_with(
        store,
        writes,
        copied,
        |files| files.dir.link(files.db, &files.old),
        |step| {
            if step == Boundary::Reopen {
                std::fs::create_dir(&journal)?;
            }
            Ok(())
        },
    );
    assert!(
        result.is_err() && !store.serving,
        "successor is unavailable after publication"
    );
    journal
}

pub(super) fn leave_link_for_restart(store: &Store, copied: &CopiedMac) -> Result<(), StoreError> {
    let files = Files::open(&store.path)
        .map_err(StoreError::Purge)?
        .ok_or_else(|| StoreError::Purge(invalid()))?;
    let old = files.dir.hold(files.db).map_err(StoreError::Purge)?;
    let clean = files.dir.hold(&files.clean).map_err(StoreError::Purge)?;
    if clean.identity() != copied.clean
        || !checkpoint_now(&store.conn)?
        || old.file_id() != copied.intent.original
        || checked_stamp(&store.conn)? != copied.intent.old
    {
        return Err(StoreError::Purge(invalid()));
    }
    old.sync().map_err(StoreError::Purge)?;
    clean.sync().map_err(StoreError::Purge)?;
    files
        .publish_intent(copied.intent)
        .map_err(StoreError::Purge)?;
    files
        .dir
        .link(files.db, &files.old)
        .map_err(StoreError::Purge)?;
    files
        .dir
        .verify(files.db, &old, 2)
        .map_err(StoreError::Purge)?;
    files
        .dir
        .verify(&files.old, &old, 2)
        .map_err(StoreError::Purge)?;
    files.dir.durable(&old).map_err(StoreError::Purge)
}

#[expect(
    clippy::too_many_lines,
    reason = "one ordered publication sequence; splitting it would separate each barrier from its step"
)]
fn promote_with(
    store: &mut Store,
    writes: u64,
    copied: &CopiedMac,
    link: impl FnOnce(&Files<'_>) -> io::Result<()>,
    mut reach: impl FnMut(Boundary) -> io::Result<()>,
) -> Result<Swap, StoreError> {
    if store.conn.total_changes() != writes {
        return Ok(Swap::Stale);
    }
    let side = Arc::clone(&store.side);
    let Some(fence) = side.try_hold() else {
        return Ok(Swap::Blocked);
    };
    let path = store.path.clone();
    let files = Files::open(&path)
        .map_err(StoreError::Purge)?
        .ok_or_else(|| StoreError::Purge(invalid()))?;
    if files.load().map_err(StoreError::Purge)?.is_some() {
        return Err(StoreError::Purge(invalid()));
    }
    let clean = files.dir.hold(&files.clean).map_err(StoreError::Purge)?;
    let old = files.dir.hold(files.db).map_err(StoreError::Purge)?;
    if clean.identity() != copied.clean
        || old.file_id() != copied.intent.original
        || checked_stamp(&store.conn)? != copied.intent.old
        || !owes(&store.conn)?
        || files
            .immutable_stamp(&files.clean, &clean, 1)
            .map_err(StoreError::Purge)?
            != (copied.intent.new, false)
    {
        return Err(StoreError::Purge(invalid()));
    }
    files
        .dir
        .verify(files.db, &old, 1)
        .map_err(StoreError::Purge)?;
    if !checkpoint_now(&store.conn)? {
        return Ok(Swap::Blocked);
    }
    let serving = std::mem::replace(&mut store.conn, Connection::open_in_memory()?);
    if let Err((serving, error)) = serving.close() {
        store.conn = serving;
        return Err(error.into());
    }
    store.serving = false;
    store.mac_published = None;
    fence.unavailable();
    reach(Boundary::Closed).map_err(StoreError::Purge)?;
    if let Err(error) = files.no_journals(files.db) {
        if let Err(error) = files.dir.verify(files.db, &old, 1) {
            stop_uncertain("verify old store", &error);
        }
        store.mac_published = Some(ActiveSwap {
            intent: copied.intent,
            canonical: old.identity(),
            role: Role::Old,
        });
        if files
            .dir
            .absent(&sidecar(files.db, "-journal"))
            .map_err(StoreError::Purge)?
        {
            store.reopen()?;
            fence.available();
        }
        return Err(StoreError::Purge(error));
    }
    let ready = (|| {
        if files.immutable_stamp(files.db, &old, 1)? != (copied.intent.old, true) {
            return Err(invalid());
        }
        old.sync()?;
        clean.sync()?;
        files.publish_intent(copied.intent)
    })();
    if let Err(error) = ready {
        stop_uncertain("before link", &error);
    }
    if let Err(error) = link(&files) {
        let unsupported = matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP));
        if !unchanged(&files, &old, &clean)
            || !(unsupported || error.kind() == ErrorKind::AlreadyExists)
        {
            stop_uncertain("link", &error);
        }
        let settled = abandon_link(&files, &old, copied.intent, error.kind());
        if let Err(settle) = settled {
            stop_uncertain("abandon link", &settle);
        }
        store.mac_published = Some(ActiveSwap {
            intent: copied.intent,
            canonical: old.identity(),
            role: Role::Old,
        });
        store.reopen()?;
        fence.available();
        return Err(StoreError::Purge(error));
    }
    if let Err(error) = files.publish_linked(&old, &clean, &mut reach) {
        stop_uncertain("publish", &error);
    }
    store.mac_published = Some(ActiveSwap {
        intent: copied.intent,
        canonical: clean.identity(),
        role: Role::Successor,
    });
    reach(Boundary::Reopen).map_err(StoreError::Purge)?;
    store.reopen()?;
    fence.available();
    Ok(Swap::Done)
}

fn unchanged(files: &Files<'_>, old: &PurgeInode, clean: &PurgeInode) -> bool {
    matches!(files.dir.absent(&files.old), Ok(true))
        && files.dir.verify(files.db, old, 1).is_ok()
        && files.dir.verify(&files.clean, clean, 1).is_ok()
}

fn abandon_link(
    files: &Files<'_>,
    old: &PurgeInode,
    expected: Intent,
    kind: ErrorKind,
) -> io::Result<()> {
    if kind != ErrorKind::AlreadyExists {
        return reconcile(files.db, None).map(|_| ());
    }
    let (intent, record) = files.load()?.ok_or_else(invalid)?;
    if intent != expected {
        return Err(invalid());
    }
    files.dir.remove(&files.intent, &record, 1)?;
    files.dir.durable(old)
}

fn stop_uncertain(stage: &str, error: &io::Error) -> ! {
    let _ = writeln!(
        io::stderr(),
        "macOS scratch purge indeterminate at {stage}: {:?} (errno {:?})",
        error.kind(),
        error.raw_os_error()
    );
    std::process::exit(78);
}

impl ActiveSwap {
    pub(super) fn published(&self) -> bool {
        self.role == Role::Successor
    }

    pub(super) fn reopen(self, path: &Path) -> Result<Connection, StoreError> {
        let files = Files::open(path)
            .map_err(StoreError::Purge)?
            .ok_or_else(|| StoreError::Purge(invalid()))?;
        let held = files.dir.hold(path).map_err(StoreError::Purge)?;
        if held.identity() != self.canonical {
            return Err(StoreError::Purge(invalid()));
        }
        files
            .dir
            .verify(path, &held, 1)
            .map_err(StoreError::Purge)?;
        if self.published() {
            files.no_journals(path).map_err(StoreError::Purge)?;
        } else if !files
            .dir
            .absent(&sidecar(path, "-journal"))
            .map_err(StoreError::Purge)?
        {
            return Err(StoreError::Purge(invalid()));
        }
        let expected = match self.role {
            Role::Old => (self.intent.old, true),
            Role::Successor => (self.intent.new, false),
        };
        if files
            .immutable_stamp(path, &held, 1)
            .map_err(StoreError::Purge)?
            != expected
        {
            return Err(StoreError::Purge(invalid()));
        }
        if self.published() {
            reconcile(path, Some(self)).map_err(StoreError::Purge)?;
        }
        let conn = open_canonical(path)?;
        files
            .dir
            .verify(path, &held, 1)
            .map_err(StoreError::Purge)?;
        if (checked_stamp(&conn)?, owes(&conn)?) != expected {
            return Err(StoreError::Purge(invalid()));
        }
        Ok(conn)
    }
}

pub(super) fn recover(path: &Path) -> io::Result<Option<ActiveSwap>> {
    if missing(path) && !missing(&sidecar(path, OLD)) && !evidence_exists(path) {
        return Ok(None);
    }
    let Some(recovery) = classify(path, None)? else {
        return Ok(None);
    };
    let attempt = recovery.attempt;
    if !attempt.published() {
        recovery.cleanup(|_| Ok(()))?;
    }
    Ok(Some(attempt))
}

pub(super) fn open_initial(
    path: &Path,
    recovered: Option<ActiveSwap>,
) -> Result<Option<Connection>, StoreError> {
    if let Some(active) = recovered {
        return active.reopen(path).map(Some);
    }
    let Some(files) = Files::open(path).map_err(StoreError::Purge)? else {
        return Ok(None);
    };
    let held = if let Some(held) = files.hold_optional(path).map_err(StoreError::Purge)? {
        files
            .dir
            .verify(path, &held, 1)
            .map_err(StoreError::Purge)?;
        if !files
            .dir
            .absent(&sidecar(path, "-journal"))
            .map_err(StoreError::Purge)?
        {
            return Err(StoreError::Purge(invalid()));
        }
        for suffix in ["-wal", "-shm"] {
            let journal = sidecar(path, suffix);
            if let Some(held) = files.hold_optional(&journal).map_err(StoreError::Purge)? {
                files
                    .dir
                    .verify(&journal, &held, 1)
                    .map_err(StoreError::Purge)?;
            }
        }
        files.dir.durable(&held).map_err(StoreError::Purge)?;
        held
    } else {
        files.no_journals(path).map_err(StoreError::Purge)?;
        files.dir.create(path).map_err(StoreError::Purge)?
    };
    let mut conn = open_canonical(path)?;
    files
        .dir
        .verify(path, &held, 1)
        .map_err(StoreError::Purge)?;
    schema::migrate(&mut conn).map_err(sanitize)?;
    seed_stamp(&mut conn).map_err(sanitize)?;
    files
        .dir
        .verify(path, &held, 1)
        .map_err(StoreError::Purge)?;
    Ok(Some(conn))
}

pub(super) fn reconcile(path: &Path, active: Option<ActiveSwap>) -> io::Result<bool> {
    reconcile_with(path, active, |_| Ok(()))
}

fn reconcile_with(
    path: &Path,
    active: Option<ActiveSwap>,
    mut reach: impl FnMut(Boundary) -> io::Result<()>,
) -> io::Result<bool> {
    let Some(recovery) = classify(path, active)? else {
        if let Some(active) = active.filter(ActiveSwap::published) {
            let files = Files::open(path)?.ok_or_else(invalid)?;
            let held = files.dir.hold(path)?;
            if held.identity() != active.canonical
                || files.immutable_stamp(path, &held, 1)? != (active.intent.new, false)
            {
                return Err(invalid());
            }
            reach(Boundary::Retire)?;
            files.dir.durable(&held)?;
            return Ok(true);
        }
        return Ok(false);
    };
    recovery.cleanup(reach)?;
    Ok(true)
}

impl Recovery<'_> {
    fn cleanup(self, mut reach: impl FnMut(Boundary) -> io::Result<()>) -> io::Result<()> {
        let Self {
            files,
            attempt,
            record,
            current,
            alias,
            clean,
        } = self;
        files.dir.verify(&files.intent, &record, 1)?;
        files.dir.durable(&current)?;
        if let Some(alias) = alias.as_ref().filter(|_| !attempt.published()) {
            files.dir.remove(&files.old, alias, 2)?;
        }
        if let Some(clean) = &clean {
            files.dir.remove(&files.clean, clean, 1)?;
        }
        reach(Boundary::Unlink)?;
        files.dir.verify(files.db, &current, 1)?;
        if (!attempt.published() && !files.dir.absent(&files.old)?)
            || !files.dir.absent(&files.clean)?
        {
            return Err(invalid());
        }
        files.dir.durable(&current)?;
        files.dir.remove(&files.intent, &record, 1)?;
        reach(Boundary::Retire)?;
        files.dir.durable(&current)
    }
}

fn classify(path: &Path, active: Option<ActiveSwap>) -> io::Result<Option<Recovery<'_>>> {
    let Some(files) = Files::open(path)? else {
        return if evidence_exists(path) || !missing(&sidecar(path, OLD)) {
            Err(invalid())
        } else {
            Ok(None)
        };
    };
    let Some((intent, record)) = files.load()? else {
        if files.dir.absent(path)? && super::purge::files(path).any(|file| !missing(&file)) {
            return Err(invalid());
        }
        return Ok(None);
    };
    let snapshot = sidecar(path, SNAPSHOT);
    if !files.dir.absent(&snapshot)? {
        return Err(invalid());
    }
    for source in [&snapshot, &files.clean, &files.old] {
        files.no_journals(source)?;
    }
    let current = files.dir.hold(path)?;
    let alias = files.hold_optional(&files.old)?;
    let clean = files.hold_optional(&files.clean)?;
    let stamp = if let Some(active) = active {
        if active.intent != intent || active.canonical != current.identity() {
            return Err(invalid());
        }
        files.dir.verify(path, &current, 1)?;
        intent.new
    } else {
        let links = if alias
            .as_ref()
            .is_some_and(|alias| alias.identity() == current.identity())
        {
            2
        } else {
            1
        };
        let (stamp, pending) = files.immutable_stamp(path, &current, links)?;
        if (stamp == intent.old) != pending {
            return Err(invalid());
        }
        stamp
    };

    if stamp == intent.old {
        if current.file_id() != intent.original {
            return Err(invalid());
        }
        if let Some(alias) = &alias {
            if alias.identity() != current.identity() {
                return Err(invalid());
            }
            files.dir.verify(&files.old, alias, 2)?;
        }
        if let Some(clean) = &clean
            && files.immutable_stamp(&files.clean, clean, 1)? != (intent.new, false)
        {
            return Err(invalid());
        }
    } else if stamp == intent.new && clean.is_none() {
        if let Some(alias) = &alias
            && (alias.file_id() != intent.original
                || files.immutable_stamp(&files.old, alias, 1)? != (intent.old, true))
        {
            return Err(invalid());
        }
    } else {
        return Err(invalid());
    }
    Ok(Some(Recovery {
        attempt: ActiveSwap {
            intent,
            canonical: current.identity(),
            role: if stamp == intent.old {
                Role::Old
            } else {
                Role::Successor
            },
        },
        files,
        record,
        current,
        alias,
        clean,
    }))
}

mod tests;
