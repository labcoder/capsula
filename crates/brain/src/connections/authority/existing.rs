//! A read-only look at a legacy profile's credential authority and the host it commits, for the
//! existing-profile observer. Nothing is created, migrated, changed or removed.

use std::path::Path;

use kumochi_protocol::constants::CREDENTIAL_SERVICE;
use kumochi_protocol::profile::Observation;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OptionalExtension as _};
use zeroize::Zeroizing;

use super::{AuthorityError, GENERATIONS, Kind, SCHEMA, VERSION, current, io};
use crate::connections::vault::{decode, valid_host};
use crate::platform::SessionLock;
use crate::platform::credential::CredentialStore;
use crate::store::existing::{self, ExistingError};

/// Every table's columns, foreign keys and index keys, and every other object's SQL, unsorted.
/// Table SQL isn't compared, since a file migrated to the current version differs in text from a
/// fresh one.
const SHAPE: &str = "
SELECT 'column', t.name, t.strict, c.cid, c.name, c.type, c.\"notnull\", c.dflt_value, c.pk
FROM pragma_table_list t, pragma_table_xinfo(t.name) c
WHERE t.schema = 'main' AND t.type = 'table'
UNION ALL
SELECT 'object', tbl_name, type, name, sql, NULL, NULL, NULL, NULL
FROM sqlite_schema WHERE type <> 'table'
UNION ALL
SELECT 'reference', m.name, f.id, f.seq, f.\"table\", f.\"from\", f.\"to\",
    f.on_update || ' ' || f.on_delete, f.match
FROM sqlite_schema m, pragma_foreign_key_list(m.name) f WHERE m.type = 'table'
UNION ALL
SELECT 'key', m.name, i.name, i.\"unique\", i.origin, i.partial, k.seqno, k.name, NULL
FROM sqlite_schema m, pragma_index_list(m.name) i, pragma_index_info(i.name) k
WHERE m.type = 'table'";

/// The ordinary kinds, whose heads every current authority holds.
const HEADS: [Kind; 5] = [
    Kind::Host,
    Kind::Account,
    Kind::Revocations,
    Kind::AnthropicKey,
    Kind::GitHub,
];

/// A legacy authority and the host it commits, both from one read-only snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Legacy {
    pub(crate) authority: Observation,
    pub(crate) host: Observation,
}

const UNVERIFIED: Legacy = Legacy {
    authority: Observation::Unverified,
    host: Observation::Unverified,
};

/// Whether `conn` holds an authority at this build's version, shaped as its open makes one, with
/// the rows its open and reads need. It only reads, so a live open can share it. Repair's rules,
/// such as a missing account chunk, stay in repair. A file its open would migrate (versions 1
/// and 2) or one in repair (version 4) isn't current. CHECK clauses are neither compared nor
/// re-verified, since SQLite's `integrity_check` skips them on a read-only connection, so the
/// `control` row's key and its lifetime, which the next open increments, are checked here.
///
/// # Errors
/// Corrupt for an older version, a different shape or a bad row; `NewerSchema` for a newer
/// version; Unavailable for a failed read.
pub(crate) fn check(conn: &Connection) -> Result<(), AuthorityError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != VERSION {
        return Err(if version > VERSION {
            AuthorityError::NewerSchema
        } else {
            AuthorityError::Corrupt
        });
    }
    if !same_shape(conn)? {
        return Err(AuthorityError::Corrupt);
    }
    let integrity: String = conn.query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))?;
    let orphans: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    let lifetime: Option<i64> = conn
        .query_row(
            "SELECT lifetime FROM control WHERE id = 1 AND (SELECT count(*) FROM control) = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if integrity != "ok"
        || orphans != 0
        || !lifetime.is_some_and(|lifetime| (0..i64::MAX).contains(&lifetime))
    {
        return Err(AuthorityError::Corrupt);
    }
    for kind in HEADS {
        current(conn, kind)?;
    }
    Ok(())
}

