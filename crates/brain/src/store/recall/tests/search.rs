use super::*;

#[test]
fn activation_ranks_unequal_scores_and_distinguishes_tasks_from_hits() {
    let (_scratch, store) = world("recall-ranked-titles");
    task(&store, 2, "same title");
    say(&store, 10, 2, "user", "alpha beta");
    say(&store, 20, 2, "user", "alpha filler");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "alpha beta".into(),
            task: None,
        },
    )
    .unwrap();
    assert_eq!(
        selected,
        Selection::Search {
            query: "alpha beta".into(),
            hits: vec![Hit { task: 2, event: 10 }, Hit { task: 2, event: 20 }],
            more: false
        }
    );
    assert_eq!(
        render(&store, Origin::Kumo, &selected).0["duplicate_titles"],
        false
    );
    task(&store, 3, "same title");
    say(&store, 30, 3, "user", "alpha beta");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "alpha beta".into(),
            task: None,
        },
    )
    .unwrap();
    assert_eq!(
        render(&store, Origin::Kumo, &selected).0["duplicate_titles"],
        true
    );
}

#[test]
fn activation_prefix_is_maximal_at_mixed_width_boundaries() {
    for (text, cap, bytes) in [
        ("a€b", 4, 4),
        ("bba界界😀é界a", 18, 18),
        ("😀é界a", 6, 6),
        ("界", 0, 0),
    ] {
        assert_eq!(
            prefix(text, |prefix| prefix.len() <= cap),
            bytes,
            "text {text}, cap {cap}"
        );
    }
}

#[test]
fn search_remembers_an_eleventh_hit_and_does_not_substitute_after_append() {
    let (_scratch, store) = world("recall-more");
    task(&store, 2, "B");
    for id in 1..=10 {
        say(&store, id, 2, "user", "needle");
    }
    let ten = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "needle".into(),
            task: None,
        },
    )
    .unwrap();
    assert_eq!(
        ten,
        Selection::Search {
            query: "needle".into(),
            hits: (1..=10).rev().map(|event| Hit { task: 2, event }).collect(),
            more: false
        }
    );
    say(&store, 11, 2, "user", "needle");
    let eleven = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "needle".into(),
            task: None,
        },
    )
    .unwrap();
    assert_eq!(
        eleven,
        Selection::Search {
            query: "needle".into(),
            hits: (2..=11).rev().map(|event| Hit { task: 2, event }).collect(),
            more: true
        }
    );
    let before = render(&store, Origin::Kumo, &eleven);
    assert_eq!(before.0["note"], MORE);
    say(&store, 12, 2, "user", "needle");
    assert_eq!(render(&store, Origin::Kumo, &eleven), before);
    assert_eq!(render(&store, Origin::Kumo, &ten).0["note"], Value::Null);
}

#[test]
fn or_search_matches_late_text_and_sql_independently_limits_reach_and_kind() {
    let (_scratch, store) = world("recall-search");
    task(&store, 2, "B private title");
    task(&store, 3, "C title");
    task(&store, 4, "hidden");
    say(&store, 10, 2, "user", "needle");
    say(&store, 20, 3, "user", "needle");
    say(&store, 30, 2, "tool_result", "needle");
    say(&store, 31, 2, "summary", "needle");
    say(&store, 32, 2, "task_deleted", "needle");
    say(&store, 40, 1, "user", "needle");
    say(&store, 50, 4, "assistant_text", "needle");
    let direct = Snapshot::begin(&store.conn)
        .unwrap()
        .recall_events(2, 1, 100, Origin::Task(ThreadId(3)), READ_EVENTS)
        .unwrap();
    assert_eq!(
        direct.iter().map(|row| row.id).collect::<Vec<_>>(),
        Vec::<u64>::new()
    );
    store
        .conn
        .execute("UPDATE threads SET state='deleting' WHERE id=4", [])
        .unwrap();
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Search {
                query: "needle".into(),
                task: None
            }
        )
        .unwrap(),
        Selection::Search {
            query: "needle".into(),
            hits: vec![Hit { task: 3, event: 20 }, Hit { task: 2, event: 10 }],
            more: false
        }
    );
    assert_eq!(
        select(
            &store,
            Origin::Task(ThreadId(3)),
            &Query::Search {
                query: "needle".into(),
                task: None
            }
        )
        .unwrap(),
        Selection::Search {
            query: "needle".into(),
            hits: vec![Hit { task: 3, event: 20 }],
            more: false
        }
    );
    assert_eq!(
        select(
            &store,
            Origin::Task(ThreadId(3)),
            &Query::Read {
                task: 2,
                from: 1,
                to: 100
            }
        ),
        Err(Error::NotHere)
    );
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 4,
                from: 1,
                to: 100
            }
        ),
        Err(Error::NoTask)
    );
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Search {
                query: "needle".into(),
                task: Some(99)
            }
        ),
        Err(Error::NoTask)
    );
}

