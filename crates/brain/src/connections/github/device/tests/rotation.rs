use super::*;

#[test]
fn a_stopped_owner_is_not_an_api_failure_and_late_identity_cannot_revive_it() {
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    let effects = m.on(Input::Stopped { stamp }, Instant::now());
    assert_eq!(error(&effects), ErrorKey::RequestRefused);
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::NeedsSignIn {
            login: user().login
        }
    );
    assert_eq!(m.user(), None);
    assert!(m.token().is_none());
    stale(
        &mut m,
        Input::User {
            stamp,
            result: Ok(user()),
        },
    );
    m.on(Input::Op(GitHubOp::Connect), Instant::now());
    let replacement = m.stamp();
    stale(&mut m, Input::Stopped { stamp });
    assert_eq!(m.stamp(), replacement);
    assert_eq!(m.view(Instant::now()), GitHubView::Requesting);
}

#[test]
fn explicit_cancel_ends_a_loaded_identity_check_without_a_late_connected_fallback() {
    let mut m = new_machine(None);
    let stamp = m.stamp();
    m.on(
        Input::Loaded {
            stamp,
            result: Ok(Some(account())),
        },
        Instant::now(),
    );
    assert!(
        !m.connecting(),
        "shell turnover alone must not cancel startup verification"
    );
    m.on(Input::Op(GitHubOp::CancelConnect), Instant::now());
    let effects = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(
            RepoName::new("owner/repo").unwrap(),
        )),
        Instant::now(),
    );
    assert_eq!(error(&effects), ErrorKey::RequestRefused);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Error(_), Effect::View(GitHubView::NotVerified { login })] if *login == user().login
    ));
}

fn marker(m: &mut Machine) -> (Stamp, Account) {
    let old = m.stamp();
    let effects = m.on(Input::Tick, Instant::now());
    let [
        Effect::CancelAll,
        Effect::Save { stamp, account },
        Effect::View(GitHubView::Requesting),
    ] = effects.as_slice()
    else {
        panic!("refresh must save its single-use marker before dispatch: {effects:?}")
    };
    assert_eq!(stamp.generation, old.generation + 1);
    assert_eq!(m.fence().load(Ordering::Acquire), stamp.generation);
    assert!(account.refresh_sent);
    assert_eq!(account.credentials.token.expose().as_str(), "ghu_sentinel");
    assert_eq!(
        account.credentials.refresh.expose().as_str(),
        "ghr_sentinel"
    );
    assert_eq!(m.user(), None);
    assert!(m.token().is_none());
    assert_eq!(m.repositories(), None);
    (*stamp, account.clone())
}

async fn saving_rotation() -> (Machine, Stamp) {
    let mut m = empty();
    connected(&mut m);
    tokio::time::advance(Duration::from_mins(59)).await;
    m.clock = Arc::new(FixedClock(4_540));
    let (stamp, _) = marker(&mut m);
    (m, stamp)
}

#[tokio::test(start_paused = true)]
async fn short_lifetimes_schedule_a_positive_delay_before_expiry_without_immediate_rotation() {
    for (lead_s, lifetime, delay_ms) in [
        (60, 1, 500),
        (60, 2, 1000),
        (60, 59, 29_500),
        (60, 60, 30_000),
        (60, 61, 30_500),
        (60, 120, 60_000),
        (60, 121, 61_000),
        (30, 59, 29_500),
        (30, 60, 30_000),
        (30, 61, 31_000),
    ] {
        let mut m = Machine::with_refresh_lead(None, Duration::from_secs(lead_s));
        m.clock = Arc::new(FixedClock(1_000));
        let mut saved = account();
        saved.credentials.expires_at_s = 1000 + lifetime;
        let now = Instant::now();
        let stamp = m.stamp();
        m.on(
            Input::Loaded {
                stamp,
                result: Ok(Some(saved)),
            },
            now,
        );
        let effects = m.on(
            Input::User {
                stamp,
                result: Ok(user()),
            },
            now,
        );
        let connected = m.stamp();
        let delay = Duration::from_millis(delay_ms);
        assert!(matches!(effects.as_slice(), [
            Effect::Wake(at),
            Effect::View(GitHubView::ConnectedUnlisted { listing: false, .. }),
        ] if *at == now + delay
            && *at > now
            && *at < now + Duration::from_secs(lifetime)));
        assert_eq!(m.user(), Some(&user()));
        stale(&mut m, Input::Tick);
        tokio::time::advance(delay.checked_sub(Duration::from_nanos(1)).unwrap()).await;
        m.clock = Arc::new(FixedClock(1000 + delay.as_secs()));
        stale(&mut m, Input::Tick);
        assert_eq!(m.stamp(), connected);
        tokio::time::advance(Duration::from_nanos(1)).await;
        let (rotating, _) = marker(&mut m);
        assert_eq!(rotating.generation, connected.generation + 1);
        stale(&mut m, Input::Tick);
    }
}

