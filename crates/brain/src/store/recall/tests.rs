use super::*;
use crate::store::{RunEnd, Store};
use crate::test_support::Scratch;

#[test]
fn exact_encoded_read_boundary_and_one_over_are_distinguished() {
    let (_scratch, store) = world("recall-exact-bytes");
    task(&store, 2, "B");
    say(&store, 10, 2, "user", &"x".repeat(32671));
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 2,
                from: 10,
                to: 10
            }
        )
        .unwrap(),
        Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(10),
            cut: None,
            next: None
        }
    );
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 10,
            to: 10,
        },
    )
    .unwrap();
    let (shown, _) = render(&store, Origin::Kumo, &selected);
    let mut payload = shown["quoted_history"][0].clone();
    payload.as_object_mut().unwrap().remove("title");
    assert_eq!(payload.to_string().len(), 32768);
    store
        .conn
        .execute("DELETE FROM events WHERE id=10", [])
        .unwrap();
    say(&store, 10, 2, "user", &"x".repeat(32672));
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 10,
            to: 10,
        },
    )
    .unwrap();
    let Selection::Read {
        cut: Some(bytes), ..
    } = selected
    else {
        panic!("one byte over requires a cut");
    };
    assert_eq!(bytes, 32614);
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
fn sqlite_diagnostics_are_sanitized_before_actual_tracing_at_query_and_render_sites() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (_scratch, store) = world("recall-sql-diagnostics");
    task(&store, 2, "B");
    say(&store, 10, 2, "user", "needle");
    let marker = "sentinel_private_sql_diagnostic";
    store.conn.execute_batch("DROP TABLE events; CREATE VIEW events AS SELECT * FROM sentinel_private_sql_diagnostic;").unwrap();
    let raw=store.conn.prepare("SELECT e.thread,e.id FROM events_fts JOIN events e ON e.id=events_fts.rowid WHERE events_fts MATCH 'needle'").unwrap_err();
    assert!(
        raw.to_string().contains(marker),
        "the injected actual SQLite failure carries the marker"
    );
    let log = Log(Arc::new(Mutex::new(Vec::new())));
    let captured = Arc::clone(&log.0);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || log.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let read = Snapshot::begin(&store.conn).unwrap();
        let error = read
            .select_recall(
                Origin::Kumo,
                &Query::Search {
                    query: "needle".into(),
                    task: None,
                },
            )
            .unwrap_err();
        assert_eq!(error, Error::Read);
        tracing::warn!(%error,?error,"a history query failed");
        let error = read
            .render_recall(
                Origin::Kumo,
                &Selection::Search {
                    query: "needle".into(),
                    hits: vec![Hit { task: 2, event: 10 }],
                    more: false,
                },
                &mut BTreeSet::new(),
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Read));
        tracing::warn!(%error,?error,"a context read failed");
    });
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(text.contains("can't read history"));
    assert!(text.contains("can't read the store page"));
    assert!(!text.contains(marker));
    assert!(!text.contains(&format!("{:?}", marker.as_bytes())));
}

fn world(name: &str) -> (Scratch, Store) {
    let scratch = Scratch::new(name);
    let mut store = Store::open(&scratch.db()).unwrap();
    store.recover().unwrap();
    (scratch, store)
}

fn task(store: &Store, id: u64, title: &str) {
    store
        .conn
        .execute(
            "INSERT INTO threads(id,title,created_ms,updated_ms) VALUES (?1,?2,0,?1)",
            params![i64::try_from(id).unwrap(), title],
        )
        .unwrap();
}

fn say(store: &Store, id: u64, task: u64, kind: &str, text: &str) {
    store
        .conn
        .execute(
            "INSERT INTO events(id,thread,kind,text,created_ms) VALUES (?1,?2,?3,?4,0)",
            params![
                i64::try_from(id).unwrap(),
                i64::try_from(task).unwrap(),
                kind,
                text
            ],
        )
        .unwrap();
}

fn select(store: &Store, origin: Origin, query: &Query) -> Result<Selection, Error> {
    Snapshot::begin(&store.conn)
        .unwrap()
        .select_recall(origin, query)
}

