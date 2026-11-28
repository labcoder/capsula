use super::*;

/// Kumo's history never lists, finds or reads a local task, nor replays a selection saved before
/// it was local, while a cloud task beside it is still found.
#[test]
fn kumo_never_recalls_a_local_task() {
    let (_scratch, store) = world("recall-local");
    task(&store, 2, "local secret title");
    task(&store, 3, "cloud");
    say(&store, 10, 2, "user", "marker local secret");
    say(&store, 11, 3, "user", "marker cloud text");
    let read = Query::Read {
        task: 2,
        from: 10,
        to: 10,
    };
    let saved = select(&store, Origin::Kumo, &read).unwrap();
    store
        .conn
        .execute(
            "UPDATE threads SET model='local-model',effort='low',connection='local_chat' WHERE id=2",
            [],
        )
        .unwrap();
    let search = Query::Search {
        query: "marker".to_owned(),
        task: None,
    };
    let found = select(&store, Origin::Kumo, &search).unwrap();
    assert!(
        matches!(&found, Selection::Search { hits, .. } if hits.iter().all(|hit| hit.task == 3)),
        "no hit in the local task"
    );
    let (text, sources) = render(&store, Origin::Kumo, &found);
    assert_eq!(sources, BTreeSet::from([ThreadId(3)]));
    assert!(text.to_string().contains("cloud text"));
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Search {
                query: "marker".to_owned(),
                task: Some(2),
            }
        ),
        Err(Error::NotHere)
    );
    assert_eq!(select(&store, Origin::Kumo, &read), Err(Error::NotHere));
    let tasks = select(&store, Origin::Kumo, &Query::Tasks).unwrap();
    let (listed, sources) = render(&store, Origin::Kumo, &tasks);
    assert_eq!(
        tasks,
        Selection::Tasks {
            tasks: vec![3],
            truncated: false
        }
    );
    assert!(!sources.contains(&ThreadId(2)));
    let (replayed, sources) = render(&store, Origin::Kumo, &saved);
    assert_eq!(sources, BTreeSet::new());
    assert_eq!(
        replayed["quoted_history"][0],
        json!({ "task": 2, "note": UNAVAILABLE })
    );
    for shown in [text, listed, replayed] {
        assert!(!shown.to_string().contains("secret"), "{shown}");
    }
}
