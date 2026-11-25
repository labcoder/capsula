//! Opens an existing database read-only without creating, changing or removing any of its files.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::purge::sidecar;
use crate::platform::SessionLock;

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
/// borrows it, so the lock is held through SQLite's last use. Today that covers the credential
/// authority, not the history store, whose writer opens it before taking the lock. With a nonempty
/// WAL and index, `readonly_shm` reads both through read-only handles.
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
}
