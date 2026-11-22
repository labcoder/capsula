//! Offline machine tests with a paused clock.
use super::*;
use crate::connections::clock::FixedClock;
use crate::connections::github::RefreshToken;
use kumochi_protocol::message::UserCode;
use zeroize::Zeroizing;

mod authority;
mod rotation;

fn new_machine(default: Option<DefaultRepo>) -> Machine {
    let mut machine = Machine::new(default);
    machine.clock = Arc::new(FixedClock(1_000));
    machine
}

#[test]
fn loading_is_not_a_completed_empty_or_cleanup_load() {
    for result in [
        Ok(None),
        Err(Removed::Clean),
        Err(Removed::Refused),
        Err(Removed::NotDurable),
    ] {
        let mut machine = new_machine(None);
        assert!(machine.is_loading());
        let stamp = machine.stamp();
        machine.on(Input::Loaded { stamp, result }, Instant::now());
        assert!(!machine.is_loading());
    }
    let mut machine = new_machine(None);
    machine.on(
        Input::Loaded {
            stamp: Stamp { generation: 9 },
            result: Ok(None),
        },
        Instant::now(),
    );
    assert!(machine.is_loading());
}

fn bearer() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new("ghu_sentinel".to_owned())).unwrap())
}
fn tokens() -> Tokens {
    Tokens {
        token: bearer(),
        refresh: Secret::new(RefreshToken::new(Zeroizing::new("ghr_sentinel".to_owned())).unwrap()),
        expires_at_s: 4_600,
        refresh_expires_at_s: 87_400,
    }
}
fn account() -> Account {
    Account {
        credentials: tokens(),
        user: user(),
        refresh_sent: false,
    }
}
fn user() -> GitHubUser {
    GitHubUser {
        id: 7,
        login: GitHubLogin::new("sentinel").unwrap(),
    }
}
fn challenge(interval_s: u16, expires_in_s: u16) -> DeviceCode {
    DeviceCode {
        code: Secret::new(DeviceCodeValue::new(Zeroizing::new("sentinel".to_owned())).unwrap()),
        user_code: UserCode::new("ABCD-EFGH").unwrap(),
        expires_in_s,
        interval_s,
    }
}
fn empty() -> Machine {
    let mut m = new_machine(None);
    m.on(
        Input::Loaded {
            stamp: m.stamp(),
            result: Ok(None),
        },
        Instant::now(),
    );
    m
}
fn begin(m: &mut Machine, interval: u16, expires: u16) -> Stamp {
    let effects = m.on(Input::Op(GitHubOp::Connect), Instant::now());
    let stamp = m.stamp();
    assert!(matches!(effects[0], Effect::RequestCode { stamp: s } if s == stamp));
    m.on(
        Input::Code {
            stamp,
            result: Ok(challenge(interval, expires)),
        },
        Instant::now(),
    );
    stamp
}
fn saving(m: &mut Machine) -> Stamp {
    let stamp = begin(m, 5, 900);
    m.on(Input::Tick, Instant::now() + Duration::from_secs(5));
    m.on(
        Input::Poll {
            stamp,
            result: Ok(TokenReply::Token(tokens())),
        },
        Instant::now(),
    );
    let effects = m.on(
        Input::User {
            stamp,
            result: Ok(user()),
        },
        Instant::now(),
    );
    assert!(matches!(effects[0], Effect::Save { stamp: s, .. } if s == stamp));
    stamp
}
fn connected(m: &mut Machine) {
    let stamp = saving(m);
    m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert!(matches!(
        m.view(Instant::now()),
        GitHubView::Connected { .. }
    ));
}
fn polls(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Poll { .. }))
        .count()
}
fn error(effects: &[Effect]) -> ErrorKey {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::Error(e) => Some(e.key()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn slow_down_uses_both_intervals() {
    let mut m = empty();
    let stamp = begin(&mut m, 5, 900);
    let second = m.on(Input::Op(GitHubOp::Connect), Instant::now());
    assert_eq!(m.stamp(), stamp);
    assert_eq!(second.len(), 1);
    for (delay, reply, next) in [
        (5, None, 10),
        (10, None, 15),
        (15, Some(30), 30),
        (30, Some(7), 35),
    ] {
        tokio::time::advance(Duration::from_secs(delay)).await;
        assert_eq!(polls(&m.on(Input::Tick, Instant::now())), 1);
        assert_eq!(polls(&m.on(Input::Tick, Instant::now())), 0);
        let effects = m.on(
            Input::Poll {
                stamp,
                result: Ok(TokenReply::Error {
                    code: TokenError::SlowDown,
                    interval_s: reply,
                }),
            },
            Instant::now(),
        );
        assert!(
            matches!(effects[0], Effect::Wake(at) if at == Instant::now() + Duration::from_secs(next))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_interval_at_the_deadline_expires_without_polling() {
    let mut m = empty();
    begin(&mut m, 900, 900);
    let now = Instant::now();
    assert_eq!(polls(&m.on(Input::Tick, now + Duration::from_secs(899))), 0);
    assert_eq!(
        error(&m.on(Input::Tick, now + Duration::from_mins(15))),
        ErrorKey::SignInTimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn exact_deadline_and_pending_schedule_do_not_dispatch_early_or_twice() {
    let mut m = empty();
    let stamp = begin(&mut m, 5, 10);
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(polls(&m.on(Input::Tick, Instant::now())), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(polls(&m.on(Input::Tick, Instant::now())), 1);
    let effects = m.on(
        Input::Poll {
            stamp,
            result: Ok(TokenReply::Error {
                code: TokenError::AuthorizationPending,
                interval_s: None,
            }),
        },
        Instant::now(),
    );
    assert!(
        matches!(effects[0], Effect::Wake(at) if at == Instant::now() + Duration::from_secs(5))
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    let effects = m.on(Input::Tick, Instant::now());
    assert_eq!(polls(&effects), 0);
    assert_eq!(error(&effects), ErrorKey::SignInTimedOut);
    m.on(
        Input::Poll {
            stamp,
            result: Ok(TokenReply::Token(tokens())),
        },
        Instant::now(),
    );
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::SignedOut { cleanup: None }
    );
}

#[tokio::test(start_paused = true)]
async fn every_terminal_code_has_a_key_and_later_valid_input_cannot_revive_it() {
    for (code, key) in [
        (TokenError::ExpiredToken, ErrorKey::SignInTimedOut),
        (TokenError::AccessDenied, ErrorKey::SignInFailed),
        (
            TokenError::DeviceFlowDisabled,
            ErrorKey::AccountMisconfigured,
        ),
        (TokenError::IncorrectDeviceCode, ErrorKey::SignInFailed),
        (
            TokenError::UnsupportedGrantType,
            ErrorKey::AccountMisconfigured,
        ),
        (
            TokenError::IncorrectClientCredentials,
            ErrorKey::AccountMisconfigured,
        ),
        (TokenError::Other, ErrorKey::SignInFailed),
    ] {
        let mut m = empty();
        let stamp = begin(&mut m, 1, 900);
        tokio::time::advance(Duration::from_secs(1)).await;
        m.on(Input::Tick, Instant::now());
        assert_eq!(
            error(&m.on(
                Input::Poll {
                    stamp,
                    result: Ok(TokenReply::Error {
                        code,
                        interval_s: None
                    })
                },
                Instant::now()
            )),
            key
        );
        let effects = m.on(
            Input::Poll {
                stamp,
                result: Ok(TokenReply::Token(tokens())),
            },
            Instant::now(),
        );
        assert_eq!(effects.len(), 1);
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::SignedOut { cleanup: None }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn sign_out_and_cancel_fence_every_live_attempt_and_all_late_completions() {
    for sign_out in [false, true] {
        for stage in 0..if sign_out { 8 } else { 4 } {
            let mut m = empty();
            match stage {
                0 => {
                    m.on(Input::Op(GitHubOp::Connect), Instant::now());
                }
                1 => {
                    begin(&mut m, 1, 900);
                }
                2 => {
                    verifying(&mut m);
                }
                3 => {
                    saving(&mut m);
                }
                5 => connected(&mut m),
                6 => {
                    connected(&mut m);
                    m.on(
                        Input::Unauthorized {
                            generation: m.stamp().generation,
                        },
                        Instant::now(),
                    );
                }
                7 => m = new_machine(None),
                _ => {}
            }
            let stamp = m.stamp();
            let op = if sign_out {
                GitHubOp::SignOut
            } else {
                GitHubOp::CancelConnect
            };
            let effects = m.on(Input::Op(op), Instant::now());
            let generation = stamp.generation + 1;
            let remove = sign_out || stage == 3;
            let expected = GitHubView::SignedOut {
                cleanup: remove.then_some(Cleanup::Running),
            };
            assert_eq!(m.fence().load(Ordering::Acquire), generation);
            if remove {
                assert!(
                    matches!(
                        effects.as_slice(),
                        [Effect::CancelAll, Effect::Remove { generation: removed }, Effect::View(view)]
                        if *removed == generation && *view == expected
                    ),
                    "stage {stage}: {effects:?}"
                );
            } else {
                assert!(
                    matches!(
                        effects.as_slice(),
                        [Effect::CancelAll, Effect::View(view)] if *view == expected
                    ),
                    "stage {stage}: {effects:?}"
                );
            }
            for late in [
                Input::Code {
                    stamp,
                    result: Ok(challenge(1, 900)),
                },
                Input::Poll {
                    stamp,
                    result: Ok(TokenReply::Token(tokens())),
                },
                Input::User {
                    stamp,
                    result: Ok(user()),
                },
                Input::Saved { stamp, saved: true },
                Input::Repos {
                    generation: stamp.generation,
                    result: Ok(Repositories {
                        names: Bounded::new(vec![RepoName::new("owner/repo").unwrap()]).unwrap(),
                    }),
                },
            ] {
                let effects = m.on(late, Instant::now());
                assert!(matches!(effects.as_slice(), [Effect::View(view)] if *view == expected));
                assert_eq!(m.view(Instant::now()), expected);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn removal_outcomes_and_same_process_retry_never_resume_a_connection() {
    for (result, cleanup) in [
        (Removed::Clean, None),
        (Removed::Refused, Some(Cleanup::Refused)),
        (Removed::NotDurable, Some(Cleanup::NotDurable)),
    ] {
        let mut m = empty();
        connected(&mut m);
        let old = m.stamp();
        m.on(Input::Op(GitHubOp::SignOut), Instant::now());
        let generation = m.stamp().generation;
        m.on(Input::Removed { generation, result }, Instant::now());
        assert_eq!(m.view(Instant::now()), GitHubView::SignedOut { cleanup });
        m.on(
            Input::Saved {
                stamp: old,
                saved: true,
            },
            Instant::now(),
        );
        assert_eq!(m.view(Instant::now()), GitHubView::SignedOut { cleanup });
        let retry = m.on(Input::Op(GitHubOp::RetryCleanup), Instant::now());
        assert_eq!(
            retry
                .iter()
                .filter(|e| matches!(e, Effect::Remove { generation: g } if *g == generation))
                .count(),
            usize::from(cleanup.is_some())
        );
        m.on(
            Input::Removed {
                generation,
                result: Removed::Clean,
            },
            Instant::now(),
        );
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::SignedOut { cleanup: None }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn only_exact_fetched_repository_spelling_can_be_selected_or_restored() {
    let repo = RepoName::new("Owner/Repo").unwrap();
    let mut m = empty();
    m.default = Some(DefaultRepo {
        account: user().id,
        repo: RepoName::new("owner/repo").unwrap(),
    });
    connected(&mut m);
    m.on(
        Input::Repos {
            generation: m.stamp().generation,
            result: Ok(Repositories {
                names: Bounded::new(vec![repo.clone()]).unwrap(),
            }),
        },
        Instant::now(),
    );
    let GitHubView::Connected { default, .. } = m.view(Instant::now()) else {
        panic!("connected")
    };
    assert_eq!(default, None);
    let refused = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(
            RepoName::new("owner/repo").unwrap(),
        )),
        Instant::now(),
    );
    assert_eq!(error(&refused), ErrorKey::RequestRefused);
    let effects = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(repo.clone())),
        Instant::now(),
    );
    let generation = m.stamp().generation;
    assert!(
        matches!(&effects[0], Effect::SaveDefault { generation: g, default }
        if *g == generation && default.account == user().id && default.repo == repo)
    );
    let GitHubView::Connected { default, .. } = m.view(Instant::now()) else {
        panic!("connected")
    };
    assert_eq!(default, None, "staging is not a committed default");
    m.on(
        Input::DefaultSaved {
            generation,
            repo: repo.clone(),
            saved: true,
        },
        Instant::now(),
    );
    let GitHubView::Connected { default, .. } = m.view(Instant::now()) else {
        panic!("connected")
    };
    assert_eq!(default, Some(repo));
    m.on(
        Input::Unauthorized {
            generation: m.stamp().generation,
        },
        Instant::now(),
    );
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::NeedsSignIn {
            login: user().login
        }
    );
}

#[tokio::test(start_paused = true)]
async fn startup_401_drops_the_bearer_and_save_refusal_stays_final() {
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    m.on(
        Input::User {
            stamp,
            result: Err(ApiError::Refused {
                status: 401,
                meta: crate::connections::http_client::ResponseMeta::default(),
            }),
        },
        Instant::now(),
    );
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::NeedsSignIn {
            login: user().login
        }
    );
    let mut m = empty();
    let stamp = saving(&mut m);
    assert_eq!(
        error(&m.on(
            Input::Saved {
                stamp,
                saved: false
            },
            Instant::now()
        )),
        ErrorKey::CredentialsUnavailable
    );
    m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::SignedOut {
            cleanup: Some(Cleanup::Running)
        }
    );
}

#[test]
fn outermost_carriers_and_attempt_do_not_print_secrets_or_byte_lists() {
    let stamp = Stamp { generation: 1 };
    let input = Input::Poll {
        stamp,
        result: Ok(TokenReply::Token(tokens())),
    };
    let effect = Effect::Save {
        stamp,
        account: account(),
    };
    let now = Instant::now();
    let attempt = Attempt {
        challenge: challenge(1, 900),
        next_poll: now,
        polling: false,
    };
    let text = format!("{input:?} {effect:?} {attempt:?}");
    assert!(!text.contains("sentinel"));
    assert!(!text.contains("115, 101, 110, 116, 105, 110, 101, 108"));
}

#[tokio::test(start_paused = true)]
async fn cancel_and_expiry_during_a_save_queue_removal_behind_the_write() {
    for expire in [false, true] {
        let mut m = empty();
        let stamp = saving(&mut m);
        let effects = if expire {
            tokio::time::advance(Duration::from_mins(15)).await;
            m.on(Input::Tick, Instant::now())
        } else {
            m.on(Input::Op(GitHubOp::CancelConnect), Instant::now())
        };
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::Remove { .. }))
                .count(),
            1
        );
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running)
            }
        );
        m.on(Input::Saved { stamp, saved: true }, Instant::now());
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running)
            }
        );
    }
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::SignedOut {
            cleanup: Some(Cleanup::Running)
        }
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_api_result_ends_attempt_and_another_attempt_has_a_new_stamp() {
    for (failure, key) in [
        (ApiError::Response, ErrorKey::ResponseUnreadable),
        (
            ApiError::Http(crate::connections::http_client::HttpError::Timeout),
            ErrorKey::AccountOffline,
        ),
    ] {
        let mut m = empty();
        m.on(Input::Op(GitHubOp::Connect), Instant::now());
        let stamp = m.stamp();
        assert_eq!(
            error(&m.on(
                Input::Code {
                    stamp,
                    result: Err(failure)
                },
                Instant::now()
            )),
            key
        );
        m.on(Input::Op(GitHubOp::Connect), Instant::now());
        assert_ne!(m.stamp(), stamp);
        let late = m.on(
            Input::Code {
                stamp,
                result: Ok(challenge(1, 900)),
            },
            Instant::now(),
        );
        assert_eq!(late.len(), 1);
        assert_eq!(m.view(Instant::now()), GitHubView::Requesting);
    }
}

#[tokio::test(start_paused = true)]
async fn startup_non_401_failure_keeps_the_saved_account_with_an_empty_refreshable_list() {
    for refusal in [
        ApiError::Response,
        ApiError::Http(crate::connections::http_client::HttpError::Connect),
        ApiError::Refused {
            status: 500,
            meta: crate::connections::http_client::ResponseMeta::default(),
        },
    ] {
        let mut m = new_machine(Some(DefaultRepo {
            account: user().id,
            repo: RepoName::new("owner/repo").unwrap(),
        }));
        let stamp = m.stamp();
        m.on(
            Input::Loaded {
                stamp,
                result: Ok(Some(account())),
            },
            Instant::now(),
        );
        let effects = m.on(
            Input::User {
                stamp,
                result: Err(refusal),
            },
            Instant::now(),
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Save { .. } | Effect::Remove { .. }))
        );
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::NotVerified {
                login: user().login,
            }
        );
        assert_eq!(m.user(), None, "offline profile data is not authority");
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
        m.on(
            Input::Repos {
                generation: m.stamp().generation,
                result: Ok(Repositories {
                    names: Bounded::new(vec![RepoName::new("owner/repo").unwrap()]).unwrap(),
                }),
            },
            Instant::now(),
        );
        assert_eq!(m.user(), None);
        assert_eq!(
            m.repositories(),
            None,
            "a listing cannot verify an offline identity"
        );
        let effects = m.on(Input::Op(GitHubOp::RefreshRepos), Instant::now());
        assert!(matches!(effects[0], Effect::ReadUser { stamp, .. } if stamp == m.stamp()));
        let stamp = m.stamp();
        let effects = m.on(
            Input::User {
                stamp,
                result: Ok(user()),
            },
            Instant::now(),
        );
        assert!(matches!(effects[0], Effect::RefreshRepos { generation }
            if generation == m.stamp().generation));
        assert_eq!(m.user(), Some(&user()));
        assert_eq!(
            m.repositories(),
            None,
            "fresh identity still needs a complete listing"
        );
        let repo = RepoName::new("owner/repo").unwrap();
        m.on(
            Input::Repos {
                generation: m.stamp().generation,
                result: Ok(Repositories {
                    names: Bounded::new(vec![repo.clone()]).unwrap(),
                }),
            },
            Instant::now(),
        );
        assert_eq!(m.repositories(), Some([repo].as_slice()));
    }
}