fn render(store: &Store, origin: Origin, selection: &Selection) -> (Value, BTreeSet<ThreadId>) {
    let mut sources = BTreeSet::new();
    let text = Snapshot::begin(&store.conn)
        .unwrap()
        .render_recall(origin, selection, &mut sources)
        .unwrap();
    (serde_json::from_str(&text).unwrap(), sources)
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
        .recall_events(2, 1, 100, Origin::Task(ThreadId(3)))
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
fn selected_ranges_keep_actual_continuation_and_do_not_grow_after_append() {
    let (_scratch, store) = world("recall-range");
    task(&store, 2, "B");
    task(&store, 3, "C");
    for at in 1..=21 {
        say(&store, at * 10, 2, "user", &format!("B{at}"));
    }
    say(&store, 15, 3, "user", "foreign-secret");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 10,
            to: 1000,
        },
    )
    .unwrap();
    assert_eq!(
        selected,
        Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(200),
            cut: None,
            next: Some(210)
        }
    );
    let before = render(&store, Origin::Kumo, &selected);
    let encoded_before = Snapshot::begin(&store.conn)
        .unwrap()
        .render_recall(Origin::Kumo, &selected, &mut BTreeSet::new())
        .unwrap();
    assert_eq!(before.0["quoted_history"][0]["next"], 210);
    let events = before.0["quoted_history"][0]["events"].as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (1..=20).map(|at| at * 10).collect::<Vec<_>>()
    );
    assert_eq!(events[0], json!({"event":10,"speaker":"user","text":"B1"}));
    assert_eq!(
        events[19],
        json!({"event":200,"speaker":"user","text":"B20"})
    );
    say(&store, 220, 2, "assistant_text", "appended-private");
    assert_eq!(render(&store, Origin::Kumo, &selected), before);
    let encoded_after = Snapshot::begin(&store.conn)
        .unwrap()
        .render_recall(Origin::Kumo, &selected, &mut BTreeSet::new())
        .unwrap();
    assert_eq!(encoded_after.as_bytes(), encoded_before.as_bytes());
    let empty = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 11,
            to: 19,
        },
    )
    .unwrap();
    assert_eq!(
        empty,
        Selection::Read {
            task: 2,
            first_shown: None,
            last_shown: None,
            cut: None,
            next: None
        }
    );
    let (shown, sources) = render(&store, Origin::Kumo, &empty);
    assert_eq!(shown["quoted_history"][0]["events"], json!([]));
    assert_eq!(sources, BTreeSet::from([ThreadId(2)]));
    let inclusive = select(
        &store,
        Origin::Task(ThreadId(2)),
        &Query::Read {
            task: 2,
            from: 20,
            to: 30,
        },
    )
    .unwrap();
    assert_eq!(
        inclusive,
        Selection::Read {
            task: 2,
            first_shown: Some(20),
            last_shown: Some(30),
            cut: None,
            next: None
        }
    );
}

#[test]
fn oversized_event_cuts_encoded_utf8_and_advances_past_the_cut_event() {
    let (_scratch, store) = world("recall-cut");
    task(&store, 2, "B");
    let text = "界\"\\\u{1}".repeat(12000);
    say(&store, 10, 2, "user", &text);
    say(&store, 30, 2, "user", "after-cut");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 1,
            to: 100,
        },
    )
    .unwrap();
    let Selection::Read {
        cut: Some(bytes),
        next: Some(30),
        first_shown: Some(10),
        last_shown: Some(10),
        ..
    } = selected
    else {
        panic!("a single cut event with the next actual id");
    };
    assert!(text.is_char_boundary(bytes));
    assert_eq!(bytes, 15053);
    let expected=json!({"quoted_history":[{"task":2,"events":[{"event":10,"speaker":"user","text":&text[..15053]}],"cut":true,"next":30,"note":CUT,"title":"B"}],"note":null,"duplicate_titles":false,"truncated":false}).to_string();
    let actual = Snapshot::begin(&store.conn)
        .unwrap()
        .render_recall(Origin::Kumo, &selected, &mut BTreeSet::new())
        .map_err(|error| error.to_string());
    assert_eq!(actual, Ok(expected));
    let (shown, _) = render(&store, Origin::Kumo, &selected);
    let event = &shown["quoted_history"][0]["events"][0];
    assert_eq!(event["text"], &text[..bytes]);
    assert_eq!(shown["quoted_history"][0]["note"], CUT);
    let payload = range(2, std::slice::from_ref(event), Some(bytes), Some(30));
    assert!(payload.to_string().len() <= TEXT_BYTES);
    let next_char = text[bytes..].chars().next().unwrap().len_utf8();
    assert!(
        range(
            2,
            &[json!({"event":10,"speaker":"user","text":&text[..bytes+next_char]})],
            Some(bytes + next_char),
            Some(30)
        )
        .to_string()
        .len()
            > TEXT_BYTES
    );
    store
        .conn
        .execute(
            "UPDATE threads SET title=?1 WHERE id=2",
            [&"renamed".repeat(100)],
        )
        .unwrap();
    let (renamed, _) = render(&store, Origin::Kumo, &selected);
    assert_eq!(
        renamed["quoted_history"][0]["events"],
        shown["quoted_history"][0]["events"]
    );
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 2,
                from: 30,
                to: 100
            }
        )
        .unwrap(),
        Selection::Read {
            task: 2,
            first_shown: Some(30),
            last_shown: Some(30),
            cut: None,
            next: None
        }
    );
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

