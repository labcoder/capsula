//! Offline machine tests with a paused clock.
use super::*;
use kumochi_protocol::message::UserCode;
use zeroize::Zeroizing;

fn bearer() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new("ghu_sentinel".to_owned())).unwrap())
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
    let mut m = Machine::new(None);
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
            result: Ok(TokenReply::Token(bearer())),
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
async fn slow_down_uses_both_intervals_and_never_exceeds_900() {
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
    let mut m = empty();
    let stamp = begin(&mut m, 900, 900);
    let now = Instant::now();
    // The minimum interval equals the deadline, so no poll can actually be sent.
    assert_eq!(polls(&m.on(Input::Tick, now + Duration::from_secs(899))), 0);
    assert_eq!(
        error(&m.on(Input::Tick, now + Duration::from_mins(15))),
        ErrorKey::SignInTimedOut
    );
    // Exercise the saturation in an admitted in-flight response independently of expiry.
    let mut m = empty();
    let stamp2 = begin(&mut m, 900, 900);
    if let State::Awaiting(a) = &mut m.state {
        a.polling = true;
    }
    m.on(
        Input::Poll {
            stamp: stamp2,
            result: Ok(TokenReply::Error {
                code: TokenError::SlowDown,
                interval_s: None,
            }),
        },
        now,
    );
    let State::Awaiting(a) = &m.state else {
        panic!("awaiting")
    };
    assert_eq!(a.challenge.interval_s, 900);
    assert_eq!(stamp.generation, 1);
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
            result: Ok(TokenReply::Token(bearer())),
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
                result: Ok(TokenReply::Token(bearer())),
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
        for stage in 0..4 {
            let mut m = empty();
            m.on(Input::Op(GitHubOp::Connect), Instant::now());
            let stamp = m.stamp();
            if stage >= 1 {
                m.on(
                    Input::Code {
                        stamp,
                        result: Ok(challenge(1, 900)),
                    },
                    Instant::now(),
                );
            }
            if stage >= 2 {
                tokio::time::advance(Duration::from_secs(1)).await;
                m.on(Input::Tick, Instant::now());
                m.on(
                    Input::Poll {
                        stamp,
                        result: Ok(TokenReply::Token(bearer())),
                    },
                    Instant::now(),
                );
            }
            if stage >= 3 {
                m.on(
                    Input::User {
                        stamp,
                        result: Ok(user()),
                    },
                    Instant::now(),
                );
            }
            let op = if sign_out {
                GitHubOp::SignOut
            } else {
                GitHubOp::CancelConnect
            };
            m.on(Input::Op(op), Instant::now());
            assert_ne!(m.fence().load(Ordering::Acquire), stamp.generation);
            for late in [
                Input::Code {
                    stamp,
                    result: Ok(challenge(1, 900)),
                },
                Input::Poll {
                    stamp,
                    result: Ok(TokenReply::Token(bearer())),
                },
                Input::User {
                    stamp,
                    result: Ok(user()),
                },
                Input::Saved { stamp, saved: true },
                Input::Repos {
                    generation: stamp.generation,
                    names: Bounded::new(vec![RepoName::new("owner/repo").unwrap()]).unwrap(),
                    truncated: false,
                },
            ] {
                let effects = m.on(late, Instant::now());
                assert_eq!(effects.len(), 1);
                assert!(matches!(
                    m.view(Instant::now()),
                    GitHubView::SignedOut { .. }
                ));
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
    m.default = Some(RepoName::new("owner/repo").unwrap());
    connected(&mut m);
    m.on(
        Input::Repos {
            generation: m.stamp().generation,
            names: Bounded::new(vec![repo.clone()]).unwrap(),
            truncated: false,
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
    assert!(matches!(&effects[0], Effect::SaveDefault(name) if name == &repo));
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
    let mut m = Machine::new(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(Account {
                token: bearer(),
                user: user(),
            })),
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
    let stamp = Stamp {
        attempt: 1,
        generation: 1,
    };
    let input = Input::Poll {
        stamp,
        result: Ok(TokenReply::Token(bearer())),
    };
    let effect = Effect::Save {
        stamp,
        account: Account {
            token: bearer(),
            user: user(),
        },
    };
    let now = Instant::now();
    let attempt = Attempt {
        challenge: challenge(1, 900),
        deadline: now,
        next_poll: now,
        polling: false,
    };
    let text = format!("{input:?} {effect:?} {attempt:?}");
    assert!(!text.contains("sentinel"));
    assert!(!text.contains("115, 101, 110, 116, 105, 110, 101, 108"));
    assert!(format!("{:?}", "sentinel").contains("sentinel"));
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
    let mut m = Machine::new(None);
    let stamp = m.stamp();
    m.on(Input::Op(GitHubOp::SignOut), Instant::now());
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(Account {
                token: bearer(),
                user: user(),
            })),
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
        (ApiError::RefreshUnsupported, ErrorKey::AccountMisconfigured),
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
