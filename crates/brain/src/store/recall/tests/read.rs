use super::*;

#[test]
fn activation_main_thread_is_unavailable_rather_than_a_missing_task() {
    let (_scratch, store) = world("recall-main-unavailable");
    assert_eq!(
        select(
            &store,
            Origin::Kumo,
            &Query::Read {
                task: 1,
                from: 1,
                to: 1
            }
        ),
        Err(Error::NotHere)
    );
    let selected = Selection::Read {
        task: 1,
        first_shown: None,
        last_shown: None,
        cut: None,
        next: None,
    };
    let (value, sources) = render(&store, Origin::Kumo, &selected);
    assert_eq!(
        value["quoted_history"],
        json!([{ "task": 1, "note": UNAVAILABLE }])
    );
    assert_eq!(sources, BTreeSet::new());
}

#[test]
fn activation_read_normalizes_user_and_assistant_speakers() {
    let (_scratch, store) = world("recall-speakers");
    task(&store, 2, "B");
    say(&store, 10, 2, "user", "question");
    say(&store, 20, 2, "assistant_text", "answer");
    let selected = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 10,
            to: 20,
        },
    )
    .unwrap();
    assert_eq!(
        render(&store, Origin::Kumo, &selected).0["quoted_history"][0]["events"],
        json!([
            { "event": 10, "speaker": "user", "text": "question" },
            { "event": 20, "speaker": "assistant", "text": "answer" },
        ])
    );
}

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
#[expect(
    clippy::too_many_lines,
    reason = "one recall selection through continuation and a later append"
)]
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
    let sparse = select(
        &store,
        Origin::Kumo,
        &Query::Read {
            task: 2,
            from: 5,
            to: 35,
        },
    )
    .unwrap();
    assert_eq!(
        sparse,
        Selection::Read {
            task: 2,
            first_shown: Some(10),
            last_shown: Some(30),
            cut: None,
            next: None,
        }
    );
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
