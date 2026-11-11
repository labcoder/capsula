use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::connections::github::{GitHubEndpoint, lane, record};
use crate::connections::http_client::{
    Endpoint, Http, HttpError, Request, Response, ResponseMeta, SharedHttp,
};
use crate::connections::vault::Vault;
use crate::platform::credential::SecretBytes;
use kumochi_protocol::Secret;
use kumochi_protocol::message::GitHubView;
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Code,
    Poll,
    Refresh,
    User,
    Installations,
    Repositories,
}

#[derive(Clone)]
struct AccountHttp {
    replies: Arc<Mutex<VecDeque<(RequestKind, Value)>>>,
    requests: Arc<Mutex<Vec<RequestKind>>>,
}

impl AccountHttp {
    fn new(replies: Vec<(RequestKind, Value)>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn factory(&self) -> crate::runtime::account::HttpFactory {
        let http = SharedHttp::new(self.clone());
        Arc::new(move || Ok(http.clone()))
    }

    fn seen(&self) -> Vec<RequestKind> {
        self.requests.lock().unwrap().clone()
    }
}

impl Http for AccountHttp {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> {
        let kind = match request.endpoint {
            Endpoint::GitHub(GitHubEndpoint::DeviceCode { .. }) => RequestKind::Code,
            Endpoint::GitHub(GitHubEndpoint::AccessToken { .. }) => RequestKind::Poll,
            Endpoint::GitHub(GitHubEndpoint::RefreshToken { .. }) => RequestKind::Refresh,
            Endpoint::GitHub(GitHubEndpoint::User { .. }) => RequestKind::User,
            Endpoint::GitHub(GitHubEndpoint::Installations { .. }) => RequestKind::Installations,
            Endpoint::GitHub(GitHubEndpoint::Repositories { .. }) => RequestKind::Repositories,
            _ => panic!("account activation must not file or call another provider"),
        };
        let (expected, reply) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("one reply per request");
        assert_eq!(kind, expected);
        self.requests.lock().unwrap().push(kind);
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            meta: ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(serde_json::to_vec(&reply).unwrap())),
        }))
    }
}

fn token_reply() -> Value {
    json!({
        "access_token":"ghu_runtime_fresh", "refresh_token":"ghr_runtime_fresh",
        "expires_in":28800, "refresh_token_expires_in":15_897_600,
        "token_type":"bearer", "scope":""
    })
}

fn identity_and_repositories() -> Vec<(RequestKind, Value)> {
    vec![
        (RequestKind::User, json!({"id":41,"login":"offline-user"})),
        (
            RequestKind::Installations,
            json!({"total_count":1,"installations":[{"id":9}]}),
        ),
        (
            RequestKind::Repositories,
            json!({"total_count":1,"repositories":[{"full_name":"offline-owner/first"}]}),
        ),
    ]
}

fn configured_accounts(
    scratch: &Scratch,
    store: ObservedStore,
    http: &AccountHttp,
) -> crate::runtime::account::Accounts {
    let mut net = crate::test_support::net();
    net.http = http.factory();
    let mut accounts = owner_accounts(scratch, store, net);
    accounts.github = crate::runtime::account::github::GitHub::with_client(Some("offline-client"));
    accounts
        .github
        .start(accounts.jobs.as_ref(), &accounts.reads_tx);
    accounts
}

async fn github_view(shell: &mut Shell<DuplexStream>) -> GitHubView {
    match shell.control().await {
        Some(ControlToShell::GitHub(view)) => view,
        other => panic!("expected a GitHub view, got {other:?}"),
    }
}