#[test]
fn task_metadata_is_bounded_ordered_ambiguous_and_derived_from_persisted_runs() {
    let (_scratch, store) = world("recall-tasks");
    assert_eq!(
        select(&store, Origin::Kumo, &Query::Tasks).unwrap(),
        Selection::Tasks {
            tasks: vec![],
            truncated: false
        }
    );
    assert_eq!(
        select(&store, Origin::Task(ThreadId(2)), &Query::Tasks),
        Err(Error::NotHere)
    );
    for id in 2..=101 {
        task(&store, id, "same title");
    }
    assert_eq!(
        select(&store, Origin::Kumo, &Query::Tasks).unwrap(),
        Selection::Tasks {
            tasks: (2..=101).rev().collect(),
            truncated: false
        }
    );
    task(&store, 102, "same title");
    task(&store, 103, "hidden title");
    store
        .conn
        .execute("UPDATE threads SET state='deleting' WHERE id=103", [])
        .unwrap();
    let selected = select(&store, Origin::Kumo, &Query::Tasks).unwrap();
    assert_eq!(
        selected,
        Selection::Tasks {
            tasks: (3..=102).rev().collect(),
            truncated: true
        }
    );
    let (shown, sources) = render(&store, Origin::Kumo, &selected);
    assert_eq!(shown["duplicate_titles"], true);
    assert_eq!(shown["truncated"], true);
    assert_eq!(
        shown["quoted_history"][0],
        json!({"task":102,"title":"same title","kind":"thread","working":false,"outcome":null,"last_use":102})
    );
    assert_eq!(sources, (3..=102).map(ThreadId).collect());
    store
        .conn
        .execute("UPDATE threads SET updated_ms=1000 WHERE id IN (2,102)", [])
        .unwrap();
    assert_eq!(
        select(&store, Origin::Kumo, &Query::Tasks).unwrap(),
        Selection::Tasks {
            tasks: [vec![102, 2], (4..=101).rev().collect()].concat(),
            truncated: true
        }
    );
}

