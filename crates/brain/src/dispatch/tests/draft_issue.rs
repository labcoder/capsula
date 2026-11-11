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
    assert_eq!(call.key.run, R);
    assert_eq!(call.tool, "draft_issue");
    assert_eq!(call.then, Some(Then::IssueDrafted));
    assert_eq!(call.output, text("Saved a local issue draft for user review. Nothing was filed on GitHub."));
    let LocalWrite::DraftIssue { title, body, repo, account } = call.write else {
        panic!("the tool may only create a local draft");
    };
    assert_eq!(title.as_str(), " Keep spaces ");
    assert_eq!(body.as_str(), "first\nsecond");
    assert_eq!(repo.as_ref().map(kumochi_protocol::message::RepoName::as_str), Some("Owner/Repo"));
    assert_eq!(account, None);
}

#[test]
fn a_draft_has_no_model_supplied_identity_approval_or_thread() {
    for extra in ["thread", "run", "account", "approval", "hash", "file"] {
        let mut ledger = Ledger::default();
        let args = format!(r#"{{"title":"T","body":"B","{extra}":1}}"#);
        assert_step(&draft(&mut ledger, model("bad"), &args), &error(ToolError::Invalid));
        assert_step(&draft(&mut ledger, model("bad"), ARGS), &error(ToolError::Invalid));
    }
}

#[test]
fn malformed_draft_content_and_the_existing_whole_call_cap_are_refused() {
    let too_long = "a".repeat(257);
    let too_large = "a".repeat(MAX_ARGS);
    for (args, refusal) in [
        (r#"{}"#.to_owned(), ToolError::Invalid),
        (r#"[]"#.to_owned(), ToolError::Invalid),
        (r#"{"title":null,"body":"B"}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":"T","body":1}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":" ","body":"B"}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":"T\nU","body":"B"}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":"T","body":"B","repo":"owner/repo/extra"}"#.to_owned(), ToolError::Invalid),
        (r#"{"title":"T","title":"U","body":"B"}"#.to_owned(), ToolError::Invalid),
        (format!(r#"{{"title":"{too_long}","body":"B"}}"#), ToolError::Invalid),
        (format!(r#"{{"title":"T","body":"{too_large}"}}"#), ToolError::TooLarge),
    ] {
        let mut ledger = Ledger::default();
        assert_step(&draft(&mut ledger, model("bad"), &args), &error(refusal));
    }
    let mut ledger = Ledger::default();
    let call = commit(draft(&mut ledger, model("empty-body"), r#"{"title":"T","body":""}"#));
    let LocalWrite::DraftIssue { body, repo, .. } = call.write else { panic!("local draft") };
    assert_eq!(body.as_str(), "");
    assert_eq!(repo, None);
}

#[test]
fn drafting_is_model_only_and_has_to_be_offered_to_that_run() {
    let mut ledger = Ledger::default();
    assert_step(&draft(&mut ledger, mcp(L1, "draft"), ARGS), &error(ToolError::NotHere));
    assert_step(&call(&mut ctx(&mut ledger, Origin::Task(T), None), model("unoffered"),
        ToolName::DraftIssue, ARGS.as_bytes()), &error(ToolError::NotHere));
    for origin in [Origin::Kumo, Origin::Task(T)] {
        let model = crate::dispatch::model_schemas(origin);
        assert!(model.iter().any(|schema| schema.name == "draft_issue"));
        assert!(!crate::dispatch::mcp_schemas(origin).iter().any(|schema| schema.name == "draft_issue"));
    }
}

#[test]
fn a_draft_replay_joins_then_returns_the_original_result_without_another_write() {
    let mut ledger = Ledger::default();
    let first = commit(draft(&mut ledger, model("draft"), ARGS));
    assert_step(&draft(&mut ledger, model("draft"), ARGS), &Step::Join);
    assert_eq!(ledger.committed(&first.key), Some(vec![(model("draft"), first.output.clone())]));
    assert_step(&draft(&mut ledger, model("draft"), ARGS), &Step::Answer(first.output));
    assert_step(&draft(&mut ledger, model("draft"), r#"{"title":"other","body":"B"}"#), &error(ToolError::Reused));
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
