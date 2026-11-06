use super::*;

#[test]
fn words_are_quoted_or_terms_not_an_fts_program() {
    assert_eq!(words("red blue").unwrap(), "\"red\" OR \"blue\"");
    assert_eq!(words("red\"blue OR").unwrap(), "\"red\"\"blue\" OR \"OR\"");
    assert_eq!(
        words(&"x".repeat(1024)).unwrap(),
        format!("\"{}\"", "x".repeat(1024))
    );
    assert_eq!(
        words(&["x"; 32].join(" ")).unwrap(),
        ["\"x\""; 32].join(" OR ")
    );
    for text in [
        "",
        " ",
        "++",
        "ok\0bad",
        "ok\nbad",
        "ok\tbad",
        &"x".repeat(1025),
        &["x"; 33].join(" "),
    ] {
        assert_eq!(words(text), Err(Error::Invalid));
    }
}

#[test]
fn persisted_selectors_reject_malformed_bounds_shapes_and_duplicates() {
    let valid =
        r#"{"recall":{"kind":"read","task":1,"first_shown":4,"last_shown":8,"cut":5,"next":12}}"#;
    assert_eq!(
        Selection::decode(valid),
        Some(Selection::Read {
            task: 1,
            first_shown: Some(4),
            last_shown: Some(8),
            cut: Some(5),
            next: Some(12)
        })
    );
    for (before, after) in [
        ("\"task\":1", "\"task\":0"),
        ("\"task\":1", "\"task\":9223372036854775808"),
        ("\"task\":1", "\"task\":1.0"),
        ("\"task\":1", "\"task\":\"1\""),
        ("\"task\":1", "\"task\":-1"),
        ("\"task\":1", "\"task\":1,\"task\":2"),
        ("\"task\":1", "\"task\":1,\"other\":2"),
        ("\"first_shown\":4", "\"first_shown\":9"),
        ("\"first_shown\":4", "\"first_shown\":null"),
        ("\"next\":12", "\"next\":8"),
        ("\"cut\":5", "\"cut\":0"),
        ("\"cut\":5", "\"cut\":32769"),
    ] {
        assert_eq!(
            Selection::decode(&valid.replace(before, after)),
            None,
            "{after}"
        );
    }
    for invalid in [
        "",
        "null",
        "[]",
        "{",
        r#"{"recall":{"kind":"search","query":"x","hits":[{"task":1,"event":2},{"task":2,"event":2}],"more":false}}"#,
        r#"{"recall":{"kind":"search","query":"ok\u0000bad","hits":[],"more":false}}"#,
        r#"{"recall":{"kind":"tasks","tasks":[1,1],"truncated":false}}"#,
    ] {
        assert_eq!(Selection::decode(invalid), None);
    }
    assert_eq!(
        Selection::decode(
            r#"{"recall":{"kind":"read","task":1,"first_shown":null,"last_shown":null,"cut":null,"next":null}}"#
        ),
        Some(Selection::Read {
            task: 1,
            first_shown: None,
            last_shown: None,
            cut: None,
            next: None
        })
    );
    assert_eq!(
        Selection::Read {
            task: 1,
            first_shown: Some(1),
            last_shown: Some(1),
            cut: Some(TEXT_BYTES),
            next: None
        }
        .validate(),
        Ok(())
    );
    for (count, expected) in [(10, Ok(())), (11, Err(Error::Invalid))] {
        assert_eq!(
            Selection::Search {
                query: "x".into(),
                hits: (1..=count).map(|event| Hit { task: 1, event }).collect(),
                more: false
            }
            .validate(),
            expected
        );
    }
    for (count, expected) in [(100, Ok(())), (101, Err(Error::Invalid))] {
        assert_eq!(
            Selection::Tasks {
                tasks: (1..=count).collect(),
                truncated: false
            }
            .validate(),
            expected
        );
    }
}

#[test]
fn query_ids_and_diagnostics_are_content_free() {
    assert!(id(9_223_372_036_854_775_807));
    assert!(!id(9_223_372_036_854_775_808));
    let empty = r#"{"recall":{"kind":"tasks","tasks":[],"truncated":false}}"#;
    let mut exact = format!(
        "{empty}{}",
        " ".repeat(crate::dispatch::MAX_OUTPUT - empty.len())
    );
    assert_eq!(
        Selection::decode(&exact),
        Some(Selection::Tasks {
            tasks: vec![],
            truncated: false
        })
    );
    exact.push(' ');
    assert_eq!(Selection::decode(&exact), None);
    for query in [
        Query::Read {
            task: 0,
            from: 1,
            to: 2,
        },
        Query::Read {
            task: 1,
            from: 2,
            to: 1,
        },
        Query::Search {
            query: "x".into(),
            task: Some(u64::MAX),
        },
    ] {
        assert_eq!(query.validate(), Err(Error::Invalid));
    }
    let marker = "sentinel-private-query";
    let query = Query::Search {
        query: marker.into(),
        task: None,
    };
    let selection = Selection::Search {
        query: marker.into(),
        hits: vec![Hit { task: 1, event: 2 }],
        more: false,
    };
    let output = crate::dispatch::ToolOutput::Recall(selection);
    let stored = output.stored();
    assert_eq!(
        crate::dispatch::ToolOutput::from_stored(&stored),
        Some(output.clone())
    );
    let debug = format!("{query:?} {output:?} {} {:?}", Error::Read, Error::Read);
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
}
