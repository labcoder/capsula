use serde_json::json;

use super::*;
use crate::dispatch::Then;

const ARGS: &str = r#"{"title":" Keep spaces ","body":"first\r\nsecond","repo":"Owner/Repo"}"#;

fn draft(ledger: &mut Ledger, caller: Caller, args: &str) -> Step {
    let mut run = ctx(ledger, Origin::Task(T), Some(L1));
    run.offered = &[ToolName::DraftIssue];
    call(&mut run, caller, ToolName::DraftIssue, args.as_bytes())
}

#[test]
fn a_draft_is_a_local_write_in_the_callers_thread_without_account_or_consent() {
    let mut ledger = Ledger::default();
    let call = commit(draft(&mut ledger, model("draft"), ARGS));
    assert_eq!(call.thread, T);
    assert_eq!(
        call.key,
        LedgerKey {
            run: R,
            route: Route::Model,
            call: "draft".into(),
        }
    );
    assert_eq!(call.tool, "draft_issue");
    assert_eq!(call.then, Some(Then::IssueDrafted));
    assert_eq!(
        call.output,
        text("Saved a local issue draft for user review. Nothing was filed on GitHub.")
    );
    let LocalWrite::DraftIssue {
        title,
        body,
        repo,
        account,
    } = call.write
    else {
        panic!("the tool may only create a local draft");
    };
    assert_eq!(title.as_str(), " Keep spaces ");
    assert_eq!(body.as_str(), "first\nsecond");
    assert_eq!(
        repo.as_ref()
            .map(kumochi_protocol::message::RepoName::as_str),
        Some("Owner/Repo")
    );
    assert_eq!(account, None);
}

