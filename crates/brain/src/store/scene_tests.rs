use kumochi_protocol::message::{RunId, SceneId, ThreadId};
use rusqlite::params;

use super::super::{Recovered, RunEnd, Store, StoreError};
use super::SceneTarget;
use crate::test_support::{Captured, SENTINEL, Scratch};

/// A new thread with an active run, on the next ids.
fn begin(store: &mut Store) -> (ThreadId, RunId) {
    let (thread, run) = store.high_water().expect("high water");
    let ids = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
    store
        .begin_turn(ids.0, Some("title"), ids.1, "turn")
        .expect("turn");
    ids
}

/// A new thread whose run draws one scene of `labels`, evicting `evict`, then ends.
fn draw(store: &mut Store, labels: &[&str], evict: Option<SceneId>) -> (ThreadId, SceneId) {
    let ids = begin(store);
    let scene = store
        .commit_step(ids, None, SceneTarget::New { evict }, 0, labels[0])
        .expect("first step");
    for (index, label) in (1..).zip(&labels[1..]) {
        store
            .commit_step(ids, None, SceneTarget::Existing(scene), index, label)
            .expect("step");
    }
    store
        .end_run(ids.0, ids.1, &[], RunEnd::Done)
        .expect("ended");
    (ids.0, scene)
}

/// The scenes a restart would recover, oldest first.
fn recovered(scratch: &Scratch) -> Vec<SceneId> {
    let store = Store::open(&scratch.db()).expect("reopen");
    let scenes = store.scenes().expect("scenes");
    scenes.iter().map(|state| state.scene).collect()
}

fn count(store: &Store, kind: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT count(*) FROM events WHERE kind = ?1",
            params![kind],
            |row| row.get(0),
        )
        .expect("count")
}

/// A crash before a transaction commits leaves what a failed one does: SQLite rolls it back.
#[test]
fn a_fifth_scene_and_its_eviction_commit_together_or_not_at_all() {
    let scratch = Scratch::new("fifth-scene");
    let mut store = Store::open(&scratch.db()).expect("store");
    let four: Vec<SceneId> = (0..4).map(|_| draw(&mut store, &["s"], None).1).collect();
    assert_eq!(recovered(&scratch), four);
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER crash BEFORE INSERT ON events WHEN NEW.kind = 'scene_step' BEGIN
                 SELECT RAISE(ABORT, 'crash');
             END;",
        )
        .expect("trap");
    let ids = begin(&mut store);
    let evict = Some(four[0]);
    assert!(matches!(
        store.commit_step(ids, Some("text"), SceneTarget::New { evict }, 0, "fifth"),
        Err(StoreError::Sql(_))
    ));
    assert_eq!(recovered(&scratch), four, "the old four");
    assert_eq!(count(&store, "scene_begin"), 4, "no scene_begin is left");
    assert_eq!(count(&store, "scene_cleared"), 0);
    store
        .conn
        .execute_batch("DROP TRIGGER crash")
        .expect("untrapped");
    let fifth = store
        .commit_step(ids, None, SceneTarget::New { evict }, 0, "fifth")
        .expect("committed");
    assert!(fifth > four[3], "a rolled-back id is never seen");
    assert_eq!(recovered(&scratch), [four[1], four[2], four[3], fifth]);
}

#[test]
fn scenes_cleared_after_an_eviction_never_come_back() {
    let scratch = Scratch::new("evicted");
    let mut store = Store::open(&scratch.db()).expect("store");
    let mut scenes: Vec<SceneId> = (0..4).map(|_| draw(&mut store, &["s"], None).1).collect();
    scenes.push(draw(&mut store, &["e"], Some(scenes[0])).1);
    store.clear_scenes(&scenes[1..]).expect("cleared");
    drop(store);
    assert_eq!(recovered(&scratch), [], "A doesn't come back");
}

#[test]
fn a_clear_commits_whole_or_not_at_all() {
    let scratch = Scratch::new("clear-crash");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (_, a) = draw(&mut store, &["a"], None);
    let (_, b) = draw(&mut store, &["b"], None);
    store
        .conn
        .execute_batch(&format!(
            "CREATE TRIGGER crash BEFORE INSERT ON events
             WHEN NEW.kind = 'scene_cleared' AND json_extract(NEW.body, '$.scene') = {} BEGIN
                 SELECT RAISE(ABORT, 'crash');
             END;",
            b.0
        ))
        .expect("trap");
    assert!(matches!(
        store.clear_scenes(&[a, b]),
        Err(StoreError::Sql(_))
    ));
    assert_eq!(
        recovered(&scratch),
        [a, b],
        "a crash before the commit restores both"
    );
    store
        .conn
        .execute_batch("DROP TRIGGER crash")
        .expect("untrapped");
    store.clear_scenes(&[a, b]).expect("cleared");
    assert_eq!(recovered(&scratch), [], "and one after it restores neither");
}

#[test]
fn a_thread_left_deleting_yields_no_scene() {
    let scratch = Scratch::new("deleting-scene");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (doomed, _) = draw(&mut store, &["doomed"], None);
    let (_, kept) = draw(&mut store, &["kept"], None);
    store.begin_delete(doomed).expect("begin");
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER keep_threads BEFORE DELETE ON threads BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trap");
    assert_eq!(
        store.recover().expect("recover"),
        Recovered {
            failed_deletions: 1,
            ..Recovered::default()
        }
    );
    assert_eq!(recovered(&scratch), [kept]);
    store.clear_scenes(&[kept]).expect("cleared");
    assert_eq!(
        count(&store, "scene_cleared"),
        1,
        "nothing is written for a deleting thread"
    );
}

#[test]
fn a_step_that_doesnt_decode_is_skipped_and_named_by_its_row_alone() {
    let scratch = Scratch::new("malformed-step");
    let mut store = Store::open(&scratch.db()).expect("store");
    let (thread, scene) = draw(&mut store, &["drawn"], None);
    let body = format!(r#"{{"scene":{},"label":"{SENTINEL}"}}"#, scene.0);
    store
        .conn
        .execute(
            "INSERT INTO events (thread, run, kind, text, body, created_ms)
             VALUES (?1, NULL, 'scene_step', ?2, ?3, 0)",
            params![i64::try_from(thread.0).expect("small"), SENTINEL, body],
        )
        .expect("planted");
    let row = store.conn.last_insert_rowid();
    let logs = Captured::default();
    let scenes = {
        let _guard = logs.install();
        store.scenes().expect("scenes")
    };
    assert_eq!(scenes.len(), 1);
    assert_eq!(scenes[0].revision, 1, "the malformed row isn't counted");
    assert_eq!(scenes[0].kept[0].label, "drawn");
    let logs = logs.text();
    assert!(
        logs.contains(&format!("stored scene row {row} doesn't decode")),
        "{logs}"
    );
    assert!(!logs.contains(SENTINEL), "{logs}");
    assert!(
        !format!("{:?}", scenes[0]).contains("drawn"),
        "Debug leaves labels out"
    );
}