fn finish_attempt(m: &mut Machine) {
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    m.on(
        Input::Removed {
            generation: m.stamp().generation,
            result: Removed::Clean,
        },
        Instant::now(),
    );
}
fn polling(m: &mut Machine) -> Stamp {
    let stamp = begin(m, 1, 900);
    m.on(Input::Tick, Instant::now() + Duration::from_secs(1));
    stamp
}
fn verifying(m: &mut Machine) -> Stamp {
    let stamp = polling(m);
    m.on(
        Input::Poll {
            stamp,
            result: Ok(TokenReply::Token(tokens())),
        },
        Instant::now(),
    );
    stamp
}
fn stale(m: &mut Machine, input: Input) {
    let before = m.view(Instant::now());
    let effects = m.on(input, Instant::now());
    assert_eq!(
        effects.len(),
        1,
        "stale completion must emit only the unchanged view"
    );
    assert_eq!(m.view(Instant::now()), before);
}

#[tokio::test(start_paused = true)]
async fn stale_poll_cannot_supply_a_token_to_a_new_poll() {
    let mut m = empty();
    let old = polling(&mut m);
    finish_attempt(&mut m);
    polling(&mut m);
    stale(
        &mut m,
        Input::Poll {
            stamp: old,
            result: Ok(TokenReply::Token(tokens())),
        },
    );
}