fn dispatch_refresh(m: &mut Machine, stamp: Stamp) {
    let effects = m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert!(matches!(effects.as_slice(), [
        Effect::RefreshToken { stamp: sent, token },
        Effect::View(GitHubView::Requesting),
    ] if *sent == stamp && token.expose().as_str() == "ghr_sentinel"));
    assert_no_marker(&format!("{:?}", effects[0]));
}

fn rotated_tokens() -> Tokens {
    Tokens {
        token: Secret::new(
            GitHubToken::new(Zeroizing::new("ghu_rotated_sentinel".to_owned())).unwrap(),
        ),
        refresh: Secret::new(
            RefreshToken::new(Zeroizing::new("ghr_rotated_sentinel".to_owned())).unwrap(),
        ),
        expires_at_s: 8_140,
        refresh_expires_at_s: 90_940,
    }
}

#[tokio::test(start_paused = true)]
async fn a_fresh_pair_expiring_during_save_ends_without_another_automatic_rotation() {
    for (rotated, overdue_s) in [(false, 0), (false, 1), (true, 0), (true, 1)] {
        let (mut m, stamp, expiry) = if rotated {
            let (mut m, stamp) = saving_rotation().await;
            dispatch_refresh(&mut m, stamp);
            let mut fresh = rotated_tokens();
            fresh.expires_at_s = 4_541;
            m.on(
                Input::Refreshed {
                    stamp,
                    result: Ok(fresh),
                },
                Instant::now(),
            );
            (m, stamp, 4_541)
        } else {
            let mut m = empty();
            let stamp = begin(&mut m, 5, 900);
            tokio::time::advance(Duration::from_secs(5)).await;
            m.on(Input::Tick, Instant::now());
            let mut fresh = tokens();
            fresh.expires_at_s = 1_001;
            m.on(
                Input::Poll {
                    stamp,
                    result: Ok(TokenReply::Token(fresh)),
                },
                Instant::now(),
            );
            (m, stamp, 1_001)
        };
        let effects = m.on(
            Input::User {
                stamp,
                result: Ok(user()),
            },
            Instant::now(),
        );
        assert!(matches!(effects.as_slice(), [
            Effect::Save { stamp: saving, account },
            Effect::View(GitHubView::Requesting),
        ] if *saving == stamp
            && account.user == user()
            && account.credentials.expires_at_s == expiry
            && !account.refresh_sent));
        assert_eq!(m.user(), None);
        tokio::time::advance(Duration::from_secs(1 + overdue_s)).await;
        m.clock = Arc::new(FixedClock(expiry + overdue_s));
        let effects = m.on(Input::Saved { stamp, saved: true }, Instant::now());
        assert!(matches!(effects.as_slice(), [
            Effect::CancelAll,
            Effect::View(GitHubView::NeedsSignIn { login }),
        ] if *login == user().login));
        let terminal = m.stamp();
        assert_eq!(terminal.generation, stamp.generation + 1);
        assert_eq!(m.fence().load(Ordering::Acquire), terminal.generation);
        tokio::time::advance(Duration::from_hours(1)).await;
        for owner in [stamp, terminal] {
            for late in [
                Input::Tick,
                Input::Saved {
                    stamp: owner,
                    saved: true,
                },
                Input::Refreshed {
                    stamp: owner,
                    result: Ok(rotated_tokens()),
                },
                Input::User {
                    stamp: owner,
                    result: Ok(user()),
                },
            ] {
                let effects = m.on(late, Instant::now());
                assert!(matches!(effects.as_slice(), [
                    Effect::View(GitHubView::NeedsSignIn { login }),
                ] if *login == user().login));
                assert_eq!(m.stamp(), terminal);
            }
        }
        assert_eq!(m.user(), None);
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
        assert!(!m.rotating());
    }
}

