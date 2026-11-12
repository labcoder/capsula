use super::*;
use crate::connections::clock::{Clock, SystemClock};
use crate::connections::github::{api::Tokens, device::Account};
use crate::dispatch::ToolError;
use kumochi_protocol::message::{GitHubLogin, GitHubView, RepoName};
use std::time::Duration;
use zeroize::Zeroizing;

fn user() -> GitHubUser {
    GitHubUser {
        id: 7,
        login: GitHubLogin::new("owner-sentinel").unwrap(),
    }
}

fn repo(name: &str) -> RepoName {
    RepoName::new(name).unwrap()
}

fn default(account: u64, name: &str) -> DefaultRepo {
    DefaultRepo {
        account,
        repo: repo(name),
    }
}

pub(in crate::runtime) fn connected_owner() -> GitHub {
    let now = Instant::now();
    let wall = SystemClock.unix_seconds().expect("usable test wall clock");
    let mut owner = GitHub::with_client(Some("offline-client"));
    owner.execution = Execution::Manual;
    let stamp = owner.machine.stamp();
    owner.machine.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(Account {
                credentials: Tokens {
                    token: Secret::new(
                        GitHubToken::new(Zeroizing::new("ghu_draft_fixture".to_owned())).unwrap(),
                    ),
                    refresh: Secret::new(
                        RefreshToken::new(Zeroizing::new("ghr_draft_fixture".to_owned())).unwrap(),
                    ),
                    expires_at_s: wall + 3_600,
                    refresh_expires_at_s: wall + 86_400,
                },
                user: GitHubUser {
                    id: 7,
                    login: GitHubLogin::new("loaded-owner").unwrap(),
                },
                refresh_sent: false,
            })),
        },
        now,
    );
    owner.machine.on(
        Input::User {
            stamp,
            result: Ok(user()),
        },
        now,
    );
    owner.machine.on(
        Input::Repos {
            generation: owner.machine.stamp().generation,
            result: Ok(Repositories {
                names: kumochi_scene::text::Bounded::new(vec![
                    repo("owner/repo"),
                    repo("owner/other"),
                ])
                .unwrap(),
            }),
        },
        now,
    );
    assert_eq!(owner.machine.user(), Some(&user()));
    assert_eq!(
        owner.machine.repositories(),
        Some([repo("owner/repo"), repo("owner/other")].as_slice())
    );
    owner
}

#[test]
fn an_exact_explicit_repository_wins_over_any_committed_default() {
    let owner = connected_owner();
    let proposed = repo("owner/repo");
    for committed in [
        None,
        Some(default(7, "owner/other")),
        Some(default(8, "owner/missing")),
    ] {
        assert_eq!(
            owner.draft_binding(Some(&proposed), committed),
            Ok((Some(repo("owner/repo")), Some(user())))
        );
    }
}

#[test]
fn missing_or_case_different_proposals_refuse_without_poisoning_a_new_binding() {
    let owner = connected_owner();
    for name in ["owner/missing", "Owner/repo", "owner/Repo"] {
        assert_eq!(
            owner.draft_binding(Some(&repo(name)), Some(default(7, "owner/other"))),
            Err(ToolError::NotHere)
        );
        assert_eq!(
            owner.draft_binding(Some(&repo("owner/repo")), None),
            Ok((Some(repo("owner/repo")), Some(user())))
        );
    }
}

#[test]
fn a_committed_default_requires_the_current_numeric_account_and_exact_membership() {
    let owner = connected_owner();
    for (account, name, expected) in [
        (7, "owner/repo", Some(repo("owner/repo"))),
        (8, "owner/repo", None),
        (7, "owner/missing", None),
        (7, "Owner/repo", None),
        (7, "owner/Repo", None),
    ] {
        assert_eq!(
            owner.draft_binding(None, Some(default(account, name))),
            Ok((expected, Some(user())))
        );
    }
    assert_eq!(owner.draft_binding(None, None), Ok((None, Some(user()))));
}

#[test]
fn a_reducer_default_never_substitutes_for_the_committed_config_snapshot() {
    let mut owner = connected_owner();
    owner.machine.on(
        Input::Op(GitHubOp::SetDefaultRepo(repo("owner/other"))),
        Instant::now(),
    );
    assert_eq!(owner.draft_binding(None, None), Ok((None, Some(user()))));
    owner.machine.on(
        Input::DefaultSaved {
            generation: owner.machine.stamp().generation,
            repo: repo("owner/other"),
            saved: true,
        },
        Instant::now(),
    );
    let GitHubView::Connected {
        default: selected, ..
    } = owner.machine.view(Instant::now())
    else {
        panic!("a current connected view");
    };
    assert_eq!(selected, Some(repo("owner/other")));
    assert_eq!(owner.draft_binding(None, None), Ok((None, Some(user()))));
    assert_eq!(
        owner.draft_binding(None, Some(default(7, "owner/repo"))),
        Ok((Some(repo("owner/repo")), Some(user())))
    );
}

#[test]
fn a_complete_empty_membership_list_is_authority_not_an_incomplete_snapshot() {
    let mut owner = connected_owner();
    owner.machine.on(
        Input::Repos {
            generation: owner.machine.stamp().generation,
            result: Ok(Repositories {
                names: kumochi_scene::text::Bounded::default(),
            }),
        },
        Instant::now(),
    );
    assert_eq!(owner.machine.repositories(), Some([].as_slice()));
    assert_eq!(
        owner.draft_binding(Some(&repo("owner/repo")), None),
        Err(ToolError::NotHere)
    );
    assert_eq!(
        owner.draft_binding(None, Some(default(7, "owner/repo"))),
        Ok((None, Some(user())))
    );
}

#[tokio::test(start_paused = true)]
async fn incomplete_expired_or_inactive_owners_keep_only_the_explicit_proposal() {
    let mut disconnected = connected_owner();
    disconnected
        .machine
        .on(Input::Op(GitHubOp::SignOut), Instant::now());
    assert_eq!(disconnected.machine.user(), None);

    let expired = connected_owner();
    tokio::time::advance(Duration::from_secs(7_200)).await;
    assert_eq!(expired.machine.user(), None);
    assert!(matches!(
        expired.machine.view(Instant::now()),
        GitHubView::Connected { .. }
    ));

    let mut incomplete = connected_owner();
    incomplete
        .machine
        .on(Input::Op(GitHubOp::RefreshRepos), Instant::now());
    assert_eq!(incomplete.machine.user(), Some(&user()));
    assert_eq!(incomplete.machine.repositories(), None);

    let mut failed = connected_owner();
    failed.machine.on(
        Input::Repos {
            generation: failed.machine.stamp().generation,
            result: Err(ApiError::Response),
        },
        Instant::now(),
    );
    assert_eq!(failed.machine.user(), Some(&user()));
    assert_eq!(failed.machine.repositories(), None);

    let mut closing = connected_owner();
    closing.closing = true;
    let mut disabled = connected_owner();
    disabled.enabled = false;
    let proposed = repo("outside/selection");
    for (case, owner) in [
        ("disconnected", disconnected),
        ("expired", expired),
        ("incomplete membership", incomplete),
        ("failed membership", failed),
        ("closing", closing),
        ("disabled", disabled),
    ] {
        assert_eq!(
            owner.draft_binding(Some(&proposed), Some(default(7, "owner/repo"))),
            Ok((Some(repo("outside/selection")), None)),
            "{case}"
        );
        assert_eq!(
            owner.draft_binding(None, Some(default(7, "owner/repo"))),
            Ok((None, None)),
            "{case}"
        );
    }
}
