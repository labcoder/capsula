use std::time::Duration;

use toml::Table;

use super::{GITHUB, GitHubTuning};
use crate::test_support::Captured;

const DOCS: &str = include_str!("../../../../docs/user/tuning.md");

fn tuned(text: &str) -> GitHubTuning {
    GitHubTuning::from_table(&text.parse::<Table>().expect("fixture parses"))
}

fn github(
    filing_s: u64,
    filing_requests: usize,
    listing_s: u64,
    listing_requests: usize,
) -> GitHubTuning {
    GitHubTuning {
        filing_deadline: Duration::from_secs(filing_s),
        filing_requests,
        listing_budget: Duration::from_secs(listing_s),
        listing_requests,
    }
}

#[test]
fn the_defaults_are_todays_budgets_inside_their_ranges() {
    assert_eq!(GitHubTuning::default(), github(60, 16, 60, 12));
    for limit in &GITHUB {
        assert!(
            (limit.min..=limit.max).contains(&limit.default),
            "{}",
            limit.key
        );
    }
    assert_eq!(tuned(""), GitHubTuning::default());
    assert_eq!(
        tuned("[tuning]\nvoice = { quiet_ms = 1 }"),
        GitHubTuning::default()
    );
    assert_eq!(tuned("[tuning.github]"), GitHubTuning::default());
}

#[test]
fn values_at_either_edge_are_taken_and_one_past_is_clamped_to_the_edge() {
    let low = "[tuning.github]\nfiling_s = 20\nfiling_requests_count = 12\nlisting_s = 20\nlisting_requests_count = 4";
    assert_eq!(tuned(low), github(20, 12, 20, 4));
    let past = "[tuning.github]\nfiling_s = 19\nfiling_requests_count = 11\nlisting_s = 19\nlisting_requests_count = 3";
    assert_eq!(tuned(past), github(20, 12, 20, 4));
    let high = "[tuning.github]\nfiling_s = 60\nfiling_requests_count = 16\nlisting_s = 60\nlisting_requests_count = 12";
    assert_eq!(tuned(high), github(60, 16, 60, 12));
    let past = "[tuning.github]\nfiling_s = 61\nfiling_requests_count = 17\nlisting_s = 61\nlisting_requests_count = 13";
    assert_eq!(tuned(past), github(60, 16, 60, 12));
}

#[test]
fn a_value_that_isnt_a_whole_number_uses_its_own_default() {
    for bad in ["\"30\"", "30.0", "-1", "true", "[30]"] {
        let text = format!(
            "[tuning.github]\nfiling_s = {bad}\nlisting_s = 30\nlisting_requests_count = 8"
        );
        assert_eq!(tuned(&text), github(60, 16, 30, 8), "{bad}");
    }
}

#[test]
fn a_group_whose_values_dont_fit_together_uses_all_its_defaults() {
    for text in [
        "[tuning.github]\nfiling_s = 30\nfiling_requests_count = 12\nlisting_s = 30\nlisting_requests_count = 9",
        "[tuning.github]\nfiling_s = 30\nlisting_s = 31",
    ] {
        assert_eq!(tuned(text), GitHubTuning::default(), "{text}");
    }
    let fits = "[tuning.github]\nfiling_s = 30\nfiling_requests_count = 12\nlisting_s = 30\nlisting_requests_count = 8";
    assert_eq!(tuned(fits), github(30, 12, 30, 8));
}

#[test]
fn a_tuning_key_or_group_that_isnt_a_table_reads_as_the_defaults() {
    assert_eq!(tuned("tuning = 5"), GitHubTuning::default());
    assert_eq!(
        tuned("[tuning]\ngithub = \"fast\""),
        GitHubTuning::default()
    );
}

#[test]
fn logs_name_owned_keys_and_only_count_unknown_ones() {
    let captured = Captured::default();
    let tuning = {
        let _capturing = captured.install();
        tuned(
            "[tuning.github]\nlisting_s = 5\nfiling_requests_count = \"x\"\nghp_SENTINELKEY = \"ghp_SENTINELVALUE\"",
        )
    };
    assert_eq!(tuning, github(60, 16, 20, 12));
    let logs = captured.text();
    assert!(logs.contains("key=\"listing_s\""), "{logs}");
    assert!(logs.contains("key=\"filing_requests_count\""), "{logs}");
    assert!(logs.contains("unknown=1"), "{logs}");
    assert!(!logs.contains("SENTINEL"), "{logs}");
}

#[test]
fn the_user_docs_list_every_key_with_its_default_and_range() {
    let block = DOCS
        .split("```toml")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("docs/user/tuning.md has a toml example");
    let table: Table = block.parse().expect("the example parses");
    let github = table["tuning"]["github"]
        .as_table()
        .expect("the example has [tuning.github]");
    assert_eq!(github.len(), GITHUB.len());
    let lines: Vec<&str> = block.lines().map(str::trim).collect();
    for limit in &GITHUB {
        assert_eq!(
            github[limit.key].as_integer(),
            i64::try_from(limit.default).ok(),
            "{}",
            limit.key
        );
        let at = lines
            .iter()
            .position(|line| line.starts_with(&format!("{} = ", limit.key)))
            .expect("each key has its own line");
        let range = format!("{}–{}", limit.min, limit.max);
        assert!(
            lines[at - 1].starts_with('#') && lines[at - 1].contains(&range),
            "{}",
            limit.key
        );
    }
    assert_eq!(tuned(block), GitHubTuning::default());
}