async fn repositories_visible(shell: &mut Shell<DuplexStream>) {
    loop {
        match github_view(shell).await {
            GitHubView::Requesting | GitHubView::AwaitingUser { .. } => {}
            GitHubView::Connected {
                login,
                repos,
                truncated,
                default,
            } => {
                assert_eq!(login.as_str(), "offline-user");
                assert!(!truncated);
                assert_eq!(default, None);
                if !repos.is_empty() {
                    assert_eq!(repos.len(), 1);
                    assert_eq!(repos[0].as_str(), "offline-owner/first");
                    return;
                }
            }
            other => panic!("unexpected account state: {other:?}"),
        }
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the real control, FIFO, settings and reconnect lifecycle with its cleanup assertions"
)]
async fn configured_account_flows_through_real_controls_fifo_settings_and_reconnect() {
    let scratch = Scratch::new("github-account-activation");
    let store = ObservedStore::default();
    let mut replies = vec![
        (
            RequestKind::Code,
            json!({
                "device_code":"offline-device", "user_code":"ABCD-EFGH",
                "verification_uri":"https://github.com/login/device", "expires_in":900, "interval":1
            }),
        ),
        (RequestKind::Poll, json!({"error":"authorization_pending"})),
        (RequestKind::Poll, json!({"error":"slow_down"})),
        (RequestKind::Poll, token_reply()),
    ];
    replies.extend(identity_and_repositories());
    let http = AccountHttp::new(replies);
    let mut accounts = configured_accounts(&scratch, store.clone(), &http);
    settle_owner(&mut accounts).await;
    assert_eq!(http.seen(), Vec::<RequestKind>::new());
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let write = brain.hold_config_write();
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(
            github_view(&mut shell).await,
            GitHubView::SignedOut { cleanup: None }
        );
        assert!(shell.scenes().await.is_empty());
        shell.send(&ControlToBrain::GitHub(GitHubOp::Connect)).await;
        assert_eq!(github_view(&mut shell).await, GitHubView::Requesting);
        let GitHubView::AwaitingUser {
            user_code,
            expires_in_s,
        } = github_view(&mut shell).await
        else {
            panic!("the checked device code is visible");
        };
        assert_eq!(user_code.as_str(), "ABCD-EFGH");
        assert!((1..=900).contains(&expires_in_s));
        repositories_visible(&mut shell).await;
        shell
            .send(&ControlToBrain::GitHub(GitHubOp::SetDefaultRepo(
                RepoName::new("offline-owner/first").unwrap(),
            )))
            .await;
        assert!(
            !scratch
                .dir()
                .join(kumochi_protocol::constants::CONFIG_FILE)
                .exists()
        );
        write.send(crate::runtime::settings::Gate::Write).unwrap();
        let GitHubView::Connected { default, .. } = github_view(&mut shell).await else {
            panic!("the committed default remains connected");
        };
        assert_eq!(default.unwrap().as_str(), "offline-owner/first");
        let saved = crate::config::Config::load(Some(scratch.dir().to_owned()), ThreadId(1));
        let default = saved.github_default().unwrap();
        assert_eq!(default.account, 41);
        assert_eq!(default.repo.as_str(), "offline-owner/first");
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    let expected = vec![
        RequestKind::Code,
        RequestKind::Poll,
        RequestKind::Poll,
        RequestKind::Poll,
        RequestKind::User,
        RequestKind::Installations,
        RequestKind::Repositories,
    ];
    assert_eq!(http.seen(), expected);
    assert!(http.replies.lock().unwrap().is_empty());
    assert_eq!(active_github(&scratch), 1);

    let ((control, data), mut shell) = duplex();
    let client = async {
        let GitHubView::Connected { default, .. } = github_view(&mut shell).await else {
            panic!("reconnect hydrates the committed account");
        };
        assert_eq!(default.unwrap().as_str(), "offline-owner/first");
        assert!(shell.scenes().await.is_empty());
        shell.send(&ControlToBrain::GitHub(GitHubOp::SignOut)).await;
        assert_eq!(
            github_view(&mut shell).await,
            GitHubView::SignedOut {
                cleanup: Some(kumochi_protocol::message::Cleanup::Running)
            }
        );
        assert_eq!(
            github_view(&mut shell).await,
            GitHubView::SignedOut { cleanup: None }
        );
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(active_github(&scratch), 0);
    assert_eq!(http.seen(), expected);
    let config =
        std::fs::read_to_string(scratch.dir().join(kumochi_protocol::constants::CONFIG_FILE))
            .unwrap();
    assert!(!config.contains("ghu_"));
    assert!(!config.contains("ghr_"));
    let store = Store::open(&scratch.db()).unwrap();
    assert_eq!(counts(&store), (0, 0));
}

#[tokio::test]
async fn an_expired_saved_account_rotates_once_and_commits_the_pair_before_becoming_usable() {
    use crate::connections::clock::{Clock, SystemClock};
    let scratch = Scratch::new("github-expired-account");
    let store = ObservedStore::default();
    let now = SystemClock.unix_seconds().unwrap();
    let saved = json!({
        "token":"ghu_runtime_expired", "refresh":"ghr_runtime_expired",
        "expires_at_s":now - 1, "refresh_expires_at_s":now + 15_868_799,
        "refresh_sent":false, "login":"offline-user", "id":41
    });
    {
        let mut vault = Vault::with_service(
            store.store.clone(),
            "github-owner-test",
            &owner_lock(&scratch),
        )
        .unwrap();
        let bytes: SecretBytes = Secret::new(Zeroizing::new(serde_json::to_vec(&saved).unwrap()));
        assert!(matches!(
            vault.github(lane::Job::Save {
                stamp: crate::connections::github::device::Stamp { generation: 0 },
                fence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                account: record::decode(&bytes).unwrap(),
            }),
            lane::Reply::Saved { saved: true, .. }
        ));
    }
    let mut replies = vec![(RequestKind::Refresh, token_reply())];
    replies.extend(identity_and_repositories());
    let http = AccountHttp::new(replies);
    let mut accounts = configured_accounts(&scratch, store.clone(), &http);
    settle_owner(&mut accounts).await;
    let mut brain = brain_with_github_owner(&scratch, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(github_view(&mut shell).await, GitHubView::Requesting);
        assert!(shell.scenes().await.is_empty());
        repositories_visible(&mut shell).await;
        shell.control_out.shutdown().await.unwrap();
        assert_eq!(shell.control().await, None);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(
        http.seen(),
        [
            RequestKind::Refresh,
            RequestKind::User,
            RequestKind::Installations,
            RequestKind::Repositories
        ]
    );
    assert!(http.replies.lock().unwrap().is_empty());
    let mut vault =
        Vault::with_service(store.store, "github-owner-test", &owner_lock(&scratch)).unwrap();
    let lane::Reply::Loaded {
        result: Ok(Some(account)),
        ..
    } = vault.github(lane::Job::Load {
        stamp: crate::connections::github::device::Stamp { generation: 99 },
    })
    else {
        panic!("the rotated pair survives restart");
    };
    assert_eq!(account.user.id, 41);
    let encoded = record::encode(&account).unwrap();
    let encoded: Value = serde_json::from_slice(encoded.expose()).unwrap();
    assert_eq!(encoded["token"], "ghu_runtime_fresh");
    assert_eq!(encoded["refresh"], "ghr_runtime_fresh");
    assert!(!account.refresh_sent);
    assert!(account.credentials.expires_at_s > now + 28000);
}