#[tokio::test(start_paused = true)]
async fn stale_user_cannot_save_another_attempts_identity() {
    let mut m = empty();
    let old = verifying(&mut m);
    finish_attempt(&mut m);
    verifying(&mut m);
    stale(
        &mut m,
        Input::User {
            stamp: old,
            result: Ok(user()),
        },
    );
}

#[tokio::test(start_paused = true)]
async fn stale_unauthorized_cannot_demote_a_new_connection() {
    let mut m = empty();
    connected(&mut m);
    let old = m.stamp().generation;
    finish_attempt(&mut m);
    connected(&mut m);
    stale(&mut m, Input::Unauthorized { generation: old });
}

#[tokio::test(start_paused = true)]
async fn stale_repositories_cannot_replace_a_new_connections_list() {
    let mut m = empty();
    connected(&mut m);
    let old = m.stamp().generation;
    finish_attempt(&mut m);
    connected(&mut m);
    stale(
        &mut m,
        Input::Repos {
            generation: old,
            result: Ok(Repositories {
                names: Bounded::new(vec![RepoName::new("old/repo").unwrap()]).unwrap(),
            }),
        },
    );
}

#[tokio::test(start_paused = true)]
async fn stale_removal_cannot_clear_a_new_sign_outs_cleanup() {
    let mut m = empty();
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    let old = m.stamp().generation;
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    stale(
        &mut m,
        Input::Removed {
            generation: old,
            result: Removed::Clean,
        },
    );
}