/// Whether `conn`'s shape is exactly a fresh authority's, built in memory from the open's own SQL.
/// The candidate's rows are read once, unsorted, and each must use up a distinct entry of the small
/// expected set, so Rust holds no copy of one, SQLite sorts nothing, and at most one row past the
/// expected count is read.
fn same_shape(conn: &Connection) -> rusqlite::Result<bool> {
    let built = Connection::open_in_memory()?;
    built.execute_batch(SCHEMA)?;
    built.execute_batch(GENERATIONS)?;
    let mut statement = built.prepare(SHAPE)?;
    let width = statement.column_count();
    let mut expected = statement
        .query_map([], |row| {
            (0..width)
                .map(|column| row.get(column))
                .collect::<rusqlite::Result<Vec<Value>>>()
                .map(Some)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = conn.prepare(SHAPE)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let mut matched = None;
        for (index, entry) in expected.iter().enumerate() {
            let Some(want) = entry else { continue };
            let mut same = true;
            for (column, want) in want.iter().enumerate() {
                same &= row.get_ref(column)? == ValueRef::from(want);
            }
            if same {
                matched = Some(index);
                break;
            }
        }
        let Some(index) = matched else {
            return Ok(false);
        };
        expected[index] = None;
    }
    Ok(expected.iter().all(Option::is_none))
}

/// Reads the legacy authority at `db` through [`existing::open`] under `session`, then the host it
/// commits under the fixed service, inside [`existing::in_folder`]. `Absent` only when the
/// authority's own folder holds no file and nothing a store or a purge leaves, as for the history
/// store; the folder is still a footprint. A missing, linked or moved folder, or any authority but
/// a current one, is `Unverified` for both.
pub(crate) fn legacy(db: &Path, session: &SessionLock, store: &impl CredentialStore) -> Legacy {
    existing::in_folder(db, || match existing::open(db, session) {
        Ok(read) if check(&read).is_ok() => Legacy {
            authority: Observation::Matching,
            host: host(&read, store),
        },
        Err(ExistingError::Missing) if existing::left_nothing(db) => Legacy {
            authority: Observation::Absent,
            host: Observation::Absent,
        },
        _ => UNVERIFIED,
    })
    .unwrap_or(UNVERIFIED)
}

/// The host `conn` commits, its chunks read by their exact names into one zeroizing buffer, decoded
/// into a zeroizing string and checked as a host, then dropped. No committed host is `Absent`; a
/// missing, short or unreadable chunk or a malformed host is not.
fn host(conn: &Connection, store: &impl CredentialStore) -> Observation {
    let Ok(head) = current(conn, Kind::Host) else {
        return Observation::Unverified;
    };
    let Some(active) = head.active else {
        return Observation::Absent;
    };
    let Ok(bytes) = io::read_active(store, CREDENTIAL_SERVICE, &active) else {
        return Observation::Unverified;
    };
    match decode::<Zeroizing<String>>(&bytes, 128) {
        Ok(host) if valid_host(&host) => Observation::Matching,
        _ => Observation::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use kumochi_protocol::Secret;
    use kumochi_protocol::constants::generation_target;

    use super::super::{Authority, FILE};
    use super::*;
    use crate::connections::vault::fake::FakeStore;
    use crate::test_support::{Scratch, files};

    const HOST: &[u8] = b"\"urn:uuid:01234567-89ab-4cde-8f01-23456789abcd\"";
    /// As long as `HOST`, but a version 5 UUID.
    const NOT_A_HOST: &[u8] = b"\"urn:uuid:01234567-89ab-5cde-8f01-23456789abcd\"";
    const MATCHING: Legacy = Legacy {
        authority: Observation::Matching,
        host: Observation::Matching,
    };

    /// Puts a change to a host chunk, named by the second argument, into the fixture's store.
    type Change = fn(&Fixture, &str);

    struct Fixture {
        _locks: Scratch,
        lock: SessionLock,
        scratch: Scratch,
        store: FakeStore,
    }

    impl Fixture {
        /// An empty authority folder, with the session lock in its own folder.
        fn new(name: &str) -> Self {
            let locks = Scratch::new(&format!("{name}-lock"));
            fs::create_dir_all(locks.dir()).expect("lock folder");
            let lock = SessionLock::try_acquire(&locks.dir().join("session.lock")).expect("lock");
            let scratch = Scratch::new(name);
            fs::create_dir_all(scratch.dir()).expect("authority folder");
            Self {
                _locks: locks,
                lock,
                scratch,
                store: FakeStore::default(),
            }
        }

        fn db(&self) -> PathBuf {
            self.scratch.dir().join(FILE)
        }

        fn raw(&self, sql: &str) {
            Connection::open(self.db())
                .and_then(|conn| conn.execute_batch(sql))
                .expect("change the authority");
        }

        /// A closed current authority, committing `host` if given; returns the host's names.
        fn authority(&self, host: Option<&[u8]>) -> Vec<String> {
            let mut authority = Authority::open(&self.db(), &self.lock).expect("open authority");
            let Some(host) = host else {
                return Vec::new();
            };
            let plan = authority
                .begin(Kind::Host, 0, host.len())
                .expect("begin host");
            let secret = Secret::new(Zeroizing::new(host.to_vec()));
            let verified =
                io::write(&self.store, CREDENTIAL_SERVICE, plan, &secret).expect("write host");
            let (active, _) = authority.activate(verified, None).expect("activate host");
            self.store.0.lock().expect("fake lock").mutations.clear();
            (0..active.layout.chunks())
                .map(|index| {
                    generation_target(CREDENTIAL_SERVICE, "host", &active.generation.hex(), index)
                })
                .collect()
        }

        /// Reads, and checks that neither the folder nor the store changed.
        fn read(&self) -> Legacy {
            let before = files(self.scratch.dir());
            let values = self.store.snapshot();
            let read = legacy(&self.db(), &self.lock, &self.store);
            assert_eq!(files(self.scratch.dir()), before, "the folder changed");
            assert_eq!(self.store.snapshot(), values, "the store changed");
            assert!(self.store.0.lock().expect("fake lock").mutations.is_empty());
            read
        }

        fn put(&self, name: &str, bytes: &[u8]) {
            self.store
                .0
                .lock()
                .expect("fake lock")
                .values
                .insert(name.to_owned(), Secret::new(Zeroizing::new(bytes.to_vec())));
        }
    }

    #[test]
    fn a_current_authority_and_its_host_match_whether_fresh_or_migrated() {
        let fresh = Fixture::new("legacy-authority-fresh");
        fresh.authority(Some(HOST));
        assert_eq!(fresh.read(), MATCHING);
        for (case, sql) in [
            (
                "v1",
                "ALTER TABLE control DROP COLUMN legacy_done;
                 ALTER TABLE control DROP COLUMN legacy_deleted_in;
                 PRAGMA user_version = 1;",
            ),
            ("v2", "PRAGMA user_version = 2;"),
        ] {
            let migrated = Fixture::new(&format!("legacy-authority-{case}"));
            migrated.authority(Some(HOST));
            migrated.raw(sql);
            assert_eq!(migrated.read(), UNVERIFIED, "{case} before its migration");
            drop(Authority::open(&migrated.db(), &migrated.lock).expect("migrate"));
            assert_eq!(migrated.read(), MATCHING, "{case} after its migration");
        }
    }

    #[test]
    fn every_other_version_is_unverified() {
        for version in [0, 1, 2, 4, 5] {
            let fixture = Fixture::new(&format!("legacy-authority-version-{version}"));
            fixture.authority(Some(HOST));
            fixture.raw(&format!("PRAGMA user_version = {version}"));
            assert_eq!(fixture.read(), UNVERIFIED, "v{version}");
        }
    }

    #[test]
    fn a_changed_shape_or_a_bad_row_is_unverified() {
        let without_host = [
            (
                "no-reference",
                "PRAGMA writable_schema = ON;
                 UPDATE sqlite_schema SET sql = replace(sql, ' REFERENCES heads (kind)', '')
                 WHERE name = 'generations';",
            ),
            (
                "moved-unique",
                "PRAGMA writable_schema = ON;
                 UPDATE sqlite_schema SET sql = replace(replace(sql,
                     'generation BLOB NOT NULL UNIQUE', 'generation BLOB NOT NULL'),
                     'kind TEXT NOT NULL REFERENCES', 'kind TEXT NOT NULL UNIQUE REFERENCES')
                 WHERE name = 'generations';",
            ),
            (
                "wrong-control-id",
                "PRAGMA ignore_check_constraints = 1; UPDATE control SET id = 2;",
            ),
            (
                "second-control-row",
                "PRAGMA ignore_check_constraints = 1;
                 INSERT INTO control (id, lifetime, marker) SELECT 2, lifetime, marker FROM control;",
            ),
            (
                "spent-lifetime",
                "UPDATE control SET lifetime = 9223372036854775807",
            ),
        ];
        for (case, sql) in without_host {
            let fixture = Fixture::new(&format!("legacy-authority-{case}"));
            fixture.authority(None);
            fixture.raw(sql);
            assert_eq!(fixture.read(), UNVERIFIED, "{case}");
        }
        for (case, sql) in [
            ("extra-table", "CREATE TABLE extra (x INTEGER)"),
            ("extra-index", "CREATE INDEX extra ON generations (kind)"),
            ("dropped-index", "DROP INDEX one_active"),
            (
                "extra-column",
                "ALTER TABLE control ADD COLUMN extra INTEGER",
            ),
            ("no-control", "DELETE FROM control"),
            ("no-head", "DELETE FROM heads WHERE kind = 'github'"),
            (
                "bad-layout",
                "UPDATE generations SET chunks = 2 WHERE state = 'active'",
            ),
            (
                "swapped-index",
                "PRAGMA writable_schema = ON;
                 UPDATE sqlite_schema SET rootpage = (SELECT max(rootpage) + min(rootpage)
                     FROM sqlite_schema WHERE name IN ('one_active', 'one_pending')) - rootpage
                 WHERE name IN ('one_active', 'one_pending');",
            ),
            (
                "orphan",
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO generations (generation, kind, state, chunks, bytes)
                 VALUES (zeroblob(16), 'other', 'removed', 1, 1)",
            ),
        ] {
            let fixture = Fixture::new(&format!("legacy-authority-{case}"));
            fixture.authority(Some(HOST));
            fixture.raw(sql);
            assert_eq!(fixture.read(), UNVERIFIED, "{case}");
        }
    }

    #[test]
    fn the_host_matches_only_when_every_chunk_reads_back_as_a_host() {
        let none = Fixture::new("legacy-host-none");
        none.authority(None);
        assert_eq!(
            none.read(),
            Legacy {
                authority: Observation::Matching,
                host: Observation::Absent,
            }
        );
        assert_eq!(NOT_A_HOST.len(), HOST.len());
        let cases: [(&str, Change); 4] = [
            ("missing", |fixture, name| {
                fixture
                    .store
                    .0
                    .lock()
                    .expect("fake lock")
                    .values
                    .remove(name);
            }),
            ("short", |fixture, name| fixture.put(name, &HOST[1..])),
            ("not-a-host", |fixture, name| fixture.put(name, NOT_A_HOST)),
            ("refused", |fixture, name| {
                fixture.store.0.lock().expect("fake lock").denied_read = Some(name.to_owned());
            }),
        ];
        for (case, change) in cases {
            let fixture = Fixture::new(&format!("legacy-host-{case}"));
            let names = fixture.authority(Some(HOST));
            assert_eq!(names.len(), 1, "{case}");
            change(&fixture, &names[0]);
            assert_eq!(
                fixture.read(),
                Legacy {
                    authority: Observation::Matching,
                    host: Observation::Unverified,
                },
                "{case}"
            );
        }
    }

    #[test]
    fn a_missing_authority_is_absent_only_alone_in_its_own_folder() {
        let bare = Fixture::new("legacy-authority-bare");
        assert_eq!(
            bare.read(),
            Legacy {
                authority: Observation::Absent,
                host: Observation::Absent,
            }
        );
        assert!(files(bare.scratch.dir()).is_empty());
        let gone = Fixture::new("legacy-authority-gone");
        let db = gone.scratch.dir().join("gone").join(FILE);
        assert_eq!(legacy(&db, &gone.lock, &gone.store), UNVERIFIED);
        assert!(!gone.scratch.dir().join("gone").exists());
        for (case, left) in [
            "-wal",
            "-shm",
            "-journal",
            ".purge-old",
            ".purge-snapshot",
            ".purge-clean-wal",
        ]
        .into_iter()
        .enumerate()
        {
            let fixture = Fixture::new(&format!("legacy-authority-left-{case}"));
            fs::write(fixture.scratch.dir().join(format!("{FILE}{left}")), b"left")
                .expect("left file");
            assert_eq!(fixture.read(), UNVERIFIED, "{left}");
        }
    }

    #[test]
    fn a_folder_renamed_or_swapped_during_the_read_is_unverified() {
        for swap in [false, true] {
            let fixture = Fixture::new(&format!("legacy-authority-moved-{swap}"));
            let dir = fixture.scratch.dir().to_owned();
            let moved = dir.with_file_name(format!("legacy-authority-moved-{swap}-away"));
            let (from, to) = (dir.clone(), moved.clone());
            existing::between_folder_checks(move || {
                fs::rename(&from, &to).expect("move the folder");
                if swap {
                    fs::create_dir(&from).expect("a new folder at the same path");
                }
            });
            assert_eq!(
                legacy(&fixture.db(), &fixture.lock, &fixture.store),
                UNVERIFIED,
                "{swap}"
            );
            assert!(files(&moved).is_empty(), "{swap}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_folder_is_unverified_even_holding_a_current_authority() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("legacy-authority-linked");
        fixture.authority(Some(HOST));
        assert_eq!(fixture.read(), MATCHING);
        let link = Fixture::new("legacy-authority-link");
        let linked = link.scratch.dir().join("linked");
        symlink(fixture.scratch.dir(), &linked).expect("folder link");
        let before = files(fixture.scratch.dir());
        assert_eq!(
            legacy(&linked.join(FILE), &fixture.lock, &fixture.store),
            UNVERIFIED
        );
        assert_eq!(files(fixture.scratch.dir()), before);
    }
}
