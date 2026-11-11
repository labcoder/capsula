use super::*;

fn list(m: &mut Machine, names: &[&str]) {
    m.on(
        Input::Repos {
            generation: m.stamp().generation,
            result: Ok(Repositories {
                names: Bounded::new(
                    names
                        .iter()
                        .map(|name| RepoName::new(name).unwrap())
                        .collect(),
                )
                .unwrap(),
            }),
        },
        Instant::now(),
    );
}

fn default(m: &Machine) -> Option<RepoName> {
    let GitHubView::Connected { default, .. } = m.view(Instant::now()) else {
        panic!("connected")
    };
    default
}

#[tokio::test(start_paused = true)]
async fn repository_authority_requires_a_complete_current_listing_and_is_revoked_on_failure() {
    let mut m = empty();
    connected(&mut m);
    assert_eq!(m.user(), Some(&user()));
    assert_eq!(m.repositories(), None);
    let repo = RepoName::new("Owner/Repo").unwrap();
    list(&mut m, &["Owner/Repo"]);
    assert_eq!(m.repositories(), Some([repo.clone()].as_slice()));
    let generation = m.stamp().generation;
    let effects = m.on(Input::Op(GitHubOp::RefreshRepos), Instant::now());
    assert!(matches!(effects.as_slice(), [
        Effect::RefreshRepos { generation: requested },
        Effect::View(GitHubView::Connected { .. }),
    ] if *requested == generation));
    assert_eq!(m.repositories(), None);
    stale(
        &mut m,
        Input::Repos {
            generation: generation - 1,
            result: Ok(Repositories {
                names: Bounded::new(vec![repo]).unwrap(),
            }),
        },
    );
    assert_eq!(m.repositories(), None);
    let effects = m.on(
        Input::Repos {
            generation,
            result: Err(ApiError::Http(
                crate::connections::http_client::HttpError::Connect,
            )),
        },
        Instant::now(),
    );
    assert_eq!(error(&effects), ErrorKey::AccountOffline);
    assert_eq!(m.user(), Some(&user()));
    assert_eq!(m.repositories(), None);
    list(&mut m, &[]);
    assert_eq!(
        m.repositories(),
        Some([].as_slice()),
        "complete and empty is authoritative"
    );
    for status in [403, 401] {
        let effects = m.on(
            Input::Repos {
                generation,
                result: Err(ApiError::Refused {
                    status,
                    meta: crate::connections::http_client::ResponseMeta::default(),
                }),
            },
            Instant::now(),
        );
        assert_eq!(m.repositories(), None);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Save { .. }))
        );
    }
    assert_eq!(m.user(), None);
    assert!(m.token().is_none());
    assert_eq!(
        m.view(Instant::now()),
        GitHubView::NeedsSignIn {
            login: user().login
        }
    );
}

#[tokio::test(start_paused = true)]
async fn access_expiry_revokes_consumers_even_before_the_refresh_wakeup_is_processed() {
    for wall_jump in [false, true] {
        let mut m = empty();
        connected(&mut m);
        list(&mut m, &["Owner/Repo"]);
        if wall_jump {
            m.clock = Arc::new(FixedClock(4_600));
        } else {
            tokio::time::advance(Duration::from_secs(3_600)).await;
        }
        assert_eq!(m.user(), None);
        assert!(m.token().is_none());
        assert_eq!(m.repositories(), None);
        let effects = m.on(
            Input::Op(GitHubOp::SetDefaultRepo(
                RepoName::new("Owner/Repo").unwrap(),
            )),
            Instant::now(),
        );
        assert_eq!(error(&effects), ErrorKey::RequestRefused);
    }
}

