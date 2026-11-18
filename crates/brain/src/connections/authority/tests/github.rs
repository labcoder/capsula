use super::*;

const V2: &str = "
CREATE TABLE heads (
    kind TEXT PRIMARY KEY CHECK (kind IN ('host', 'chatgpt', 'revoke', 'anthropic')),
    revision INTEGER NOT NULL CHECK (revision >= 0)
) STRICT;
INSERT INTO heads (kind, revision) VALUES ('host', 0), ('chatgpt', 0), ('revoke', 0), ('anthropic', 0);
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
CREATE TABLE control (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    lifetime INTEGER NOT NULL CHECK (lifetime >= 0),
    marker TEXT NOT NULL,
    legacy_deleted_in INTEGER,
    legacy_done INTEGER NOT NULL DEFAULT 0 CHECK (legacy_done IN (0, 1))
) STRICT;
PRAGMA user_version = 2;
";

fn old(setup: &Setup, version: i64) {
    let conn = setup.raw();
    conn.execute_batch(V2).unwrap();
    conn.execute(
        "INSERT INTO control(id,lifetime,marker) VALUES(1,0,?1)",
        [serde_json::to_string(&SignOutMarker::default()).unwrap()],
    )
    .unwrap();
    for (i, state) in ["active", "pending", "removed", "swept"]
        .into_iter()
        .enumerate()
    {
        let layout = (state != "swept").then_some(10);
        conn.execute("INSERT INTO generations(generation,kind,state,chunks,bytes) VALUES(?1,'chatgpt',?2,?3,?4)",
            params![vec![u8::try_from(i).unwrap(); 16], state, layout.map(|_| 1), layout]).unwrap();
    }
    conn.execute("UPDATE heads SET revision=12 WHERE kind='chatgpt'", [])
        .unwrap();
    if version == 1 {
        conn.execute_batch("ALTER TABLE control DROP COLUMN legacy_done; ALTER TABLE control DROP COLUMN legacy_deleted_in; PRAGMA user_version=1;").unwrap();
    }
}

fn schema(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn github_migration_matches_fresh_schema_and_preserves_existing_generations() {
    let fresh = Setup::new("github-fresh");
    let fresh_auth = fresh.open();
    let setup = Setup::new("github-v2");
    old(&setup, 2);
    let auth = setup.open();
    assert_eq!(schema(&auth.conn), schema(&fresh_auth.conn));
    assert_eq!(auth.current(Kind::Account).unwrap().revision, 12);
    let states = auth
        .conn
        .prepare("SELECT state FROM generations ORDER BY generation")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(states, ["active", "removed", "removed", "swept"]);
    assert_eq!(
        auth.conn
            .pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn github_can_be_written_removed_and_swept_after_every_supported_upgrade() {
    for version in [0, 1, 2] {
        let setup = Setup::new("github-upgrade");
        if version != 0 {
            old(&setup, version);
        }
        let mut auth = setup.open();
        assert_eq!(
            auth.conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        let active = setup.save(&mut auth, Kind::GitHub, &secret(7, 37));
        assert_eq!(
            io::read(&setup.store, SERVICE, &active)
                .unwrap()
                .expose()
                .as_slice(),
            &[7; 37]
        );
        auth.remove(&[Kind::GitHub], None, None).unwrap();
        let rows = auth.retired(20).unwrap();
        let row = rows.iter().find(|r| r.kind == Kind::GitHub).unwrap();
        assert_eq!(io::sweep(&setup.store, SERVICE, row), Observed::Absent);
        auth.observed(row, Observed::Absent).unwrap();
        drop(auth);
        let mut auth = setup.open();
        let rows = auth.retired(20).unwrap();
        let row = rows.iter().find(|r| r.kind == Kind::GitHub).unwrap();
        auth.observed(row, io::sweep(&setup.store, SERVICE, row))
            .unwrap();
        assert_eq!(auth.current(Kind::GitHub).unwrap().active, None);
        assert!(
            !auth
                .retired(20)
                .unwrap()
                .iter()
                .any(|r| r.kind == Kind::GitHub)
        );
    }
}

#[test]
fn a_failed_foreign_key_check_rolls_back_the_upgrade() {
    let setup = Setup::new("github-foreign-key");
    old(&setup, 2);
    let conn = setup.raw();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute("INSERT INTO generations(generation,kind,state,chunks,bytes) VALUES(?1,'missing','removed',1,1)",[vec![9u8;16]]).unwrap();
    assert!(matches!(
        Authority::open(&setup.path(), &setup.lock),
        Err(AuthorityError::Corrupt)
    ));
    assert_eq!(
        setup
            .raw()
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        setup
            .raw()
            .query_row("SELECT count(*) FROM heads WHERE kind='github'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
}
