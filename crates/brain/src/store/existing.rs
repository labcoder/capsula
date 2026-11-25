//! Opens an existing database read-only without creating, changing or removing any of its files.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::StoreError;
use super::purge::{Leftovers, missing, sidecar};
use super::schema::{self, MIGRATIONS};
use crate::platform::{SessionLock, folder_id};

/// SQLite's smallest page. A shorter main file can read as zero pages, and SQLite then deletes the
/// WAL beside it.
const MIN_PAGE: u64 = 512;

/// Why an existing database wasn't opened.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the existing-profile reader that calls it isn't landed yet"
    )
)]
#[derive(Debug, thiserror::Error)]
pub(crate) enum ExistingError {
    /// The database file doesn't exist.
    #[error("database file is missing")]
    Missing,
    /// SQLite can't read the files as they are without creating, changing or removing one.
    #[error("database can't be read without changing its files")]
    Uncertain,
    /// SQLite refused the read-only open.
    #[error("database open failed")]
    Sqlite(#[from] rusqlite::Error),
}

/// A read-only connection to an existing database that can't outlive the session lock it was
/// opened under.
#[derive(Debug)]
pub(crate) struct Existing<'lock> {
    conn: Connection,
    _session: PhantomData<&'lock SessionLock>,
}

/// Keeps the lock borrowed until the connection has closed, not just until its last use.
impl Drop for Existing<'_> {
    fn drop(&mut self) {}
}

impl Deref for Existing<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

/// Opens `db` read-only in the one mode that leaves its files as they are, or refuses.
///
/// Every file must be a regular file under one name, not a link or reparse point, so SQLite reads
/// the files that were checked. With no WAL, `immutable` reads only the main file and takes no
/// SQLite lock, so every writer of `db` must be one that takes `session`: the connection
/// borrows it, so the lock is held through SQLite's last use. The brain writes the credential
/// authority and the history store only under it. With a nonempty WAL and index, `readonly_shm`
/// reads both through read-only handles.
/// Anything else that SQLite would create, replay, re-permission or remove is uncertain: a
/// journal, a WAL or index without the other or empty, a main file shorter than a page. No other
/// connection to `db` may be open in this process, since SQLite shares one index handle per
/// process.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the existing-profile reader that calls it isn't landed yet"
    )
)]
pub(crate) fn open<'lock>(
    db: &Path,
    _session: &'lock SessionLock,
) -> Result<Existing<'lock>, ExistingError> {
    let present = |suffix| match fs::symlink_metadata(sidecar(db, suffix)) {
        Ok(metadata) if plain(&metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        _ => Err(ExistingError::Uncertain),
    };
    let mode = match (
        present("")?,
        present("-wal")?,
        present("-shm")?,
        present("-journal")?,
    ) {
        (None, ..) => return Err(ExistingError::Missing),
        (Some(MIN_PAGE..), None, None, None) => "immutable",
        (Some(MIN_PAGE..), Some(1..), Some(1..), None) => "readonly_shm",
        _ => return Err(ExistingError::Uncertain),
    };
    Ok(Existing {
        conn: Connection::open_with_flags(
            format!("{}?mode=ro&{mode}=1", uri(db)?),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?,
        _session: PhantomData,
    })
}

/// What a legacy history store at `db` shows, read under `session`, the profile's writer lock.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the existing-profile reader that calls it isn't landed yet"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegacyHistory {
    /// No store and nothing a store or a purge leaves beside one, in a folder that exists. The
    /// folder itself is still a footprint, so this isn't an absent profile.
    Missing,
    /// A schema version this build knows, with exactly the objects its migrations make at that
    /// version. Schema evidence only: no rows, product invariants, page integrity or binding are
    /// read, so it promises neither an intact store nor a successful open.
    KnownSchema,
    /// Anything else, including any failed read.
    Unverified,
}

/// Reads the legacy history store at `db` through [`open`], borrowing `session`, inside
/// [`in_folder`].
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the existing-profile reader that calls it isn't landed yet"
    )
)]
pub(crate) fn legacy_history(db: &Path, session: &SessionLock) -> LegacyHistory {
    in_folder(db, || match open(db, session) {
        Ok(read) => match known_schema(&read) {
            Ok(true) => LegacyHistory::KnownSchema,
            Ok(false) | Err(_) => LegacyHistory::Unverified,
        },
        Err(ExistingError::Missing) if left_nothing(db) => LegacyHistory::Missing,
        Err(_) => LegacyHistory::Unverified,
    })
    .unwrap_or(LegacyHistory::Unverified)
}