#[tokio::test(start_paused = true)]
async fn refresh_is_due_at_minus_sixty_and_is_single_use_across_acknowledgments_and_restart() {
    let mut m = empty();
    connected(&mut m);
    tokio::time::advance(Duration::from_secs(3_539)).await;
    m.clock = Arc::new(FixedClock(4_539));
    stale(&mut m, Input::Tick);
    tokio::time::advance(Duration::from_secs(1)).await;
    m.clock = Arc::new(FixedClock(4_540));
    let (stamp, account) = marker(&mut m);
    stale(&mut m, Input::Tick);
    stale(
        &mut m,
        Input::Refreshed {
            stamp,
            result: Ok(rotated_tokens()),
        },
    );
    let mut restarted = new_machine(None);
    restarted.clock = Arc::new(FixedClock(4_540));
    let effects = restarted.on(
        Input::Loaded {
            stamp: restarted.stamp(),
            result: Ok(Some(account)),
        },
        Instant::now(),
    );
    assert!(matches!(effects.as_slice(), [
        Effect::CancelAll,
        Effect::View(GitHubView::NeedsSignIn { login }),
    ] if *login == user().login));
    dispatch_refresh(&mut m, stamp);
    stale(&mut m, Input::Saved { stamp, saved: true });
    stale(&mut m, Input::Tick);
}

#[tokio::test(start_paused = true)]
async fn a_consumed_or_expired_refresh_credential_never_dispatches_after_load() {
    for (sent, expiry) in [(true, 87_400), (false, 1_000), (false, 999)] {
        let mut account = account();
        account.refresh_sent = sent;
        account.credentials.refresh_expires_at_s = expiry;
        let mut m = new_machine(None);
        let stamp = m.stamp();
        let effects = m.on(
            Input::Loaded {
                stamp,
                result: Ok(Some(account)),
            },
            Instant::now(),
        );
        assert!(matches!(effects.as_slice(), [
            Effect::CancelAll,
            Effect::View(GitHubView::NeedsSignIn { login }),
        ] if *login == user().login));
        assert_eq!(m.user(), None);
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
        stale(&mut m, Input::Tick);
        stale(
            &mut m,
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
        );
        stale(&mut m, Input::Saved { stamp, saved: true });
    }
}