fn assert_no_marker(text: &str) {
    assert!(!text.contains("sentinel"), "{text}");
    assert!(
        !text.contains("115, 101, 110, 116, 105, 110, 101, 108"),
        "{text}"
    );
}

#[tokio::test(start_paused = true)]
async fn account_machine_removed_and_stamp_formats_do_not_expose_markers() {
    let account = account();
    assert_no_marker(&format!("{account:?}"));
    let mut m = empty();
    let stamp = polling(&mut m);
    assert_no_marker(&format!("{m:?}"));
    m.on(
        Input::Poll {
            stamp,
            result: Ok(TokenReply::Token(tokens())),
        },
        Instant::now(),
    );
    assert_no_marker(&format!("{m:?}"));
    m.on(
        Input::User {
            stamp,
            result: Ok(user()),
        },
        Instant::now(),
    );
    assert_no_marker(&format!("{m:?}"));
    m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert_no_marker(&format!("{m:?}"));
    let stamp = Stamp {
        generation: u64::from_le_bytes(*b"sentinel"),
    };
    assert_no_marker(&format!("{stamp:?}"));
    for removed in [Removed::Clean, Removed::Refused, Removed::NotDurable] {
        assert_no_marker(&format!("{removed:?}"));
    }
}

#[tokio::test(start_paused = true)]
async fn startup_success_refreshes_without_saving_the_already_durable_account() {
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    let effects = m.on(
        Input::User {
            stamp,
            result: Ok(user()),
        },
        Instant::now(),
    );
    assert!(matches!(effects.as_slice(), [
        Effect::RefreshRepos { generation },
        Effect::Wake(at),
        Effect::View(GitHubView::Connected { .. })
    ] if *generation == stamp.generation + 1
        && *at == Instant::now() + Duration::from_mins(59)));
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::Connected {
            login: user().login,
            repos: Bounded::default(),
            truncated: false,
            default: None
        }
    );
}