#[cfg(test)]
thread_local! {
    static BETWEEN: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

/// Runs `hook` once, in this thread's next [`in_folder`], after the folder's identity is taken
/// and before `read` runs.
#[cfg(test)]
pub(crate) fn between_folder_checks(hook: impl FnOnce() + 'static) {
    BETWEEN.set(Some(Box::new(hook)));
}

/// Runs `read` between taking the identity of `db`'s folder and confirming the same identity at
/// the same path again. A folder renamed or swapped meanwhile makes every name under the path read
/// as not found, so `read` counts only when the folder held still. `None` when the folder is
/// missing, a link or reparse point, or changed; a link could redirect the read to another
/// folder's store.
pub(crate) fn in_folder<T>(db: &Path, read: impl FnOnce() -> T) -> Option<T> {
    let dir = db.parent()?;
    let held = folder_id(dir)?;
    #[cfg(test)]
    if let Some(hook) = BETWEEN.take() {
        hook();
    }
    let read = read();
    (folder_id(dir) == Some(held)).then_some(read)
}

/// Whether nothing a store or a purge leaves is beside `db`: no WAL, index or journal and no purge
/// leftover. One that can't be checked counts as left.
pub(crate) fn left_nothing(db: &Path) -> bool {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .all(|suffix| missing(&sidecar(db, suffix)))
        && !Leftovers::of(db).exist()
}

/// Every schema object's type, name, table and SQL.
const CATALOG: &str = "SELECT type, name, tbl_name, sql FROM sqlite_schema";

/// Whether `conn`'s objects are exactly those this build's migrations make at its `user_version`.
/// The candidate's catalog is counted first, then each of its rows is read once, unsorted, and
/// must use up a distinct entry of the small expected set, so Rust holds no copy of a candidate
/// row and SQLite sorts nothing.
fn known_schema(conn: &Connection) -> Result<bool, StoreError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let Some(version) = usize::try_from(found)
        .ok()
        .filter(|version| (1..=MIGRATIONS.len()).contains(version))
    else {
        return Ok(false);
    };
    let mut built = Connection::open_in_memory()?;
    schema::migrate_to(&mut built, version)?;
    let mut expected = built
        .prepare(CATALOG)?
        .query_map([], |row| {
            Ok(Some([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]))
        })?
        .collect::<rusqlite::Result<Vec<Option<[Option<String>; 4]>>>>()?;
    let count: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    if usize::try_from(count).ok() != Some(expected.len()) {
        return Ok(false);
    }
    let mut catalog = conn.prepare(CATALOG)?;
    let mut rows = catalog.query([])?;
    while let Some(row) = rows.next()? {
        let mut candidate = [None; 4];
        for (column, value) in candidate.iter_mut().enumerate() {
            let Ok(text) = row.get_ref(column)?.as_str_or_null() else {
                return Ok(false);
            };
            *value = text;
        }
        let Some(entry) = expected.iter_mut().find(|entry| {
            entry.as_ref().is_some_and(|want| {
                want.iter()
                    .zip(candidate)
                    .all(|(want, got)| want.as_deref() == got)
            })
        }) else {
            return Ok(false);
        };
        *entry = None;
    }
    Ok(expected.iter().all(Option::is_none))
}

/// Whether `metadata` is a regular file with one name.
#[cfg(unix)]
fn plain(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.nlink() == 1
}