#[test]
fn or_search_matches_late_text_not_just_a_prefix() {
    let (_scratch, store) = world("recall-late-snippet");
    task(&store, 2, "B");
    say(
        &store,
        60,
        2,
        "assistant_text",
        &format!("{} latefact private-sentinel", "padding ".repeat(1000)),
    );
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "absent latefact".into(),
            task: Some(2),
        },
    )
    .unwrap();
    assert_eq!(
        selected,
        Selection::Search {
            query: "absent latefact".into(),
            hits: vec![Hit { task: 2, event: 60 }],
            more: false
        }
    );
    let (shown, sources) = render(&store, Origin::Kumo, &selected);
    assert!(
        shown["quoted_history"][0]["text"]
            .as_str()
            .unwrap()
            .contains("latefact private-sentinel")
    );
    assert!(
        !serde_json::to_string(&selected)
            .unwrap()
            .contains("private-sentinel")
    );
    assert_eq!(sources, BTreeSet::from([ThreadId(2)]));
}

#[test]
fn snippet_byte_caps_cover_cjk_one_long_token_and_escaping() {
    let (_scratch, store) = world("recall-snippet-caps");
    task(&store, 2, "B");
    let cases = [
        ("界 ".repeat(16000), "界"),
        (format!("needle {}", "x".repeat(40 * 1024)), "needle"),
        ("word \"\\\u{1} ".repeat(2000), "word"),
    ];
    for (at, (text, query)) in cases.iter().enumerate() {
        let id = u64::try_from(at).unwrap() + 10;
        say(&store, id, 2, "user", text);
        let selected = Selection::Search {
            query: (*query).into(),
            hits: vec![Hit { task: 2, event: id }],
            more: false,
        };
        let (shown, _) = render(&store, Origin::Kumo, &selected);
        let text = shown["quoted_history"][0]["text"].as_str().unwrap();
        assert!(text.contains(query));
        assert!(json!(text).to_string().len() <= SNIPPET_BYTES);
        if at == 1 {
            assert_eq!(json!(text).to_string().len(), 4096);
        }
    }
    let text = "界".repeat(2000);
    let bytes = prefix(&text, |text| json!(text).to_string().len() <= SNIPPET_BYTES);
    assert_eq!(bytes, 4092);
    assert_eq!(
        prefix("\"\\\n", |text| json!(text).to_string().len() <= 6),
        2
    );
    assert_eq!(prefix("界", |_| false), 0);
    assert_eq!(prefix("", |_| true), 0);
}

#[test]
fn selected_search_survives_deletion_restart_and_rejects_forged_reach_or_owner() {
    let (scratch, mut store) = world("recall-delete");
    task(&store, 2, "B-private-title");
    task(&store, 3, "C-title");
    say(&store, 10, 2, "user", "needle B-private-text");
    say(&store, 20, 3, "user", "needle C-private-text");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Search {
            query: "needle".into(),
            task: None,
        },
    )
    .unwrap();
    assert_eq!(
        render(&store, Origin::Kumo, &selected).1,
        BTreeSet::from([ThreadId(2), ThreadId(3)])
    );
    let (denied, sources) = render(
        &store,
        Origin::Task(ThreadId(3)),
        &Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(10),
            cut: None,
            next: None,
        },
    );
    assert_eq!(
        denied["quoted_history"],
        json!([{"task":2,"note":UNAVAILABLE}])
    );
    assert_eq!(sources, BTreeSet::new());
    let (forged, sources) = render(
        &store,
        Origin::Kumo,
        &Selection::Search {
            query: "needle".into(),
            hits: vec![Hit { task: 2, event: 20 }],
            more: false,
        },
    );
    assert_eq!(
        forged["quoted_history"],
        json!([{"task":2,"note":UNAVAILABLE}])
    );
    assert_eq!(sources, BTreeSet::new());
    assert_eq!(
        store.delete(ThreadId(2)).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    let after = render(&store, Origin::Kumo, &selected);
    assert_eq!(
        after.0["quoted_history"],
        json!([{"task":3,"event":20,"text":"needle C-private-text","title":"C-title"},{"task":2,"note":REMOVED}])
    );
    assert_eq!(after.1, BTreeSet::from([ThreadId(3)]));
    drop(store);
    let mut reopened = Store::open(&scratch.db()).unwrap();
    reopened.recover().unwrap();
    let stored = crate::dispatch::ToolOutput::Recall(selected).stored();
    let crate::dispatch::ToolOutput::Recall(selected) =
        crate::dispatch::ToolOutput::from_stored(&stored).unwrap()
    else {
        panic!("reference result");
    };
    assert_eq!(render(&reopened, Origin::Kumo, &selected), after);
}