#[tokio::test(start_paused = true)]
async fn the_challenge_view_has_the_exact_user_code_and_remaining_seconds() {
    let mut m = empty();
    begin(&mut m, 5, 900);
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::AwaitingUser {
            user_code: UserCode::new("ABCD-EFGH").unwrap(),
            expires_in_s: 900
        }
    );
    tokio::time::advance(Duration::from_secs(100)).await;
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::AwaitingUser {
            user_code: UserCode::new("ABCD-EFGH").unwrap(),
            expires_in_s: 800
        }
    );
}

#[tokio::test(start_paused = true)]
async fn cleanup_running_and_not_durable_block_connect_but_refused_allows_it() {
    for result in [Removed::Clean, Removed::Refused] {
        let mut m = empty();
        m.on(Input::Op(GitHubOp::SignOut), Instant::now());
        m.on(
            Input::Removed {
                generation: m.stamp().generation,
                result,
            },
            Instant::now(),
        );
        let old = m.stamp();
        let effects = m.on(Input::Op(GitHubOp::Connect), Instant::now());
        assert!(
            matches!(effects.as_slice(), [Effect::RequestCode { stamp }, Effect::View(GitHubView::Requesting)] if stamp.generation == old.generation + 1)
        );
    }
    for cleanup in [Cleanup::Running, Cleanup::NotDurable] {
        let mut m = empty();
        m.state = State::SignedOut(Some(cleanup));
        stale(&mut m, Input::Op(GitHubOp::Connect));
    }
}