#[tokio::test(start_paused = true)]
async fn rotation_checks_the_numeric_user_and_saves_the_whole_new_pair_before_connecting() {
    let (mut m, stamp) = saving_rotation().await;
    dispatch_refresh(&mut m, stamp);
    let effects = m.on(
        Input::Refreshed {
            stamp,
            result: Ok(rotated_tokens()),
        },
        Instant::now(),
    );
    assert!(matches!(effects.as_slice(), [
        Effect::ReadUser { stamp: checked, token },
        Effect::View(GitHubView::Requesting),
    ] if *checked == stamp && token.expose().as_str() == "ghu_rotated_sentinel"));
    assert_eq!(m.user(), None);
    let renamed = GitHubUser {
        id: 7,
        login: GitHubLogin::new("renamed").unwrap(),
    };
    let effects = m.on(
        Input::User {
            stamp,
            result: Ok(renamed.clone()),
        },
        Instant::now(),
    );
    let [
        Effect::Save {
            stamp: saved,
            account,
        },
        Effect::View(GitHubView::Requesting),
    ] = effects.as_slice()
    else {
        panic!("the rotated pair must be saved before publication: {effects:?}")
    };
    assert_eq!(*saved, stamp);
    assert_eq!(account.user, renamed);
    assert!(!account.refresh_sent);
    assert_eq!(
        account.credentials.token.expose().as_str(),
        "ghu_rotated_sentinel"
    );
    assert_eq!(
        account.credentials.refresh.expose().as_str(),
        "ghr_rotated_sentinel"
    );
    assert_eq!(account.credentials.expires_at_s, 8_140);
    assert_eq!(account.credentials.refresh_expires_at_s, 90_940);
    assert_no_marker(&format!("{:?}", effects[0]));
    assert_eq!(m.user(), None);
    assert!(m.token().is_none());
    assert_eq!(m.repositories(), None);
    let effects = m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert!(matches!(effects.as_slice(), [
        Effect::Wake(at),
        Effect::View(GitHubView::ConnectedUnlisted { login, listing: false }),
    ] if *at == Instant::now() + Duration::from_mins(59)
        && *login == renamed.login));
    assert_eq!(m.stamp().generation, stamp.generation + 1);
    assert_eq!(m.user(), Some(&renamed));
    assert_eq!(m.token().unwrap().expose().as_str(), "ghu_rotated_sentinel");
    assert_eq!(m.repositories(), None);
    let repo = RepoName::new("Owner/After").unwrap();
    m.on(Input::Op(GitHubOp::RefreshRepos), Instant::now());
    m.on(
        Input::Repos {
            generation: m.stamp().generation,
            result: Ok(Repositories {
                names: Bounded::new(vec![repo.clone()]).unwrap(),
                ids: Vec::new(),
            }),
        },
        Instant::now(),
    );
    assert_eq!(m.repositories(), Some([repo].as_slice()));
    stale(&mut m, Input::Saved { stamp, saved: true });
    stale(
        &mut m,
        Input::Refreshed {
            stamp,
            result: Ok(tokens()),
        },
    );
    assert_eq!(m.token().unwrap().expose().as_str(), "ghu_rotated_sentinel");
}

