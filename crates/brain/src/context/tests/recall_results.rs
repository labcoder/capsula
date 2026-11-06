use super::*;
use crate::dispatch::ToolOutput;
use crate::recall::{Query, Selection};

fn built(world: &World, current: RunId) -> Built {
    world
        .build(
            &snapshot(Origin::Kumo, None),
            (world.main, current),
            Supplied {
                summary: None,
                items: vec![],
            },
            BOUND,
        )
        .unwrap()
}

#[test]
fn native_results_dereference_live_sources_without_retaining_their_text() {
    let mut world = world("recall-native-replay");
    let (b, _) = world.run(None, "needle B-private-sentinel", None);
    let (c, _) = world.run(None, "needle C-private-sentinel", None);
    let main = world.main;
    let current = world.start("find a fact");
    let selected = {
        let read = Snapshot::begin(world.store.conn_for_tests()).unwrap();
        read.select_recall(
            Origin::Kumo,
            &Query::Search {
                query: "needle".into(),
                task: None,
            },
        )
        .unwrap()
    };
    world.store.append_round((main,current), ConnectionTag(7), None,
        &[Opaque::new(br#"{"type":"function_call","call_id":"recall","name":"search_history","arguments":"{}"}"#.to_vec()).unwrap()],
        &[(call("recall"), ToolOutput::Recall(selected))]).unwrap();
    let (stored, searchable): (String, Option<String>) = world
        .store
        .conn_for_tests()
        .query_row(
            "SELECT body,text FROM events WHERE thread=?1 AND kind='tool_result'",
            [i64::try_from(main.0).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(!stored.contains("private-sentinel"));
    assert_eq!(searchable, None);
    let before = built(&world, current);
    assert_eq!(before.sources, vec![main, b, c]);
    assert_eq!(
        before.request.tools.len(),
        0,
        "14b-1 exposes no tool schemas"
    );
    let output = before
        .request
        .input
        .iter()
        .find_map(|item| match item {
            InputItem::FunctionOutput { call: id, output } if *id == call("recall") => Some(output),
            _ => None,
        })
        .unwrap();
    let output: Value = serde_json::from_str(output).unwrap();
    assert_eq!(
        output["quoted_history"],
        json!([
            {"task":c.0,"event":world.events(c)[0].0,"text":"needle C-private-sentinel","title":"Clouds deck"},
            {"task":b.0,"event":world.events(b)[0].0,"text":"needle B-private-sentinel","title":"Clouds deck"}
        ])
    );
    assert_eq!(output["duplicate_titles"], true);
    assert_eq!(
        world.store.delete(b).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    let after = built(&world, current);
    assert_eq!(after.sources, vec![main, c]);
    let output = after
        .request
        .input
        .iter()
        .find_map(|item| match item {
            InputItem::FunctionOutput { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(!output.contains("B-private-sentinel"));
    assert!(output.contains("C-private-sentinel"));
    assert_eq!(
        serde_json::from_str::<Value>(output).unwrap()["quoted_history"][1],
        json!({"task":b.0,"note":REMOVED})
    );
    assert!(!format!("{after:?}").contains("private-sentinel"));
    let retained_output = output.clone();
    let mut reopened = Store::open(&world.scratch.db()).unwrap();
    reopened.recover().unwrap();
    world.store = reopened;
    let next = world.start("after restart");
    let restarted = built(&world, next);
    assert_eq!(restarted.sources, vec![main, c]);
    let output = restarted
        .request
        .input
        .iter()
        .find_map(|item| match item {
            InputItem::FunctionOutput { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert_eq!(output.as_bytes(), retained_output.as_bytes());
}

#[test]
fn metadata_only_native_replay_is_source_fenced_and_other_connections_do_not_replay_it() {
    let mut world = world("recall-native-titles");
    let (b, _) = world.run(None, "private-task-text", None);
    let main = world.main;
    let current = world.start("list tasks");
    world
        .store
        .append_round(
            (main, current),
            ConnectionTag(7),
            None,
            &[Opaque::new(br#"{"type":"function_call"}"#.to_vec()).unwrap()],
            &[(
                call("list"),
                ToolOutput::Recall(Selection::Tasks {
                    tasks: vec![b.0],
                    truncated: false,
                }),
            )],
        )
        .unwrap();
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            Supplied {
                summary: None,
                items: vec![],
            },
            BOUND,
        )
        .unwrap();
    assert_eq!(built.sources, vec![main, b]);
    let output = built
        .request
        .input
        .iter()
        .find_map(|item| match item {
            InputItem::FunctionOutput { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(!output.contains("private-task-text"));
    let mut other = snapshot(Origin::Kumo, None);
    other.connection = ConnectionTag(8);
    let built = world
        .build(
            &other,
            (main, current),
            Supplied {
                summary: None,
                items: vec![],
            },
            BOUND,
        )
        .unwrap();
    assert_eq!(built.sources, vec![main]);
    assert_eq!(
        built.request.input,
        vec![message(
            Role::User,
            &format!("list tasks\n\nLocal time: {TIME}")
        )]
    );
    assert_eq!(
        world.store.delete(b).outcome,
        kumochi_protocol::message::Deleted::Removed
    );
    let built = world
        .build(
            &snapshot(Origin::Kumo, None),
            (main, current),
            Supplied {
                summary: None,
                items: vec![],
            },
            BOUND,
        )
        .unwrap();
    assert_eq!(built.sources, vec![main]);
}