#[tokio::test(start_paused = true)]
async fn needs_sign_in_can_connect_but_cancel_does_not_sign_a_connected_account_out() {
    let mut m = empty();
    connected(&mut m);
    stale(&mut m, Input::Op(GitHubOp::CancelConnect));
    m.on(
        Input::Unauthorized {
            generation: m.stamp().generation,
        },
        Instant::now(),
    );
    let effects = m.on(Input::Op(GitHubOp::Connect), Instant::now());
    assert!(matches!(
        effects.as_slice(),
        [
            Effect::RequestCode { .. },
            Effect::View(GitHubView::Requesting)
        ]
    ));
}

#[tokio::test(start_paused = true)]
async fn expiry_during_user_verification_rejects_a_late_identity() {
    let mut m = empty();
    let stamp = verifying(&mut m);
    tokio::time::advance(Duration::from_mins(15)).await;
    let effects = m.on(Input::Tick, Instant::now());
    assert_eq!(error(&effects), ErrorKey::SignInTimedOut);
    stale(
        &mut m,
        Input::User {
            stamp,
            result: Ok(user()),
        },
    );
}

#[tokio::test(start_paused = true)]
async fn the_connected_user_is_only_a_verified_connection() {
    let mut m = empty();
    assert_eq!(m.user(), None, "signed out");
    let stamp = verifying(&mut m);
    assert_eq!(m.user(), None, "verifying");
    m.on(
        Input::User {
            stamp,
            result: Ok(user()),
        },
        Instant::now(),
    );
    assert_eq!(m.user(), None, "saving");
    m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert_eq!(m.user(), Some(&user()));
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    assert_eq!(m.user(), None, "signed out again");

    let namesake = GitHubUser {
        id: 8,
        login: GitHubLogin::new("sentinel").unwrap(),
    };
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    assert_eq!(m.user(), None, "a loaded account isn't verified yet");
    m.on(
        Input::User {
            stamp,
            result: Ok(namesake),
        },
        Instant::now(),
    );
    assert_eq!(
        m.user(),
        None,
        "a namesake cannot replace the saved numeric identity"
    );
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::NeedsSignIn {
            login: user().login
        }
    );
    m.on(
        Input::Unauthorized {
            generation: m.stamp().generation,
        },
        Instant::now(),
    );
    assert_eq!(m.user(), None, "needs sign-in");
}
