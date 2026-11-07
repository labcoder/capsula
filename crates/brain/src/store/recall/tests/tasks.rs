use super::*;

#[test]
fn activation_metadata_exact_encoded_limit_and_one_over_are_distinct() {
    let (_scratch, store) = world("recall-metadata-exact");
    let empty = r#"{"quoted_history":[{"task":2,"title":"","kind":"thread","working":false,"outcome":null,"last_use":2}],"note":null,"duplicate_titles":false,"truncated":false}"#;
    let title = "x".repeat(32768 - empty.len());
    task(&store, 2, &title);
    let exact = select(&store, Origin::Kumo, &Query::Tasks).unwrap();
    assert_eq!(
        exact,
        Selection::Tasks {
            tasks: vec![2],
            truncated: false
        }
    );
    assert_eq!(
        render(&store, Origin::Kumo, &exact).0.to_string().len(),
        32768
    );
    store
        .conn
        .execute(
            "UPDATE threads SET title=?1 WHERE id=2",
            [format!("{title}x")],
        )
        .unwrap();
    assert_eq!(
        select(&store, Origin::Kumo, &Query::Tasks),
        Ok(Selection::Tasks {
            tasks: Vec::new(),
            truncated: true
        })
    );
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