#[test]
fn a_draft_has_no_model_supplied_identity_approval_or_thread() {
    for extra in ["thread", "run", "account", "approval", "hash", "file"] {
        let mut ledger = Ledger::default();
        let args = format!(r#"{{"title":"T","body":"B","{extra}":1}}"#);
        assert_step(
            &draft(&mut ledger, model("bad"), &args),
            &error(ToolError::Invalid),
        );
        assert_step(
            &draft(&mut ledger, model("bad"), ARGS),
            &error(ToolError::Invalid),
        );
    }
}

#[test]
fn malformed_draft_content_and_the_existing_whole_call_cap_are_refused() {
    let too_long = "a".repeat(257);
    let too_large = "a".repeat(MAX_ARGS);
    for (args, refusal) in [
        ("{}".to_owned(), ToolError::Invalid),
        ("[]".to_owned(), ToolError::Invalid),
        (
            r#"{"title":null,"body":"B"}"#.to_owned(),
            ToolError::Invalid,
        ),
        (r#"{"title":"T","body":1}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":" ","body":"B"}"#.to_owned(), ToolError::Invalid),
        (
            r#"{"title":"T\nU","body":"B"}"#.to_owned(),
            ToolError::Invalid,
        ),
        (
            r#"{"title":"T","body":"B","repo":"owner/repo/extra"}"#.to_owned(),
            ToolError::Invalid,
        ),
        (
            r#"{"title":"T","title":"U","body":"B"}"#.to_owned(),
            ToolError::Invalid,
        ),
        (
            format!(r#"{{"title":"{too_long}","body":"B"}}"#),
            ToolError::Invalid,
        ),
        (
            format!(r#"{{"title":"T","body":"{too_large}"}}"#),
            ToolError::TooLarge,
        ),
    ] {
        let mut ledger = Ledger::default();
        assert_step(&draft(&mut ledger, model("bad"), &args), &error(refusal));
    }
    let mut ledger = Ledger::default();
    let call = commit(draft(
        &mut ledger,
        model("empty-body"),
        r#"{"title":"T","body":""}"#,
    ));
    let LocalWrite::DraftIssue { body, repo, .. } = call.write else {
        panic!("local draft")
    };
    assert_eq!(body.as_str(), "");
    assert_eq!(repo, None);
}

#[test]
fn drafting_has_to_be_offered_to_the_run_on_either_route() {
    let mut ledger = Ledger::default();
    assert_step(
        &call(
            &mut ctx(&mut ledger, Origin::Task(T), Some(L1)),
            mcp(L1, "draft"),
            ToolName::DraftIssue,
            br#"{"title":"Local","body":"Draft"}"#,
        ),
        &error(ToolError::NotHere),
    );
    assert_step(
        &call(
            &mut ctx(&mut ledger, Origin::Task(T), None),
            model("unoffered"),
            ToolName::DraftIssue,
            ARGS.as_bytes(),
        ),
        &error(ToolError::NotHere),
    );
    for origin in [Origin::Kumo, Origin::Task(T)] {
        let model = crate::dispatch::model_schemas(origin);
        assert!(model.iter().any(|schema| schema.name == "draft_issue"));
        let mcp = crate::dispatch::mcp_schemas(origin);
        assert!(mcp.iter().any(|schema| schema.name == "draft_issue"));
    }
}

#[test]
fn a_draft_replay_joins_then_returns_the_original_result_without_another_write() {
    let mut ledger = Ledger::default();
    let first = commit(draft(
        &mut ledger,
        model("draft"),
        r#"{"title":"T","body":"B"}"#,
    ));
    let reordered = r#" { "body": "B", "title": "T" } "#;
    let equivalent = r#" { "repo": null, "body": "B", "title": "T" } "#;
    let changed = [
        r#"{"title":"other","body":"B"}"#,
        r#"{"title":"T","body":"other"}"#,
    ];
    assert_step(&draft(&mut ledger, model("draft"), reordered), &Step::Join);
    for args in changed {
        assert_step(
            &draft(&mut ledger, model("draft"), args),
            &error(ToolError::Reused),
        );
    }
    assert_step(&draft(&mut ledger, model("draft"), equivalent), &Step::Join);
    let expected = text("Saved a local issue draft for user review. Nothing was filed on GitHub.");
    assert_eq!(
        ledger.committed(&first.key),
        Some(vec![
            (model("draft"), expected.clone()),
            (model("draft"), expected.clone()),
            (model("draft"), expected.clone()),
        ])
    );
    assert_step(
        &draft(&mut ledger, model("draft"), equivalent),
        &Step::Answer(expected),
    );
    assert_step(
        &draft(&mut ledger, model("draft"), changed[0]),
        &error(ToolError::Reused),
    );
}

#[test]
fn draft_schema_refuses_truncation_duplicate_optional_fields_and_controls() {
    for args in [
        "",
        "{",
        r#"{"title":"T","body":"B""#,
        r#"{"title":"T"}"#,
        r#"{"title":"T","body":null}"#,
        r#"{"title":"T","body":"B","body":"C"}"#,
        r#"{"title":"T","body":"B","repo":null,"repo":"o/r"}"#,
        r#"{"title":"T","body":"B","repo":""}"#,
        r#"{"title":"T","body":"one\rtwo"}"#,
        r#"{"title":"T","body":"one\u0000two"}"#,
        r#"{"title":"one\u2028two","body":"B"}"#,
    ] {
        let mut ledger = Ledger::default();
        assert_step(
            &draft(&mut ledger, model("bad"), args),
            &error(ToolError::Invalid),
        );
        assert_step(
            &draft(&mut ledger, model("bad"), ARGS),
            &error(ToolError::Invalid),
        );
        commit(draft(&mut ledger, model("after"), ARGS));
    }
}

#[test]
fn serialized_argument_cap_counts_keys_escaping_and_the_first_excess_byte() {
    let mut escaped = "\"".repeat(8_180);
    escaped.push('a');
    for mut body in ["a".repeat(16_361), escaped] {
        let mut ledger = Ledger::default();
        let args = serde_json::to_string(&json!({"title":"T","body":&body})).unwrap();
        assert_eq!(args.len(), 16_384);
        let call = commit(draft(&mut ledger, model("at"), &args));
        let LocalWrite::DraftIssue { body: stored, .. } = call.write else {
            panic!("a local draft at the serialized limit");
        };
        assert_eq!(stored.as_str(), body);
        body.push('a');
        let over = serde_json::to_string(&json!({"title":"T","body":body})).unwrap();
        assert_eq!(over.len(), 16_385);
        assert_step(
            &draft(&mut ledger, model("over"), &over),
            &error(ToolError::TooLarge),
        );
        assert_step(
            &draft(&mut ledger, model("over"), ARGS),
            &error(ToolError::TooLarge),
        );
        commit(draft(&mut ledger, model("after"), ARGS));
    }
}

#[test]
fn visible_unicode_and_literal_markdown_are_kept_and_titles_count_scalars() {
    let mut ledger = Ledger::default();
    let title = "Café Ελληνικά العربية שלום 漢字 🐈";
    let body = "@visible [link](https://example.invalid) **literal**\nCafe\u{301}\t☀";
    let args = serde_json::to_string(&json!({"title":title,"body":body})).unwrap();
    let call = commit(draft(&mut ledger, model("unicode"), &args));
    let LocalWrite::DraftIssue {
        title: stored_title,
        body: stored_body,
        ..
    } = call.write
    else {
        panic!("visible Unicode is not an ASCII-only restriction");
    };
    assert_eq!(stored_title.as_str(), title);
    assert_eq!(stored_body.as_str(), body);
    let title = "é".repeat(256);
    let args = serde_json::to_string(&json!({"title":&title,"body":""})).unwrap();
    let call = commit(draft(&mut ledger, model("title-at"), &args));
    let LocalWrite::DraftIssue { title: stored, .. } = call.write else {
        panic!("scalar limit")
    };
    assert_eq!(stored.as_str(), title);
    let args = serde_json::to_string(&json!({"title":"é".repeat(257),"body":""})).unwrap();
    assert_step(
        &draft(&mut ledger, model("title-over"), &args),
        &error(ToolError::Invalid),
    );
}

#[test]
fn hidden_format_authoring_is_refused_in_both_fields_without_poisoning_new_calls() {
    let hidden = [
        '\u{00ad}',
        '\u{034f}',
        '\u{0600}',
        '\u{0605}',
        '\u{061c}',
        '\u{06dd}',
        '\u{070f}',
        '\u{0890}',
        '\u{0891}',
        '\u{08e2}',
        '\u{115f}',
        '\u{1160}',
        '\u{17b4}',
        '\u{17b5}',
        '\u{180b}',
        '\u{180f}',
        '\u{200b}',
        '\u{200f}',
        '\u{2060}',
        '\u{206f}',
        '\u{3164}',
        '\u{fe00}',
        '\u{fe0f}',
        '\u{feff}',
        '\u{ffa0}',
        '\u{fff0}',
        '\u{fffb}',
        '\u{110bd}',
        '\u{110cd}',
        '\u{13430}',
        '\u{1343f}',
        '\u{1bca0}',
        '\u{1bca3}',
        '\u{1d173}',
        '\u{1d17a}',
        '\u{e0000}',
        '\u{e0fff}',
    ];
    for c in hidden
        .into_iter()
        .chain('\u{202a}'..='\u{202e}')
        .chain('\u{2066}'..='\u{2069}')
    {
        for field in ["title", "body"] {
            let mut ledger = Ledger::default();
            let mut args = json!({"title":"visible","body":"visible"});
            args[field] = json!(format!("before{c}after"));
            let args = serde_json::to_string(&args).unwrap();
            assert_step(
                &draft(&mut ledger, model("hidden"), &args),
                &error(ToolError::Invalid),
            );
            assert_step(
                &draft(&mut ledger, model("hidden"), ARGS),
                &error(ToolError::Invalid),
            );
            commit(draft(&mut ledger, model("after"), ARGS));
        }
    }
    for args in [
        r#"{"title":"T\u202e","body":"B"}"#,
        r#"{"title":"T","body":"B\u2066"}"#,
    ] {
        let mut ledger = Ledger::default();
        assert_step(
            &draft(&mut ledger, model("escaped"), args),
            &error(ToolError::Invalid),
        );
    }
}

#[test]
fn the_outer_draft_commit_never_formats_its_content() {
    let marker = "zqx-private-draft";
    let args = format!(r#"{{"title":"{marker}","body":"{marker}","repo":"{marker}/repo"}}"#);
    let mut ledger = Ledger::default();
    let call = commit(draft(&mut ledger, model("draft"), &args));
    let debug = format!("{call:?}");
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
}
