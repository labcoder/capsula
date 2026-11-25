use super::*;
use kumochi_protocol::message::RunId;

use crate::store::Boundary;

fn tasks_hit(selection: &Selection) -> BTreeSet<u64> {
    let Selection::Search { hits, .. } = selection else {
        panic!("a search: {selection:?}");
    };
    hits.iter().map(|hit| hit.task).collect()
}

/// Thread 2's run 1 with a summary, cut by tag 7, then thread 3's run 2 and thread 2's run 3.
fn cut_world(name: &str) -> (Scratch, Store, Boundary) {
    let (scratch, mut store) = world(name);
    store
        .begin_turn(ThreadId(2), Some("Before"), RunId(1), "alpha before")
        .unwrap();
    store
        .end_run(ThreadId(2), RunId(1), &[], RunEnd::Done)
        .unwrap();
    let ids: [i64; 2] = store
        .conn
        .query_row(
            "SELECT min(id), max(id) FROM events WHERE thread = 2",
            [],
            |row| Ok([row.get(0)?, row.get(1)?]),
        )
        .unwrap();
    let [first, last] = ids.map(|id| kumochi_protocol::message::EventId(row_id(id)));
    assert!(
        store
            .store_summary(
                ThreadId(2),
                crate::store::Generation(0),
                (first, last),
                "old",
                None
            )
            .unwrap()
    );
    let boundary = store.cut_history(7);
    store
        .begin_turn(ThreadId(3), Some("After"), RunId(2), "alpha after")
        .unwrap();
    store
        .end_run(ThreadId(3), RunId(2), &[], RunEnd::Done)
        .unwrap();
    store
        .begin_turn(ThreadId(2), None, RunId(3), "continued")
        .unwrap();
    (scratch, store, boundary)
}

#[test]
fn a_recovered_snapshot_refuses_a_cutoff_that_differs_from_its_row() {
    let (_scratch, store, boundary) = cut_world("recall-recovery-mismatch");
    let (threads, runs) = boundary.bounds();
    assert_eq!((threads, runs), (2, 1));
    for wrong in [
        Boundary::fixture(8, threads, runs),
        Boundary::fixture(7, threads - 1, runs),
        Boundary::fixture(7, threads, runs - 1),
        Boundary::fixture(7, threads + 1, runs),
    ] {
        assert!(matches!(
            Snapshot::begin(&store.conn)
                .unwrap()
                .for_recovery(Some(wrong)),
            Err(StoreError::RecoveryContext)
        ));
    }
}

#[test]
fn a_recovered_snapshot_reads_only_history_after_its_cutoff() {
    let (_scratch, store, boundary) = cut_world("recall-recovery");
    let search = Query::Search {
        query: "alpha".into(),
        task: None,
    };

    let all = Snapshot::begin(&store.conn).unwrap();
    assert_eq!(
        tasks_hit(&all.select_recall(Origin::Kumo, &search).unwrap()),
        BTreeSet::from([2, 3])
    );
    assert!(all.permits_history(ThreadId(2)));
    assert_eq!(
        all.summary(ThreadId(2))
            .unwrap()
            .map(|summary| summary.text),
        Some("old".to_owned())
    );
    assert!(all.recovery_run(RunId(1)).is_ok());
    let everything = all
        .conversation(ThreadId(2), None, usize::MAX, None)
        .unwrap();
    assert_eq!(
        everything
            .iter()
            .map(|item| item.run)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([RunId(1), RunId(3)])
    );
    drop(all);

    let cut = Snapshot::begin(&store.conn)
        .unwrap()
        .for_recovery(Some(boundary))
        .unwrap();
    assert!(!cut.permits_history(ThreadId(2)));
    assert!(matches!(cut.summary(ThreadId(2)), Ok(None)));
    assert!(cut.permits_history(ThreadId(3)));
    assert!(matches!(
        cut.recovery_run(RunId(1)),
        Err(StoreError::RecoveryContext)
    ));
    assert!(cut.recovery_run(RunId(2)).is_ok());
    assert_eq!(
        tasks_hit(&cut.select_recall(Origin::Kumo, &search).unwrap()),
        BTreeSet::from([3])
    );
    assert_eq!(
        cut.select_recall(Origin::Kumo, &Query::Tasks).unwrap(),
        Selection::Tasks {
            tasks: vec![3],
            truncated: false
        }
    );
    assert_eq!(
        cut.select_recall(
            Origin::Kumo,
            &Query::Read {
                task: 2,
                from: 1,
                to: 100
            }
        ),
        Err(Error::NotHere)
    );
    let own = cut
        .conversation(ThreadId(2), None, usize::MAX, Some(RunId(3)))
        .unwrap();
    assert!(!own.is_empty());
    assert!(own.iter().all(|item| item.run == RunId(3)));
}