#[tokio::test(start_paused = true)]
async fn failed_marker_and_pair_saves_retain_cleanup_and_cannot_be_revived() {
    for pair_save in [false, true] {
        let (mut m, stamp) = saving_rotation().await;
        if pair_save {
            dispatch_refresh(&mut m, stamp);
            m.on(
                Input::Refreshed {
                    stamp,
                    result: Ok(rotated_tokens()),
                },
                Instant::now(),
            );
            m.on(
                Input::User {
                    stamp,
                    result: Ok(user()),
                },
                Instant::now(),
            );
        }
        let effects = m.on(
            Input::Saved {
                stamp,
                saved: false,
            },
            Instant::now(),
        );
        assert_eq!(error(&effects), ErrorKey::CredentialsUnavailable);
        assert!(matches!(effects.as_slice(), [
            Effect::CancelAll,
            Effect::Remove { generation },
            Effect::Error(_),
            Effect::View(GitHubView::SignedOut { cleanup: Some(Cleanup::Running) }),
        ] if *generation == stamp.generation + 1));
        for late in [
            Input::Saved { stamp, saved: true },
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
            Input::User {
                stamp,
                result: Ok(user()),
            },
        ] {
            stale(&mut m, late);
        }
        assert_eq!(m.user(), None);
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_refresh_is_terminal_and_stale_success_cannot_supply_a_pair() {
    for (failure, key) in [
        (ApiError::Response, ErrorKey::ResponseUnreadable),
        (
            ApiError::Http(crate::connections::http_client::HttpError::Timeout),
            ErrorKey::AccountOffline,
        ),
    ] {
        let (mut m, stamp) = saving_rotation().await;
        dispatch_refresh(&mut m, stamp);
        let effects = m.on(
            Input::Refreshed {
                stamp,
                result: Err(failure),
            },
            Instant::now(),
        );
        assert_eq!(error(&effects), key);
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::NeedsSignIn {
                login: user().login
            }
        );
        stale(
            &mut m,
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
        );
        stale(
            &mut m,
            Input::User {
                stamp,
                result: Ok(user()),
            },
        );
        stale(&mut m, Input::Saved { stamp, saved: true });
        let saved = saving(&mut m);
        m.on(
            Input::Saved {
                stamp: saved,
                saved: true,
            },
            Instant::now(),
        );
        tokio::time::advance(Duration::from_secs(30)).await;
        m.clock = Arc::new(FixedClock(4_570));
        let (newer, _) = marker(&mut m);
        dispatch_refresh(&mut m, newer);
        assert_ne!(newer, stamp);
        stale(
            &mut m,
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_rotated_identity_mismatch_or_failed_check_never_falls_back_to_connected() {
    for (result, key) in [
        (
            Ok(GitHubUser {
                id: 8,
                login: user().login,
            }),
            ErrorKey::RequestRefused,
        ),
        (Err(ApiError::Response), ErrorKey::ResponseUnreadable),
        (
            Err(ApiError::Http(
                crate::connections::http_client::HttpError::Connect,
            )),
            ErrorKey::AccountOffline,
        ),
    ] {
        let (mut m, stamp) = saving_rotation().await;
        dispatch_refresh(&mut m, stamp);
        m.on(
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
            Instant::now(),
        );
        let effects = m.on(Input::User { stamp, result }, Instant::now());
        assert_eq!(error(&effects), key);
        assert_eq!(
            m.view(Instant::now()),
            GitHubView::NeedsSignIn {
                login: user().login
            }
        );
        assert_eq!(m.user(), None);
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
        stale(
            &mut m,
            Input::User {
                stamp,
                result: Ok(user()),
            },
        );
        stale(&mut m, Input::Saved { stamp, saved: true });
        stale(
            &mut m,
            Input::Refreshed {
                stamp,
                result: Ok(rotated_tokens()),
            },
        );
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_cancel_and_sign_out_fence_every_rotation_phase_and_late_completion() {
    for sign_out in [false, true] {
        for (phase, cancellation_cleanup) in [
            (0, Some(Cleanup::Running)),
            (1, None),
            (2, None),
            (3, Some(Cleanup::Running)),
        ] {
            let (mut m, stamp) = saving_rotation().await;
            if phase >= 1 {
                dispatch_refresh(&mut m, stamp);
            }
            if phase >= 2 {
                m.on(
                    Input::Refreshed {
                        stamp,
                        result: Ok(rotated_tokens()),
                    },
                    Instant::now(),
                );
            }
            if phase >= 3 {
                m.on(
                    Input::User {
                        stamp,
                        result: Ok(user()),
                    },
                    Instant::now(),
                );
            }
            let (op, cleanup) = if sign_out {
                (GitHubOp::SignOut, Some(Cleanup::Running))
            } else {
                (GitHubOp::CancelConnect, cancellation_cleanup)
            };
            let effects = m.on(Input::Op(op), Instant::now());
            assert!(matches!(effects[0], Effect::CancelAll));
            assert_eq!(m.stamp().generation, stamp.generation + 1, "phase {phase}");
            assert_eq!(m.fence().load(Ordering::Acquire), m.stamp().generation);
            assert_eq!(m.view(Instant::now()), GitHubView::SignedOut { cleanup });
            for late in [
                Input::Saved { stamp, saved: true },
                Input::Refreshed {
                    stamp,
                    result: Ok(rotated_tokens()),
                },
                Input::User {
                    stamp,
                    result: Ok(user()),
                },
            ] {
                stale(&mut m, late);
            }
            assert_eq!(m.user(), None);
            assert!(m.token().is_none());
            assert_eq!(m.repositories(), None);
        }
    }
}