#[tokio::test(start_paused = true)]
async fn defaults_publish_only_a_matching_success_and_keep_the_prior_default_on_failure() {
    let old = RepoName::new("Owner/Old").unwrap();
    let next = RepoName::new("Owner/Next").unwrap();
    let mut m = empty();
    m.restore_default(Some(DefaultRepo {
        account: 8,
        repo: old.clone(),
    }));
    connected(&mut m);
    list(&mut m, &["Owner/Old", "Owner/Next"]);
    assert_eq!(
        default(&m),
        None,
        "a different account cannot inherit a default"
    );
    m.restore_default(Some(DefaultRepo {
        account: 7,
        repo: old.clone(),
    }));
    assert_eq!(default(&m), Some(old.clone()));
    let generation = m.stamp().generation;
    let effects = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(next.clone())),
        Instant::now(),
    );
    assert!(matches!(effects.as_slice(), [
        Effect::SaveDefault { generation: saved, default: proposed },
    ] if *saved == generation && proposed.account == 7 && proposed.repo == next));
    assert_eq!(default(&m), Some(old.clone()));
    let effects = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(old.clone())),
        Instant::now(),
    );
    assert_eq!(
        error(&effects),
        ErrorKey::RequestRefused,
        "only one default write is admitted"
    );
    stale(
        &mut m,
        Input::DefaultSaved {
            generation,
            repo: old.clone(),
            saved: true,
        },
    );
    let effects = m.on(
        Input::DefaultSaved {
            generation,
            repo: next.clone(),
            saved: false,
        },
        Instant::now(),
    );
    assert_eq!(error(&effects), ErrorKey::SettingsRefused);
    assert_eq!(default(&m), Some(old));
    stale(
        &mut m,
        Input::DefaultSaved {
            generation,
            repo: next.clone(),
            saved: true,
        },
    );
    m.on(
        Input::Op(GitHubOp::SetDefaultRepo(next.clone())),
        Instant::now(),
    );
    m.on(
        Input::DefaultSaved {
            generation,
            repo: next.clone(),
            saved: true,
        },
        Instant::now(),
    );
    assert_eq!(default(&m), Some(next));
    assert_eq!(m.default.as_ref().unwrap().account, 7);
}

#[tokio::test(start_paused = true)]
async fn stale_default_acknowledgments_never_cross_numeric_account_generations() {
    let old = RepoName::new("Owner/Old").unwrap();
    let next = RepoName::new("Owner/Next").unwrap();
    let mut m = empty();
    m.restore_default(Some(DefaultRepo {
        account: 7,
        repo: next,
    }));
    connected(&mut m);
    list(&mut m, &["Owner/Old", "Owner/Next"]);
    let generation = m.stamp().generation;
    m.on(
        Input::Op(GitHubOp::SetDefaultRepo(old.clone())),
        Instant::now(),
    );
    finish_attempt(&mut m);
    let stamp = verifying(&mut m);
    let another = GitHubUser {
        id: 8,
        login: user().login,
    };
    m.on(
        Input::User {
            stamp,
            result: Ok(another.clone()),
        },
        Instant::now(),
    );
    m.on(Input::Saved { stamp, saved: true }, Instant::now());
    assert_eq!(m.user(), Some(&another));
    list(&mut m, &["Owner/Old", "Owner/Next"]);
    assert_eq!(default(&m), None);
    let current = m.stamp().generation;
    let effects = m.on(
        Input::Op(GitHubOp::SetDefaultRepo(old.clone())),
        Instant::now(),
    );
    assert!(
        matches!(&effects[0], Effect::SaveDefault { generation: saved, default: proposed }
        if *saved == current && proposed.account == 8 && proposed.repo == old)
    );
    for saved in [true, false] {
        stale(
            &mut m,
            Input::DefaultSaved {
                generation,
                repo: old.clone(),
                saved,
            },
        );
    }
    assert_eq!(default(&m), None);
    m.on(
        Input::DefaultSaved {
            generation: current,
            repo: old.clone(),
            saved: true,
        },
        Instant::now(),
    );
    assert_eq!(default(&m), Some(old));
    assert_eq!(m.default.as_ref().unwrap().account, 8);
}