/// Whether `metadata` is a regular file and not a reparse point.
///
/// shortcut: a second hard link isn't detected here.
#[cfg(windows)]
fn plain(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

/// `db` as a `file:` URI with every byte but unreserved ones percent-encoded, separators and
/// drive colons included, so SQLite decodes the path unchanged and never reads an authority.
fn uri(db: &Path) -> Result<String, ExistingError> {
    let mut uri = String::from("file:");
    for byte in db.to_str().ok_or(ExistingError::Uncertain)?.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(char::from(byte));
            }
            _ => write!(uri, "%{byte:02X}").expect("a String takes writes"),
        }
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;
    use crate::test_support::{Scratch, files};

    /// A change to a left copy's folder.
    type Change = fn(&Path);

    /// Puts a link to the first path at the second.
    #[cfg(unix)]
    type Link = fn(&Path, &Path);

    /// A WAL database at `dir`, its last row only in the WAL while the returned writer is open.
    fn writer(dir: &Path) -> (PathBuf, Connection) {
        fs::create_dir_all(dir).expect("scratch folder");
        let db = dir.join("x.db");
        let conn = Connection::open(&db).expect("fixture database");
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .expect("WAL");
        assert_eq!(mode, "wal");
        conn.execute_batch(
            "CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1); PRAGMA wal_checkpoint(TRUNCATE);
             PRAGMA wal_autocheckpoint = 0; INSERT INTO t VALUES (2);",
        )
        .expect("fixture rows");
        (db, conn)
    }

    /// A copy in `dir` of the live database's main file, WAL and index.
    fn left(live: &Path, dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).expect("left folder");
        for name in ["x.db", "x.db-wal", "x.db-shm"] {
            fs::copy(live.with_file_name(name), dir.join(name)).expect("left file");
        }
        dir.join("x.db")
    }

    /// The session lock a reader holds, in its own scratch folder so the database folders keep
    /// only their own files.
    fn session() -> (Scratch, SessionLock) {
        let scratch = Scratch::new("existing-session");
        fs::create_dir_all(scratch.dir()).expect("lock folder");
        let lock = SessionLock::try_acquire(&scratch.dir().join("session.lock")).expect("lock");
        (scratch, lock)
    }

    fn rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT sum(v) FROM t", [], |row| row.get(0))
            .expect("fixture rows read")
    }

    #[test]
    fn a_closed_wal_database_reads_and_keeps_its_files() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-closed");
        let (db, conn) = writer(&scratch.dir().join("50% #1 é"));
        drop(conn);
        let dir = db.parent().expect("fixture folder");
        let before = files(dir);
        assert_eq!(before.len(), 1, "a clean close leaves only the main file");
        let read = open(&db, &lock).expect("existing database");
        assert_eq!(rows(&read), 3);
        drop(read);
        assert_eq!(files(dir), before);
    }

    #[test]
    fn a_left_wal_reads_its_commits_and_keeps_every_file() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-left-wal");
        let (live, conn) = writer(&scratch.dir().join("live"));
        let dir = scratch.dir().join("left");
        let db = left(&live, &dir);
        drop(conn);
        let before = files(&dir);
        assert!(!before[&OsString::from("x.db-wal")].bytes.is_empty());
        let read = open(&db, &lock).expect("existing database");
        assert_eq!(rows(&read), 3, "the WAL's commit is read");
        drop(read);
        assert_eq!(files(&dir), before);
    }

    #[test]
    fn a_missing_database_stays_missing() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-missing");
        fs::create_dir_all(scratch.dir()).expect("scratch folder");
        assert!(matches!(
            open(&scratch.dir().join("x.db"), &lock),
            Err(ExistingError::Missing)
        ));
        assert!(files(scratch.dir()).is_empty());
    }

    #[test]
    fn files_sqlite_would_change_are_uncertain_and_kept() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-uncertain");
        let (live, conn) = writer(&scratch.dir().join("live"));
        let cases: [(&str, Change); 6] = [
            ("wal-only", |dir| {
                fs::remove_file(dir.join("x.db-shm")).expect("no index");
            }),
            ("shm-only", |dir| {
                fs::remove_file(dir.join("x.db-wal")).expect("no WAL");
            }),
            ("journal", |dir| {
                fs::write(dir.join("x.db-journal"), b"journal").expect("journal");
            }),
            ("empty-wal", |dir| {
                fs::write(dir.join("x.db-wal"), b"").expect("empty WAL");
            }),
            ("empty-index", |dir| {
                let index = dir.join("x.db-shm");
                fs::write(&index, b"").expect("empty index");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&index, fs::Permissions::from_mode(0o600))
                        .expect("index mode unlike the main file's");
                }
            }),
            ("one-byte-main", |dir| {
                fs::write(dir.join("x.db"), b"S").expect("short main");
            }),
        ];
        for (case, change) in cases {
            let dir = scratch.dir().join(case);
            let db = left(&live, &dir);
            change(&dir);
            let before = files(&dir);
            assert!(
                matches!(open(&db, &lock), Err(ExistingError::Uncertain)),
                "{case}"
            );
            assert_eq!(files(&dir), before, "{case}");
        }
        drop(conn);
        let empty = scratch.dir().join("empty");
        fs::create_dir(&empty).expect("empty folder");
        fs::write(empty.join("x.db"), b"").expect("empty file");
        assert!(matches!(
            open(&empty.join("x.db"), &lock),
            Err(ExistingError::Uncertain)
        ));
        assert_eq!(files(&empty).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_main_file_or_sidecar_is_uncertain_and_kept() {
        use std::os::unix::fs::symlink;
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-linked");
        let (live, conn) = writer(&scratch.dir().join("live"));
        let long = "t".repeat(200);
        let target = left(&live, &scratch.dir().join(&long).join(&long).join(&long));
        assert!(
            u64::try_from(target.as_os_str().len()).expect("path length") >= MIN_PAGE,
            "a link to the target is as long as a page"
        );
        drop(conn);
        let cases: [(&str, Link); 3] = [
            ("main-symlink", |target, db| {
                symlink(target, db).expect("main link");
            }),
            ("main-hard-link", |target, db| {
                fs::hard_link(target, db).expect("main hard link");
            }),
            ("index-symlink", |target, db| {
                fs::copy(target, db).expect("main copy");
                fs::copy(
                    target.with_file_name("x.db-wal"),
                    db.with_file_name("x.db-wal"),
                )
                .expect("WAL copy");
                symlink(
                    target.with_file_name("x.db-shm"),
                    db.with_file_name("x.db-shm"),
                )
                .expect("index link");
            }),
        ];
        for (case, link) in cases {
            let dir = scratch.dir().join(case);
            fs::create_dir(&dir).expect("case folder");
            let db = dir.join("x.db");
            link(&target, &db);
            let target_dir = target.parent().expect("target folder");
            let before = (files(&dir), files(target_dir));
            assert!(
                matches!(open(&db, &lock), Err(ExistingError::Uncertain)),
                "{case}"
            );
            assert_eq!((files(&dir), files(target_dir)), before, "{case}");
            fs::remove_dir_all(&dir).expect("case cleanup");
        }
    }

    /// A cleanly closed WAL store in `dir`, migrated to `version`.
    fn store_at(dir: &Path, version: usize) -> PathBuf {
        fs::create_dir_all(dir).expect("store folder");
        let db = dir.join("x.db");
        let mut conn = Connection::open(&db).expect("store");
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .expect("WAL");
        assert_eq!(mode, "wal");
        schema::migrate_to(&mut conn, version).expect("migrations");
        db
    }

    #[test]
    fn a_store_at_any_known_version_has_a_known_schema_and_keeps_its_files() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-history");
        for version in 1..=MIGRATIONS.len() {
            let db = store_at(&scratch.dir().join(format!("v{version}")), version);
            let dir = db.parent().expect("store folder");
            let before = files(dir);
            assert_eq!(
                legacy_history(&db, &lock),
                LegacyHistory::KnownSchema,
                "v{version}"
            );
            assert_eq!(files(dir), before, "v{version}");
        }
        let opened = scratch.dir().join("opened").join("x.db");
        drop(super::super::Store::open(&opened).expect("the brain's own open"));
        assert_eq!(legacy_history(&opened, &lock), LegacyHistory::KnownSchema);
        let copy = scratch.dir().join("copy");
        fs::create_dir(&copy).expect("copy folder");
        let source = Connection::open(scratch.dir().join("opened").join("x.db")).expect("source");
        source
            .execute(
                "VACUUM INTO ?1",
                [copy.join("x.db").to_str().expect("UTF-8 path")],
            )
            .expect("a purge's kind of copy");
        drop(source);
        assert_eq!(
            legacy_history(&copy.join("x.db"), &lock),
            LegacyHistory::KnownSchema
        );
    }

    #[test]
    fn a_newer_foreign_or_changed_schema_is_unverified_and_kept() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-history-changed");
        let known = MIGRATIONS.len();
        let newer = i64::try_from(known + 1).expect("small");
        let older = i64::try_from(known - 1).expect("small");
        let cases: [(&str, String); 6] = [
            ("newer", format!("PRAGMA user_version = {newer}")),
            ("unversioned", "PRAGMA user_version = 0".into()),
            ("extra-table", "CREATE TABLE extra (x INTEGER)".into()),
            (
                "extra-index",
                "CREATE INDEX extra ON threads (title)".into(),
            ),
            (
                "changed-table",
                "ALTER TABLE threads ADD COLUMN extra INTEGER".into(),
            ),
            (
                "ahead-of-its-version",
                format!("PRAGMA user_version = {older}"),
            ),
        ];
        for (case, change) in cases {
            let db = store_at(&scratch.dir().join(case), known);
            Connection::open(&db)
                .and_then(|conn| conn.execute_batch(&change))
                .expect("change the store");
            let dir = db.parent().expect("store folder");
            let before = files(dir);
            assert_eq!(
                legacy_history(&db, &lock),
                LegacyHistory::Unverified,
                "{case}"
            );
            assert_eq!(files(dir), before, "{case}");
        }
        let foreign = scratch.dir().join("foreign");
        let (db, conn) = writer(&foreign);
        drop(conn);
        assert_eq!(
            legacy_history(&db, &lock),
            LegacyHistory::Unverified,
            "foreign"
        );
    }

    #[test]
    fn a_missing_store_is_missing_only_in_its_own_folder_with_nothing_beside_it() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-history-missing");
        let bare = scratch.dir().join("bare");
        fs::create_dir_all(&bare).expect("bare folder");
        assert_eq!(
            legacy_history(&bare.join("x.db"), &lock),
            LegacyHistory::Missing
        );
        assert!(files(&bare).is_empty());
        assert_eq!(
            legacy_history(&scratch.dir().join("gone").join("x.db"), &lock),
            LegacyHistory::Unverified,
            "a missing folder"
        );
        assert!(!scratch.dir().join("gone").exists());
        for left in [
            "x.db-wal",
            "x.db-shm",
            "x.db-journal",
            "x.db.purge-old",
            "x.db.purge-snapshot",
            "x.db.purge-clean-wal",
        ] {
            let dir = scratch.dir().join(left);
            fs::create_dir_all(&dir).expect("case folder");
            fs::write(dir.join(left), b"left").expect("left file");
            let before = files(&dir);
            assert_eq!(
                legacy_history(&dir.join("x.db"), &lock),
                LegacyHistory::Unverified,
                "{left}"
            );
            assert_eq!(files(&dir), before, "{left}");
        }
        let refused = store_at(&scratch.dir().join("refused"), MIGRATIONS.len());
        fs::write(refused.with_file_name("x.db-wal"), b"").expect("empty WAL");
        assert_eq!(
            legacy_history(&refused, &lock),
            LegacyHistory::Unverified,
            "refused"
        );
    }

    #[test]
    fn a_folder_renamed_or_swapped_during_the_read_is_unverified() {
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-history-moved");
        for swap in [false, true] {
            let dir = scratch.dir().join(format!("profile-{swap}"));
            let moved = scratch.dir().join(format!("moved-{swap}"));
            fs::create_dir_all(&dir).expect("profile folder");
            let db = dir.join("x.db");
            assert_eq!(legacy_history(&db, &lock), LegacyHistory::Missing, "{swap}");
            let (from, to) = (dir.clone(), moved.clone());
            between_folder_checks(move || {
                fs::rename(&from, &to).expect("move the folder");
                if swap {
                    fs::create_dir(&from).expect("a new folder at the same path");
                }
            });
            assert_eq!(
                legacy_history(&db, &lock),
                LegacyHistory::Unverified,
                "{swap}"
            );
            assert!(files(&moved).is_empty(), "{swap}");
            assert_eq!(dir.exists(), swap, "{swap}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_folder_is_unverified_even_holding_a_known_store() {
        use std::os::unix::fs::symlink;
        let (_held, lock) = session();
        let scratch = Scratch::new("existing-history-linked");
        let known = store_at(&scratch.dir().join("known"), MIGRATIONS.len());
        assert_eq!(legacy_history(&known, &lock), LegacyHistory::KnownSchema);
        let target = known.parent().expect("store folder");
        symlink(target, scratch.dir().join("linked")).expect("folder link");
        let before = files(target);
        assert_eq!(
            legacy_history(&scratch.dir().join("linked").join("x.db"), &lock),
            LegacyHistory::Unverified,
            "a linked folder holding a known store"
        );
        assert_eq!(files(target), before);
        let empty = scratch.dir().join("empty");
        fs::create_dir(&empty).expect("empty folder");
        symlink(&empty, scratch.dir().join("linked-empty")).expect("folder link");
        assert_eq!(
            legacy_history(&scratch.dir().join("linked-empty").join("x.db"), &lock),
            LegacyHistory::Unverified,
            "a linked empty folder"
        );
    }
}