#[test]
fn task_activity_and_outcomes_follow_persisted_runs_after_restart() {
    let (scratch, mut store) = world("recall-task-outcomes");
    task(&store, 2, "B");
    store
        .begin_turn(
            ThreadId(2),
            None,
            kumochi_protocol::message::RunId(1),
            "ask",
        )
        .unwrap();
    let live = Selection::Tasks {
        tasks: vec![2],
        truncated: false,
    };
    let shown = render(&store, Origin::Kumo, &live).0;
    assert_eq!(shown["quoted_history"][0]["working"], true);
    assert_eq!(shown["quoted_history"][0]["outcome"], Value::Null);
    store
        .end_run(
            ThreadId(2),
            kumochi_protocol::message::RunId(1),
            &[],
            RunEnd::Done,
        )
        .unwrap();
    assert_eq!(
        render(&store, Origin::Kumo, &live).0["quoted_history"][0]["outcome"],
        "Done"
    );
    store
        .begin_turn(
            ThreadId(2),
            None,
            kumochi_protocol::message::RunId(2),
            "next",
        )
        .unwrap();
    drop(store);
    let mut reopened = Store::open(&scratch.db()).unwrap();
    assert_eq!(reopened.recover().unwrap().crashed_runs, 1);
    let shown = render(&reopened, Origin::Kumo, &live).0;
    assert_eq!(shown["quoted_history"][0]["working"], false);
    assert_eq!(shown["quoted_history"][0]["outcome"], "Failed");
    reopened
        .begin_turn(
            ThreadId(2),
            None,
            kumochi_protocol::message::RunId(3),
            "older active",
        )
        .unwrap();
    reopened
        .begin_turn(
            ThreadId(2),
            None,
            kumochi_protocol::message::RunId(4),
            "newer stopped",
        )
        .unwrap();
    reopened
        .end_run(
            ThreadId(2),
            kumochi_protocol::message::RunId(4),
            &[],
            RunEnd::Interrupted(crate::store::Interruption::Stop),
        )
        .unwrap();
    let shown = render(&reopened, Origin::Kumo, &live).0;
    assert_eq!(shown["quoted_history"][0]["working"], true);
    assert_eq!(shown["quoted_history"][0]["outcome"], "Stopped");
}

#[test]
fn metadata_byte_limit_and_strict_round_envelope_refuse_bad_inputs() {
    let (_scratch, store) = world("recall-metadata-bytes");
    task(&store, 2, &"x".repeat(32768));
    assert_eq!(
        select(&store, Origin::Kumo, &Query::Tasks).unwrap(),
        Selection::Tasks {
            tasks: vec![],
            truncated: true
        }
    );
    for body in [
        r#"{"call":"x","output":{"recall":{"kind":"tasks","tasks":[2],"tasks":[3],"truncated":false}}}"#,
        r#"{"call":"x","call":"y","output":{"recall":{"kind":"tasks","tasks":[],"truncated":false}}}"#,
        r#"{"call":"x","output":{"recall":{"kind":"tasks","tasks":[],"truncated":false}},"other":true}"#,
    ] {
        assert!(
            matches!(
                crate::store::rounds::result(body),
                Err(StoreError::RoundRecord)
            ),
            "{body}"
        );
    }
}

#[test]
fn forged_read_selectors_cannot_exceed_rendered_event_or_encoded_byte_bounds() {
    let (_scratch, store) = world("recall-forged-ranges");
    task(&store, 2, "B");
    for id in 1..=21 {
        say(&store, id, 2, "user", "small");
    }
    let reference = Selection::Read {
        task: 2,
        first_shown: Some(1),
        last_shown: Some(21),
        cut: None,
        next: None,
    };
    let mut sources = BTreeSet::new();
    let read = Snapshot::begin(&store.conn).unwrap();
    assert!(matches!(
        read.render_recall(Origin::Kumo, &reference, &mut sources),
        Err(StoreError::Read)
    ));
    assert_eq!(sources, BTreeSet::new());
    drop(read);
    say(&store, 30, 2, "user", &"x".repeat(40000));
    let reference = Selection::Read {
        task: 2,
        first_shown: Some(30),
        last_shown: Some(30),
        cut: None,
        next: None,
    };
    assert!(matches!(
        Snapshot::begin(&store.conn)
            .unwrap()
            .render_recall(Origin::Kumo, &reference, &mut sources),
        Err(StoreError::Read)
    ));
    assert_eq!(sources, BTreeSet::new());
}

#[test]
fn a_later_oversized_event_waits_for_the_next_page_before_being_cut() {
    let (_scratch, store) = world("recall-later-cut");
    task(&store, 2, "B");
    say(&store, 10, 2, "user", "whole");
    say(&store, 20, 2, "user", &"x".repeat(40000));
    say(&store, 30, 2, "user", "after");
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 2,
                from: 1,
                to: 1000
            }
        )
        .unwrap(),
        Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(10),
            cut: None,
            next: Some(20)
        }
    );
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 2,
                from: 20,
                to: 1000
            }
        )
        .unwrap(),
        Selection::Read {
            task: 2,
            first_shown: Some(20),
            last_shown: Some(20),
            cut: Some(32616),
            next: Some(30)
        }
    );
}
