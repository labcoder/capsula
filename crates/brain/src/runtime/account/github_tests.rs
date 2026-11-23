use super::*;
use crate::connections::clock::{Clock, SystemClock};
use crate::connections::github::{
    GitHubToken, RefreshToken,
    api::{GitHubUser, Tokens},
    device::{Account, Removed, Stamp},
    lane::{Job as GitHubJob, Reply},
};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::test_support::Scratch;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

#[test]
fn an_unopened_vault_refuses_each_github_operation_without_exposing_the_cause() {
    use crate::connections::github::{lane::LoadError, record};
    use crate::connections::{authority::AuthorityError, error::VaultError};
    for why in [
        VaultError::Busy,
        AuthorityError::NewerSchema.into(),
        VaultError::Lock(std::io::Error::other("foreign-error-sentinel")),
    ] {
        let mut why = Some(why);
        let mut worker: Worker<FakeStore> = Worker::new(
            Box::new(move || Err(why.take().unwrap())),
            Arc::new(crate::connections::clock::SystemClock),
        );
        assert!(matches!(
            worker.run(crate::connections::account::Job::Load { patient: false }),
            crate::connections::account::Done::Loaded(Err(_))
        ));
        let loaded = worker.github(GitHubJob::Load {
            stamp: Stamp { generation: 3 },
        });
        assert_eq!(format!("{loaded:?}"), "GitHubCredentialReply");
        assert!(matches!(
            loaded,
            Reply::Loaded {
                stamp: Stamp { generation: 3 },
                result: Err(LoadError::Unavailable)
            }
        ));
        let bytes = Secret::new(Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({
                "token":"ghu_fake_marker", "refresh":"ghr_fake_marker",
                "expires_at_s":SystemClock.unix_seconds().unwrap() + 28_800,
                "refresh_expires_at_s":SystemClock.unix_seconds().unwrap() + 15_897_600,
                "refresh_sent":false, "login":"sentinel", "id":7
            }))
            .unwrap(),
        ));
        let job = GitHubJob::Save {
            stamp: Stamp { generation: 3 },
            fence: Arc::new(AtomicU64::new(3)),
            account: record::decode(&bytes).unwrap(),
        };
        let (reply, _receiver) = oneshot::channel();
        let work = Work::GitHub { job, reply };
        let debug = format!("{work:?}");
        for marker in [
            "ghu_fake_marker",
            "ghr_fake_marker",
            "sentinel",
            "foreign-error-sentinel",
        ] {
            assert!(!debug.contains(marker));
            assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
        }
        let Work::GitHub { job, .. } = work else {
            unreachable!()
        };
        assert!(matches!(
            worker.github(job),
            Reply::Saved {
                stamp: Stamp { generation: 3 },
                saved: false
            }
        ));
        assert!(matches!(
            worker.github(GitHubJob::Remove { generation: 4 }),
            Reply::Removed {
                generation: 4,
                result: Removed::NotDurable
            }
        ));
    }
}

#[tokio::test]
async fn a_closed_save_receiver_cannot_drop_the_queued_removal() {
    let scratch = Scratch::new("github-worker-order");
    let path = scratch.db().with_file_name("credential-test.lock");
    let store = FakeStore::default();
    let opened_store = store.clone();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let worker = Worker::new(
        Box::new(move || {
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release startup");
            Vault::with_service(opened_store.clone(), "github-worker-test", &path)
        }),
        Arc::new(crate::connections::clock::SystemClock),
    );
    let (loaded_tx, loaded_rx) = std_mpsc::channel();
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (reads_tx, _reads_rx) = mpsc::unbounded_channel();
    let (jobs, released) = spawn(worker, loaded_tx, events_tx, reads_tx).unwrap();
    jobs.send(Work::Account(crate::connections::account::Job::Load {
        patient: false,
    }))
    .unwrap();
    let (save_tx, save_rx) = oneshot::channel();
    drop(save_rx);
    jobs.send(Work::GitHub {
        job: GitHubJob::Save {
            stamp: Stamp { generation: 1 },
            fence: Arc::new(AtomicU64::new(1)),
            account: {
                let mut account = checked_account();
                account.credentials.token = Secret::new(
                    GitHubToken::new(Zeroizing::new("ghu_fake_worker_sentinel".to_owned()))
                        .unwrap(),
                );
                account.user.login = GitHubLogin::new("octocat").unwrap();
                account
            },
        },
        reply: save_tx,
    })
    .unwrap();
    let (remove_tx, remove_rx) = oneshot::channel();
    jobs.send(Work::GitHub {
        job: GitHubJob::Remove { generation: 2 },
        reply: remove_tx,
    })
    .unwrap();
    ready_tx.send(()).unwrap();
    let (loaded, key) = loaded_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("startup finished")
        .expect("startup admitted");
    assert!(matches!(
        loaded,
        Input::Vault(crate::connections::account::Done::Loaded(Ok(_)))
    ));
    assert!(matches!(key.1, Ok(None)));
    let removed = tokio::time::timeout(Duration::from_secs(5), remove_rx)
        .await
        .expect("removal finishes")
        .expect("removal reply");
    assert!(matches!(
        removed,
        Reply::Removed {
            generation: 2,
            result: Removed::Clean
        }
    ));
    drop(jobs);
    tokio::time::timeout(Duration::from_secs(5), released)
        .await
        .expect("worker exits")
        .unwrap();
    assert_eq!(store.names().len(), 1);
    assert!(store.names()[0].starts_with("github-worker-test/gen/host/"));
    let conn = rusqlite::Connection::open(
        scratch
            .db()
            .with_file_name(crate::connections::authority::FILE),
    )
    .unwrap();
    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM generations WHERE kind='github' AND state='active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, 0);
    let removed: i64 = conn
        .query_row(
            "SELECT count(*) FROM generations WHERE kind='github' AND state='removed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed, 1);
}

use crate::connections::github::{
    DeviceCodeValue,
    api::{DeviceCode, TokenReply},
    device::{Effect as GitHubEffect, Input as GitHubInput},
};
use crate::platform::credential::{CredentialStoreError, SecretBytes};
use kumochi_protocol::message::{Cleanup, GitHubLogin, GitHubOp, GitHubView, RepoName, UserCode};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

type Gate = (bool, std_mpsc::Sender<()>, std_mpsc::Receiver<()>);

#[derive(Clone, Default)]
pub(crate) struct ObservedStore {
    pub(crate) store: FakeStore,
    pub(crate) reads: Arc<AtomicUsize>,
    pub(crate) writes: Arc<AtomicUsize>,
    gate: Arc<Mutex<Option<Gate>>>,
}

impl ObservedStore {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "selects the read or write side of the fixture gate"
    )]
    pub(crate) fn hold(&self, write: bool) -> (std_mpsc::Receiver<()>, std_mpsc::Sender<()>) {
        let (entered, heard) = std_mpsc::channel();
        let (resume, resumed) = std_mpsc::channel();
        *self.gate.lock().unwrap() = Some((write, entered, resumed));
        (heard, resume)
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "selects the read or write side of the fixture gate"
    )]
    fn wait(&self, write: bool) {
        let gate = self
            .gate
            .lock()
            .unwrap()
            .take_if(|(at_write, _, _)| *at_write == write);
        if let Some((_, entered, resume)) = gate {
            entered.send(()).unwrap();
            resume
                .recv_timeout(Duration::from_secs(10))
                .expect("release fake vault");
        }
    }

    pub(crate) fn seed(&self, scratch: &Scratch) {
        let mut vault = Vault::with_service(
            self.store.clone(),
            "github-owner-test",
            &owner_lock(scratch),
        )
        .unwrap();
        assert!(matches!(
            vault.github(GitHubJob::Save {
                stamp: Stamp { generation: 0 },
                fence: Arc::new(AtomicU64::new(0)),
                account: checked_account(),
            }),
            Reply::Saved { saved: true, .. }
        ));
    }
}

impl CredentialStore for ObservedStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        if target.contains("/gen/github/") {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        self.wait(false);
        self.store.get(target)
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        if target.contains("/gen/github/") {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.wait(true);
        }
        self.store.set(target, secret)
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.store.delete(target)
    }
}

pub(crate) fn owner_lock(scratch: &Scratch) -> std::path::PathBuf {
    scratch.db().with_file_name("github-owner.lock")
}

pub(crate) fn owner_accounts(scratch: &Scratch, store: ObservedStore, net: Net) -> Accounts {
    let lock = owner_lock(scratch);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "github-owner-test", &lock)),
        Arc::new(crate::connections::clock::SystemClock),
    );
    Accounts::spawn_unconfigured(worker, net).unwrap()
}

pub(crate) fn active_github(scratch: &Scratch) -> i64 {
    rusqlite::Connection::open(
        scratch
            .db()
            .with_file_name(crate::connections::authority::FILE),
    )
    .unwrap()
    .query_row(
        "SELECT count(*) FROM generations WHERE kind='github' AND state='active'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn checked_account() -> Account {
    let now = SystemClock.unix_seconds().unwrap();
    Account {
        credentials: Tokens {
            token: Secret::new(
                GitHubToken::new(Zeroizing::new("ghu_owner_sentinel".to_owned())).unwrap(),
            ),
            refresh: Secret::new(
                RefreshToken::new(Zeroizing::new("ghr_owner_sentinel".to_owned())).unwrap(),
            ),
            expires_at_s: now + 28_800,
            refresh_expires_at_s: now + 15_897_600,
        },
        user: GitHubUser {
            id: 7,
            login: GitHubLogin::new("owner-sentinel").unwrap(),
        },
        refresh_sent: false,
    }
}

pub(crate) fn drive(accounts: &mut Accounts, input: GitHubInput) -> Vec<ControlToShell> {
    accounts
        .github
        .checked(
            input,
            accounts.jobs.as_ref(),
            &accounts.reads_tx,
            &accounts.net.http,
        )
        .unwrap()
}

pub(crate) async fn settle_owner(accounts: &mut Accounts) -> Vec<ControlToShell> {
    let mut frames = Vec::new();
    while accounts.github.pending() {
        let event = tokio::time::timeout(Duration::from_secs(5), accounts.next())
            .await
            .unwrap();
        frames.extend(accounts.handle(event));
    }
    frames
}

async fn one_reply(accounts: &mut Accounts) -> Vec<ControlToShell> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = accounts.next().await;
            if matches!(event, Event::GitHub { .. }) {
                break accounts.handle(event);
            }
            accounts.handle(event);
        }
    })
    .await
    .unwrap()
}

fn device_challenge() -> DeviceCode {
    DeviceCode {
        code: Secret::new(
            DeviceCodeValue::new(Zeroizing::new("device-sentinel".to_owned())).unwrap(),
        ),
        user_code: UserCode::new("ABCD-EFGH").unwrap(),
        expires_in_s: 900,
        interval_s: 1,
    }
}

pub(crate) async fn saving_owner(accounts: &mut Accounts) -> Stamp {
    let stamp = verifying_owner(accounts).await;
    drive(
        accounts,
        GitHubInput::User {
            stamp,
            result: Ok(checked_account().user),
        },
    );
    stamp
}

pub(crate) async fn verifying_owner(accounts: &mut Accounts) -> Stamp {
    tokio::time::pause();
    let stamp = accounts.github.machine().stamp();
    if accounts.github.machine().is_loading() {
        drive(
            accounts,
            GitHubInput::Loaded {
                stamp,
                result: Ok(None),
            },
        );
    }
    drive(accounts, GitHubInput::Op(GitHubOp::Connect));
    let stamp = accounts.github.machine().stamp();
    drive(
        accounts,
        GitHubInput::Code {
            stamp,
            result: Ok(device_challenge()),
        },
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    accounts
        .github
        .wake(
            stamp,
            accounts.jobs.as_ref(),
            &accounts.reads_tx,
            &accounts.net.http,
        )
        .unwrap();
    drive(
        accounts,
        GitHubInput::Poll {
            stamp,
            result: Ok(TokenReply::Token(checked_account().credentials)),
        },
    );
    tokio::time::resume();
    stamp
}

#[tokio::test]
async fn unavailable_admission_and_closed_load_replies_never_claim_a_signed_out_account() {
    for mode in 0..4 {
        let scratch = Scratch::new("github-load-unavailable");
        let store = ObservedStore::default();
        store.seed(&scratch);
        let mut accounts = owner_accounts(&scratch, store, crate::test_support::net());
        let (jobs, queued) = std_mpsc::channel();
        accounts.jobs = (mode != 0).then_some(jobs);
        accounts.github.enable();
        accounts.github_session(Some(11));
        if mode == 1 {
            drop(queued);
            accounts
                .github
                .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        } else {
            accounts
                .github
                .start(accounts.jobs.as_ref(), &accounts.reads_tx);
            if mode >= 2 {
                let Work::GitHub {
                    job: GitHubJob::Load { stamp },
                    reply,
                } = queued.recv().unwrap()
                else {
                    panic!("load")
                };
                assert_eq!(stamp, Stamp { generation: 0 });
                if mode == 2 {
                    drop(reply);
                } else {
                    reply
                        .send(Reply::Loaded {
                            stamp,
                            result: Err(crate::connections::github::lane::LoadError::Unavailable),
                        })
                        .unwrap();
                }
            }
        }
        assert_eq!(
            settle_owner(&mut accounts).await,
            vec![ControlToShell::GitHubError(
                ErrorKey::CredentialsUnavailable.into()
            )]
        );
        assert!(accounts.github.machine().is_loading());
        assert!(accounts.github.machine().user().is_none());
        accounts.github_session(None);
        accounts.github_session(Some(12));
        assert_eq!(accounts.github_opened(), Vec::<ControlToShell>::new());
        assert_eq!(active_github(&scratch), 1);
        accounts.close(Instant::now(), Instant::now()).await;
    }
}

#[tokio::test]
async fn empty_and_cleanup_loads_publish_exact_views_but_offline_errors_are_not_replayed() {
    for (result, cleanup) in [
        (Ok(None), None),
        (
            Err(crate::connections::github::lane::LoadError::Cleanup(
                Removed::Clean,
            )),
            None,
        ),
        (
            Err(crate::connections::github::lane::LoadError::Cleanup(
                Removed::Refused,
            )),
            Some(Cleanup::Refused),
        ),
        (
            Err(crate::connections::github::lane::LoadError::Cleanup(
                Removed::NotDurable,
            )),
            Some(Cleanup::NotDurable),
        ),
    ] {
        let scratch = Scratch::new("github-load-outcome");
        let mut accounts = owner_accounts(
            &scratch,
            ObservedStore::default(),
            crate::test_support::net(),
        );
        let (jobs, queued) = std_mpsc::channel();
        accounts.jobs = Some(jobs);
        accounts.github.enable();
        accounts
            .github
            .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        let Work::GitHub {
            job: GitHubJob::Load { stamp },
            reply,
        } = queued.recv().unwrap()
        else {
            panic!("load")
        };
        reply.send(Reply::Loaded { stamp, result }).unwrap();
        assert_eq!(
            settle_owner(&mut accounts).await,
            Vec::<ControlToShell>::new()
        );
        accounts.github_session(Some(2));
        assert_eq!(
            accounts.github_opened(),
            vec![ControlToShell::GitHub(GitHubView::SignedOut { cleanup })]
        );
        assert!(!accounts.github.machine().is_loading());
        assert!(accounts.github.machine().user().is_none());
        accounts.close(Instant::now(), Instant::now()).await;
    }
}

#[tokio::test]
async fn a_real_saved_account_stops_at_owned_stamped_verification_without_http_or_authority() {
    let scratch = Scratch::new("github-suspended-user");
    let store = ObservedStore::default();
    store.seed(&scratch);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let mut net = crate::test_support::net();
    net.http = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Err(HttpError::Configuration)
    });
    let mut accounts = owner_accounts(&scratch, store, net);
    accounts.github.enable();
    accounts.github_session(Some(1));
    accounts
        .github
        .start(accounts.jobs.as_ref(), &accounts.reads_tx);
    assert_eq!(
        settle_owner(&mut accounts).await,
        vec![ControlToShell::GitHub(GitHubView::Requesting)]
    );
    assert!(matches!(
        accounts.github.suspended(),
        [(
            Stamp { generation: 0 },
            GitHubEffect::ReadUser {
                stamp: Stamp { generation: 0 },
                ..
            }
        )]
    ));
    assert!(accounts.github.machine().user().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    accounts.github_session(None);
    accounts.github_session(Some(2));
    assert_eq!(
        accounts.github_opened(),
        vec![ControlToShell::GitHub(GitHubView::Requesting)]
    );
    assert_eq!(accounts.github.suspended().len(), 1);
    let event = Event::GitHub {
        session: Some(1),
        reply: Reply::Loaded {
            stamp: Stamp { generation: 0 },
            result: Ok(Some(checked_account())),
        },
    };
    let debug = format!("{accounts:?} {:?} {event:?}", accounts.github.suspended());
    for marker in ["ghu_owner_sentinel", "ghr_owner_sentinel", "owner-sentinel"] {
        assert!(!debug.contains(marker));
        assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    }
    accounts.github.enable_verification();
    assert!(!accounts.github.verifying());
    assert_eq!(accounts.github.suspended().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    accounts.close(Instant::now(), Instant::now()).await;
    assert_eq!(accounts.github.suspended().len(), 0);
    assert_eq!(active_github(&scratch), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_on_either_side_of_the_save_fence_settles_fifo_removal() {
    for after_check in [false, true] {
        let scratch = Scratch::new("github-owner-save-race");
        let store = ObservedStore::default();
        let mut accounts = owner_accounts(&scratch, store.clone(), crate::test_support::net());
        accounts.github.enable();
        accounts.github_session(Some(1));
        accounts
            .github
            .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        assert_eq!(
            settle_owner(&mut accounts).await,
            vec![ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: None
            })]
        );
        let (entered, release) = store.hold(after_check);
        if !after_check {
            accounts
                .jobs
                .as_ref()
                .unwrap()
                .send(Work::Account(Job::Load { patient: false }))
                .unwrap();
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let stamp = saving_owner(&mut accounts).await;
        if after_check {
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(accounts.github.machine().user().is_none());
        accounts.github_session(None);
        assert_ne!(accounts.github.machine().stamp(), stamp);
        accounts.github_session(Some(2));
        assert_eq!(
            accounts.github_opened(),
            vec![ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running)
            })]
        );
        release.send(()).unwrap();
        if !after_check {
            let barrier = tokio::time::timeout(Duration::from_secs(5), accounts.events.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                barrier,
                Input::Vault(crate::connections::account::Done::Loaded(Ok(_)))
            ));
        }
        let frames: Vec<_> = settle_owner(&mut accounts)
            .await
            .into_iter()
            .filter(|frame| matches!(frame, ControlToShell::GitHub(_)))
            .collect();
        assert_eq!(frames.len(), 2);
        assert!(
            frames
                .iter()
                .all(|frame| matches!(frame, ControlToShell::GitHub(GitHubView::SignedOut { .. })))
        );
        assert_eq!(
            frames.last(),
            Some(&ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: None
            }))
        );
        assert_eq!(
            accounts.github_opened(),
            vec![ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: None
            })]
        );
        assert_eq!(active_github(&scratch), 0);
        assert_eq!(
            store.writes.load(Ordering::SeqCst),
            usize::from(after_check)
        );
        assert!(accounts.github.machine().user().is_none());
        accounts.close(Instant::now(), Instant::now()).await;
    }
}

#[tokio::test]
async fn stale_credential_replies_settle_without_restoring_the_canceled_account() {
    let scratch = Scratch::new("github-owner-stale");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    let (jobs, queued) = std_mpsc::channel();
    accounts.jobs = Some(jobs);
    accounts.github.enable();
    accounts.github_session(Some(1));
    accounts
        .github
        .start(accounts.jobs.as_ref(), &accounts.reads_tx);
    let Work::GitHub {
        job: GitHubJob::Load { stamp: loaded },
        reply: load,
    } = queued.recv().unwrap()
    else {
        panic!("load")
    };
    let old = saving_owner(&mut accounts).await;
    let Work::GitHub {
        job: GitHubJob::Save { stamp, .. },
        reply: save,
    } = queued.recv().unwrap()
    else {
        panic!("save")
    };
    assert_eq!(old, stamp);
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    accounts.github_session(None);
    let Work::GitHub {
        job: GitHubJob::Remove { generation },
        reply: remove,
    } = queued.recv().unwrap()
    else {
        panic!("remove")
    };
    accounts.github_session(Some(2));
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    let Work::GitHub {
        job: GitHubJob::Remove { generation: newer },
        reply: newer_remove,
    } = queued.recv().unwrap()
    else {
        panic!("newer sign-out removal")
    };
    assert_eq!(newer, generation + 1);
    load.send(Reply::Loaded {
        stamp: loaded,
        result: Ok(Some(checked_account())),
    })
    .unwrap();
    save.send(Reply::Saved {
        stamp: old,
        saved: true,
    })
    .unwrap();
    for _ in 0..2 {
        one_reply(&mut accounts).await;
    }
    remove
        .send(Reply::Removed {
            generation,
            result: Removed::Clean,
        })
        .unwrap();
    assert_eq!(
        one_reply(&mut accounts).await,
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: Some(Cleanup::Running)
        })]
    );
    assert!(accounts.github.pending());
    newer_remove
        .send(Reply::Removed {
            generation: newer,
            result: Removed::Clean,
        })
        .unwrap();
    settle_owner(&mut accounts).await;
    assert_eq!(
        accounts.github_opened(),
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: None
        })]
    );
    assert!(accounts.github.machine().user().is_none());
    accounts.close(Instant::now(), Instant::now()).await;
}

use super::github::{VerificationEvent, VerificationOutcome, VerificationStage};
use crate::connections::http_client::{
    Endpoint, Http, Request, Response, ResponseMeta, SharedHttp,
};
use tokio::sync::Notify;

#[derive(Clone)]
pub(crate) struct UserHttp {
    reply: Arc<Mutex<Option<Result<Response, HttpError>>>>,
    pub(crate) calls: Arc<AtomicUsize>,
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) dropped: Arc<AtomicUsize>,
    pub(crate) entered: Arc<Notify>,
    hold: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    panic: bool,
}

struct UserActive(UserHttp);

impl Drop for UserActive {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl UserHttp {
    pub(crate) fn new(reply: Result<Response, HttpError>) -> Self {
        Self {
            reply: Arc::new(Mutex::new(Some(reply))),
            calls: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Notify::new()),
            hold: Arc::new(Mutex::new(None)),
            panic: false,
        }
    }

    pub(crate) fn hold(&self) -> oneshot::Sender<()> {
        let (release, held) = oneshot::channel();
        *self.hold.lock().unwrap() = Some(held);
        release
    }

    pub(crate) fn factory(&self) -> HttpFactory {
        let http = SharedHttp::new(self.clone());
        Arc::new(move || Ok(http.clone()))
    }
}

impl Http for UserHttp {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        assert!(matches!(
            request.endpoint,
            Endpoint::GitHub(crate::connections::github::GitHubEndpoint::User { .. })
        ));
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = UserActive(self.clone());
        self.entered.notify_one();
        let held = self.hold.lock().unwrap().take();
        if let Some(held) = held {
            held.await.unwrap();
        }
        assert!(!self.panic, "foreign-panic-sentinel");
        self.reply.lock().unwrap().take().expect("one user request")
    }
}

pub(crate) fn user_response(status: u16, body: &[u8], meta: ResponseMeta) -> Response {
    Response {
        status,
        body: Secret::new(Zeroizing::new(body.to_vec())),
        request_id: None,
        meta,
    }
}

fn connected_view(login: &str) -> GitHubView {
    GitHubView::Connected {
        login: GitHubLogin::new(login).unwrap(),
        repos: kumochi_scene::text::Bounded::default(),
        truncated: false,
        default: None,
    }
}

fn assert_private_verification(event: &VerificationEvent, accounts: &Accounts) {
    let debug = format!("{:?} {accounts:?}", Event::Verification(event.clone()));
    for marker in [
        "ghu_owner_sentinel",
        "ghr_owner_sentinel",
        "owner-sentinel",
        "fresh-user-sentinel",
        "private-provider-sentinel",
        "foreign-panic-sentinel",
    ] {
        assert!(!debug.contains(marker));
        assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    }
}

pub(crate) fn verification_accounts(
    scratch: &Scratch,
    factory: HttpFactory,
    origin: Option<u64>,
) -> (Accounts, ObservedStore) {
    let store = ObservedStore::default();
    store.seed(scratch);
    let mut net = crate::test_support::net();
    net.http = factory;
    let mut accounts = owner_accounts(scratch, store.clone(), net);
    accounts.github.enable();
    accounts.github.enable_verification();
    accounts.github_session(origin);
    accounts
        .github
        .start(accounts.jobs.as_ref(), &accounts.reads_tx);
    (accounts, store)
}

pub(crate) async fn verification_event(accounts: &mut Accounts) -> VerificationEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match accounts.next().await {
                Event::Verification(event) => break event,
                event => {
                    accounts.handle(event);
                }
            }
        }
    })
    .await
    .unwrap()
}

pub(crate) async fn start_user_request(accounts: &mut Accounts) {
    settle_owner(accounts).await;
    let built = verification_event(accounts).await;
    assert_eq!(built.stage, VerificationStage::Construction);
    assert!(matches!(built.outcome, VerificationOutcome::Built(Ok(_))));
    assert_eq!(
        accounts.handle(Event::Verification(built)),
        Vec::<ControlToShell>::new()
    );
}

async fn next_verifying_attempt(accounts: &mut Accounts) -> Stamp {
    tokio::time::pause();
    drive(accounts, GitHubInput::Op(GitHubOp::Connect));
    let stamp = accounts.github.machine().stamp();
    drive(
        accounts,
        GitHubInput::Code {
            stamp,
            result: Ok(device_challenge()),
        },
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    accounts
        .github
        .wake(
            stamp,
            accounts.jobs.as_ref(),
            &accounts.reads_tx,
            &accounts.net.http,
        )
        .unwrap();
    drive(
        accounts,
        GitHubInput::Poll {
            stamp,
            result: Ok(TokenReply::Token(checked_account().credentials)),
        },
    );
    tokio::time::resume();
    stamp
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the fixed response matrix keeps literal outcomes and identity expectations together"
)]
async fn loaded_verification_preserves_actual_user_api_results_and_never_resaves() {
    let meta = ResponseMeta {
        date_s: Some(100),
        retry_after_s: Some(7),
        retry_after_present: true,
        rate_remaining: Some(0),
        rate_reset_s: Some(107),
    };
    let cases = [
        (
            Ok(user_response(
                200,
                br#"{"id":7,"login":"fresh-user-sentinel"}"#,
                ResponseMeta::default(),
            )),
            Ok(GitHubUser {
                id: 7,
                login: GitHubLogin::new("fresh-user-sentinel").unwrap(),
            }),
            Some(7),
            None,
            connected_view("fresh-user-sentinel"),
        ),
        (
            Ok(user_response(
                200,
                br#"{"id":11,"login":"fresh-user-sentinel"}"#,
                ResponseMeta::default(),
            )),
            Ok(GitHubUser {
                id: 11,
                login: GitHubLogin::new("fresh-user-sentinel").unwrap(),
            }),
            None,
            Some(ErrorKey::RequestRefused),
            GitHubView::NeedsSignIn {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
            },
        ),
        (
            Ok(user_response(
                401,
                b"private-provider-sentinel",
                ResponseMeta::default(),
            )),
            Err(crate::connections::github::api::ApiError::Refused {
                status: 401,
                meta: ResponseMeta::default(),
            }),
            None,
            Some(ErrorKey::ResponseUnreadable),
            GitHubView::NeedsSignIn {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
            },
        ),
        (
            Ok(user_response(403, b"private-provider-sentinel", meta)),
            Err(crate::connections::github::api::ApiError::Refused { status: 403, meta }),
            None,
            Some(ErrorKey::ResponseUnreadable),
            GitHubView::NotVerified {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
            },
        ),
        (
            Err(HttpError::Transport),
            Err(crate::connections::github::api::ApiError::Http(
                HttpError::Transport,
            )),
            None,
            Some(ErrorKey::AccountOffline),
            GitHubView::NotVerified {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
            },
        ),
        (
            Ok(user_response(200, b"{", ResponseMeta::default())),
            Err(crate::connections::github::api::ApiError::Response),
            None,
            Some(ErrorKey::ResponseUnreadable),
            GitHubView::NotVerified {
                login: GitHubLogin::new("owner-sentinel").unwrap(),
            },
        ),
    ];
    for (response, expected, user, error, view) in cases {
        let scratch = Scratch::new("github-loaded-user-matrix");
        let http = UserHttp::new(response);
        let (mut accounts, store) = verification_accounts(&scratch, http.factory(), Some(1));
        let authority = rusqlite::Connection::open(
            scratch
                .db()
                .with_file_name(crate::connections::authority::FILE),
        )
        .unwrap();
        let head = || {
            authority
                .query_row(
                    "SELECT revision FROM heads WHERE kind='github'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        let before = head();
        start_user_request(&mut accounts).await;
        let event = verification_event(&mut accounts).await;
        assert_eq!(event.stage, VerificationStage::Request);
        assert_eq!(event.stamp, Stamp { generation: 0 });
        assert_eq!(event.session, Some(1));
        let VerificationOutcome::User(result) = &event.outcome else {
            panic!("real user response")
        };
        assert_eq!(*result, expected);
        assert_private_verification(&event, &accounts);
        let mut frames = error
            .map(|key| ControlToShell::GitHubError(key.into()))
            .into_iter()
            .collect::<Vec<_>>();
        frames.push(ControlToShell::GitHub(view.clone()));
        assert_eq!(accounts.handle(Event::Verification(event)), frames);
        assert_eq!(accounts.github.machine().view(Instant::now()), view);
        assert_eq!(accounts.github.machine().user().map(|user| user.id), user);
        if user.is_none() {
            assert!(accounts.github.machine().token().is_none());
            assert!(accounts.github.machine().repositories().is_none());
        }
        assert_eq!(accounts.github.machine().stamp(), Stamp { generation: 1 });
        if user == Some(7) {
            assert!(matches!(
                accounts.github.suspended(),
                [
                    (
                        Stamp { generation: 1 },
                        GitHubEffect::RefreshRepos { generation: 1 }
                    ),
                    (Stamp { generation: 1 }, GitHubEffect::Wake(_))
                ]
            ));
        } else {
            assert_eq!(accounts.github.suspended().len(), 0);
        }
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(active_github(&scratch), 1);
        assert_eq!(head(), before);
        assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        assert!(!accounts.github.pending());
        assert!(!accounts.github.verifying());
        accounts.close(Instant::now(), Instant::now()).await;
    }
}

#[tokio::test]
async fn genuine_factory_refusal_uses_api_semantics_but_factory_and_request_panics_do_not() {
    for mode in 0..3 {
        let scratch = Scratch::new("github-verification-lifecycle");
        let mut http = UserHttp::new(Ok(user_response(
            200,
            br#"{"id":11,"login":"fresh-user-sentinel"}"#,
            ResponseMeta::default(),
        )));
        http.panic = mode == 2;
        let client = http.factory();
        let factory: HttpFactory = Arc::new(move || match mode {
            0 => Err(HttpError::Configuration),
            1 => panic!("foreign-panic-sentinel"),
            _ => client(),
        });
        let (mut accounts, store) = verification_accounts(&scratch, factory, Some(1));
        settle_owner(&mut accounts).await;
        let built = verification_event(&mut accounts).await;
        let frames = if mode == 2 {
            accounts.handle(Event::Verification(built));
            let failed = verification_event(&mut accounts).await;
            assert!(matches!(failed.outcome, VerificationOutcome::Stopped));
            assert_private_verification(&failed, &accounts);
            accounts.handle(Event::Verification(failed))
        } else {
            if mode == 0 {
                assert!(matches!(
                    built.outcome,
                    VerificationOutcome::Built(Err(HttpError::Configuration))
                ));
            } else {
                assert!(matches!(built.outcome, VerificationOutcome::Stopped));
            }
            assert_private_verification(&built, &accounts);
            accounts.handle(Event::Verification(built))
        };
        if mode == 0 {
            assert_eq!(
                frames,
                vec![
                    ControlToShell::GitHubError(ErrorKey::AccountOffline.into()),
                    ControlToShell::GitHub(GitHubView::NotVerified {
                        login: GitHubLogin::new("owner-sentinel").unwrap(),
                    })
                ]
            );
            assert!(accounts.github.machine().user().is_none());
        } else {
            assert_eq!(
                frames,
                vec![
                    ControlToShell::GitHubError(ErrorKey::RequestRefused.into()),
                    ControlToShell::GitHub(GitHubView::NeedsSignIn {
                        login: GitHubLogin::new("owner-sentinel").unwrap(),
                    }),
                ]
            );
            assert_eq!(
                accounts.github.machine().view(Instant::now()),
                GitHubView::NeedsSignIn {
                    login: GitHubLogin::new("owner-sentinel").unwrap(),
                }
            );
            assert!(accounts.github.machine().user().is_none());
        }
        assert_eq!(http.calls.load(Ordering::SeqCst), usize::from(mode == 2));
        assert_eq!(http.active.load(Ordering::SeqCst), 0);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(active_github(&scratch), 1);
        assert_eq!(accounts.github.suspended().len(), 0);
        assert!(!accounts.github.verifying());
        accounts.close(Instant::now(), Instant::now()).await;
    }
}

#[tokio::test]
async fn a_held_user_future_times_out_at_fifteen_seconds_and_is_joined_after_drop() {
    let scratch = Scratch::new("github-user-deadline");
    let http = UserHttp::new(Ok(user_response(200, b"{}", ResponseMeta::default())));
    let _release = http.hold();
    let (mut accounts, _) = verification_accounts(&scratch, http.factory(), Some(1));
    start_user_request(&mut accounts).await;
    http.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(14)).await;
    assert_eq!(http.active.load(Ordering::SeqCst), 1);
    {
        let next = accounts.next();
        tokio::pin!(next);
        let state = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(next.as_mut(), cx))
        })
        .await;
        assert!(matches!(state, std::task::Poll::Pending));
    }
    assert!(accounts.github.verifying());
    tokio::time::advance(Duration::from_secs(1)).await;
    let event = accounts.github.next_verification().await;
    assert!(matches!(
        event.outcome,
        VerificationOutcome::User(Err(crate::connections::github::api::ApiError::Http(
            HttpError::Timeout
        )))
    ));
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
    assert_eq!(http.dropped.load(Ordering::SeqCst), 1);
    accounts.handle(Event::Verification(event));
    tokio::time::resume();
    assert!(accounts.github.machine().user().is_none());
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn sign_out_aborts_and_joins_the_exact_user_request_without_reviving_its_success() {
    let scratch = Scratch::new("github-user-cancel");
    let http = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":11,"login":"fresh-user-sentinel"}"#,
        ResponseMeta::default(),
    )));
    let release = http.hold();
    let (mut accounts, _) = verification_accounts(&scratch, http.factory(), Some(1));
    start_user_request(&mut accounts).await;
    http.entered.notified().await;
    drive(&mut accounts, GitHubInput::Op(GitHubOp::CancelConnect));
    assert!(accounts.github.verifying());
    assert_eq!(
        accounts.github.machine().view(Instant::now()),
        GitHubView::NotVerified {
            login: GitHubLogin::new("owner-sentinel").unwrap()
        }
    );
    assert_eq!(http.active.load(Ordering::SeqCst), 1);
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    release.send(()).ok();
    let event = verification_event(&mut accounts).await;
    assert!(matches!(event.outcome, VerificationOutcome::Stopped));
    assert_eq!(event.stamp, Stamp { generation: 0 });
    assert_eq!(
        accounts.handle(Event::Verification(event)),
        Vec::<ControlToShell>::new()
    );
    settle_owner(&mut accounts).await;
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
    assert_eq!(http.dropped.load(Ordering::SeqCst), 1);
    assert!(!accounts.github.verifying());
    assert!(accounts.github.machine().user().is_none());
    assert_eq!(
        accounts.github.machine().view(Instant::now()),
        GitHubView::SignedOut { cleanup: None }
    );
    assert_eq!(active_github(&scratch), 0);
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn canceling_a_saved_check_preserves_credentials_but_requires_explicit_reverification() {
    let scratch = Scratch::new("github-saved-check-cancel");
    let first = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":7,"login":"fresh-user-sentinel"}"#,
        ResponseMeta::default(),
    )));
    let release = first.hold();
    let (mut accounts, store) = verification_accounts(&scratch, first.factory(), Some(1));
    let saved = store.store.snapshot();
    start_user_request(&mut accounts).await;
    first.entered.notified().await;
    let unchecked = GitHubView::NotVerified {
        login: GitHubLogin::new("owner-sentinel").unwrap(),
    };
    assert_eq!(
        drive(&mut accounts, GitHubInput::Op(GitHubOp::CancelConnect)),
        vec![ControlToShell::GitHub(unchecked.clone())]
    );
    let stopped = verification_event(&mut accounts).await;
    assert!(matches!(stopped.outcome, VerificationOutcome::Stopped));
    assert_eq!(
        accounts.handle(Event::Verification(stopped.clone())),
        Vec::<ControlToShell>::new()
    );
    settle_owner(&mut accounts).await;
    assert_eq!(first.active.load(Ordering::SeqCst), 0);
    assert_eq!(first.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(store.store.snapshot(), saved);
    assert!(release.send(()).is_err());
    assert!(!accounts.github.verifying());
    assert!(!accounts.github.pending());
    assert_eq!(accounts.github.machine().user(), None);
    assert!(accounts.github.machine().token().is_none());
    assert_eq!(accounts.github.machine().repositories(), None);
    assert_eq!(active_github(&scratch), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    for outcome in [
        VerificationOutcome::User(Ok(GitHubUser {
            id: 7,
            login: GitHubLogin::new("late-user").unwrap(),
        })),
        VerificationOutcome::User(Err(crate::connections::github::api::ApiError::Http(
            HttpError::Transport,
        ))),
    ] {
        let mut late = stopped.clone();
        late.outcome = outcome;
        assert_eq!(
            accounts.handle(Event::Verification(late)),
            Vec::<ControlToShell>::new()
        );
        assert_eq!(accounts.github.machine().view(Instant::now()), unchecked);
        assert_eq!(accounts.github.machine().user(), None);
    }
    tokio::time::pause();
    assert!(
        tokio::time::timeout(Duration::from_hours(9), accounts.github.next_verification())
            .await
            .is_err()
    );
    tokio::time::resume();
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    let second = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":7,"login":"fresh-user-sentinel"}"#,
        ResponseMeta::default(),
    )));
    accounts.net.http = second.factory();
    drive(&mut accounts, GitHubInput::Op(GitHubOp::RefreshRepos));
    start_user_request(&mut accounts).await;
    let checked = verification_event(&mut accounts).await;
    accounts.handle(Event::Verification(checked));
    assert_eq!(accounts.github.machine().user().unwrap().id, 7);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    assert_eq!(active_github(&scratch), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert_eq!(store.store.snapshot(), saved);
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn held_construction_is_joined_after_credential_release_even_without_a_release_receiver() {
    for (missing_release, sign_out) in [(false, true), (true, true), (false, false)] {
        let scratch = Scratch::new("github-factory-close-order");
        let store = ObservedStore::default();
        store.seed(&scratch);
        let http = UserHttp::new(Ok(user_response(200, b"{}", ResponseMeta::default())));
        let client = http.factory();
        let (entered, heard) = std_mpsc::channel();
        let (release, resume) = std_mpsc::channel();
        let resume = Mutex::new(resume);
        let mut net = crate::test_support::net();
        net.http = Arc::new(move || {
            entered.send(()).unwrap();
            resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            client()
        });
        let mut accounts = owner_accounts(&scratch, store, net);
        accounts.github.enable();
        accounts.github.enable_verification();
        accounts.github_session(Some(1));
        accounts
            .github
            .start(accounts.jobs.as_ref(), &accounts.reads_tx);
        settle_owner(&mut accounts).await;
        heard.recv_timeout(Duration::from_secs(5)).unwrap();
        {
            let event = accounts.next();
            tokio::pin!(event);
            let state = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(event.as_mut(), cx))
            })
            .await;
            assert!(matches!(state, std::task::Poll::Pending));
        }
        if sign_out {
            drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
            assert!(accounts.github.pending());
        }
        if missing_release {
            accounts.released = None;
        }
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let closing = async {
            accounts.close(Instant::now(), Instant::now()).await;
            done.store(true, Ordering::SeqCst);
        };
        let witness = async {
            let lock = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(lock) =
                        crate::platform::SessionLock::try_acquire(&owner_lock(&scratch))
                    {
                        break lock;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!finished.load(Ordering::SeqCst));
            assert_eq!(active_github(&scratch), i64::from(!sign_out));
            assert_eq!(http.calls.load(Ordering::SeqCst), 0);
            let sibling = tokio::spawn(async { 17 });
            assert_eq!(sibling.await.unwrap(), 17);
            release.send(()).unwrap();
            drop(lock);
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(closing, witness);
        })
        .await
        .unwrap();
        assert!(finished.load(Ordering::SeqCst));
        assert!(!accounts.github.verifying());
        assert!(accounts.github.machine().user().is_none());
        assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn the_request_body_rechecks_the_real_fence_before_its_first_poll() {
    let scratch = Scratch::new("github-user-final-fence");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    accounts.github.enable();
    let stamp = accounts.github.machine().stamp();
    let fence = accounts.github.machine().fence();
    let http = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":11,"login":"fresh-user-sentinel"}"#,
        ResponseMeta::default(),
    )));
    let future = super::github::read_user(
        stamp,
        fence,
        checked_account().credentials.token,
        SharedHttp::new(http.clone()),
    );
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    assert_eq!(future.await, None);
    assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    settle_owner(&mut accounts).await;
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn an_actual_completed_old_event_cannot_clear_a_new_verification_operation() {
    let scratch = Scratch::new("github-user-operation-identity");
    let first = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":11,"login":"fresh-user-sentinel"}"#,
        ResponseMeta::default(),
    )));
    let (mut accounts, _) = verification_accounts(&scratch, first.factory(), Some(1));
    start_user_request(&mut accounts).await;
    let old = verification_event(&mut accounts).await;
    let old_id = old.id;
    assert!(matches!(old.outcome, VerificationOutcome::User(Ok(_))));
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    assert!(!accounts.github.verifying());
    settle_owner(&mut accounts).await;
    let second = UserHttp::new(Ok(user_response(
        200,
        br#"{"id":12,"login":"second"}"#,
        ResponseMeta::default(),
    )));
    let release = second.hold();
    accounts.net.http = second.factory();
    let stamp = next_verifying_attempt(&mut accounts).await;
    assert!(accounts.github.verifying());
    assert_eq!(
        accounts.handle(Event::Verification(old)),
        Vec::<ControlToShell>::new()
    );
    assert!(accounts.github.verifying());
    assert!(accounts.github.machine().user().is_none());
    let built = verification_event(&mut accounts).await;
    assert_ne!(built.id, old_id);
    assert_eq!(built.stamp, stamp);
    accounts.handle(Event::Verification(built));
    second.entered.notified().await;
    release.send(()).unwrap();
    let current = verification_event(&mut accounts).await;
    assert_ne!(current.id, old_id);
    accounts.handle(Event::Verification(current));
    settle_owner(&mut accounts).await;
    assert_eq!(accounts.github.machine().user().unwrap().id, 12);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn a_retiring_factory_cannot_be_replaced_and_does_not_replay_new_suspended_work() {
    let scratch = Scratch::new("github-user-retiring-operation");
    let http = UserHttp::new(Ok(user_response(200, b"{}", ResponseMeta::default())));
    let client = http.factory();
    let calls = Arc::new(AtomicUsize::new(0));
    let builds = Arc::clone(&calls);
    let (entered, heard) = std_mpsc::channel();
    let (release, resume) = std_mpsc::channel();
    let resume = Mutex::new(resume);
    let factory: HttpFactory = Arc::new(move || {
        builds.fetch_add(1, Ordering::SeqCst);
        entered.send(()).unwrap();
        resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        client()
    });
    let (mut accounts, _) = verification_accounts(&scratch, factory, Some(1));
    settle_owner(&mut accounts).await;
    heard.recv_timeout(Duration::from_secs(5)).unwrap();
    drive(&mut accounts, GitHubInput::Op(GitHubOp::SignOut));
    settle_owner(&mut accounts).await;
    let stamp = next_verifying_attempt(&mut accounts).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(accounts.github.suspended(), [(wake, GitHubEffect::Wake(_)), (owner, GitHubEffect::ReadUser { stamp: read, .. })] if *wake == stamp && *owner == stamp && *read == stamp)
    );
    release.send(()).unwrap();
    let cancelled = verification_event(&mut accounts).await;
    assert_eq!(cancelled.stamp, Stamp { generation: 0 });
    assert_eq!(
        accounts.handle(Event::Verification(cancelled)),
        Vec::<ControlToShell>::new()
    );
    assert!(!accounts.github.verifying());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    assert!(accounts.github.machine().user().is_none());
    assert!(
        matches!(accounts.github.suspended(), [(wake, GitHubEffect::Wake(_)), (owner, GitHubEffect::ReadUser { .. })] if *wake == stamp && *owner == stamp)
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn verification_keeps_the_load_origin_when_the_owner_session_changes_before_delivery() {
    let scratch = Scratch::new("github-user-load-origin");
    let http = UserHttp::new(Ok(user_response(
        403,
        b"provider-sentinel",
        ResponseMeta::default(),
    )));
    let (mut accounts, _) = verification_accounts(&scratch, http.factory(), None);
    accounts.github_session(Some(1));
    settle_owner(&mut accounts).await;
    let built = verification_event(&mut accounts).await;
    assert_eq!(built.session, None);
    accounts.handle(Event::Verification(built));
    let user = verification_event(&mut accounts).await;
    assert_eq!(user.session, None);
    assert_eq!(
        accounts.handle(Event::Verification(user)),
        vec![ControlToShell::GitHub(GitHubView::NotVerified {
            login: GitHubLogin::new("owner-sentinel").unwrap(),
        })]
    );
    assert!(accounts.github.machine().user().is_none());
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn a_stale_wake_cannot_poll_a_new_attempt_and_unexecuted_effects_keep_their_stamp() {
    let scratch = Scratch::new("github-owner-wakes");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    accounts.github.enable();
    accounts.github_session(Some(1));
    drive(
        &mut accounts,
        GitHubInput::Loaded {
            stamp: Stamp { generation: 0 },
            result: Ok(None),
        },
    );
    drive(&mut accounts, GitHubInput::Op(GitHubOp::Connect));
    let old = accounts.github.machine().stamp();
    drive(
        &mut accounts,
        GitHubInput::Code {
            stamp: old,
            result: Ok(device_challenge()),
        },
    );
    accounts.github_session(None);
    accounts.github_session(Some(2));
    drive(&mut accounts, GitHubInput::Op(GitHubOp::Connect));
    let current = accounts.github.machine().stamp();
    assert_ne!(current, old);
    drive(
        &mut accounts,
        GitHubInput::Code {
            stamp: current,
            result: Ok(device_challenge()),
        },
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    let http = &accounts.net.http;
    assert!(
        accounts
            .github
            .wake(old, accounts.jobs.as_ref(), &accounts.reads_tx, http)
            .is_err()
    );
    assert!(
        accounts
            .github
            .suspended()
            .iter()
            .all(|(_, effect)| !matches!(effect, GitHubEffect::Poll { .. }))
    );
    accounts
        .github
        .wake(current, accounts.jobs.as_ref(), &accounts.reads_tx, http)
        .unwrap();
    assert_eq!(
        accounts
            .github
            .suspended()
            .iter()
            .filter(|(_, effect)| matches!(effect, GitHubEffect::Poll { .. }))
            .count(),
        1
    );
    assert!(
        accounts
            .github
            .wake(current, accounts.jobs.as_ref(), &accounts.reads_tx, http)
            .is_err()
    );
    assert!(
        accounts
            .github
            .checked(
                GitHubInput::Op(GitHubOp::SetDefaultRepo(
                    RepoName::new("owner/repo").unwrap()
                )),
                accounts.jobs.as_ref(),
                &accounts.reads_tx,
                http
            )
            .is_err()
    );
    assert!(accounts.github.machine().user().is_none());
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn a_load_failure_queued_under_an_old_session_is_not_delivered_to_its_replacement() {
    let scratch = Scratch::new("github-owner-old-error");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    let (jobs, queued) = std_mpsc::channel();
    accounts.jobs = Some(jobs);
    accounts.github.enable();
    accounts.github_session(Some(1));
    accounts
        .github
        .start(accounts.jobs.as_ref(), &accounts.reads_tx);
    let Work::GitHub {
        job: GitHubJob::Load { stamp },
        reply,
    } = queued.recv().unwrap()
    else {
        panic!("load")
    };
    assert_eq!(stamp, Stamp { generation: 0 });
    drop(reply);
    accounts.github_session(None);
    accounts.github_session(Some(2));
    assert_eq!(
        settle_owner(&mut accounts).await,
        Vec::<ControlToShell>::new()
    );
    assert!(accounts.github.machine().is_loading());
    assert_eq!(accounts.github_opened(), Vec::<ControlToShell>::new());
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn closed_save_and_removal_replies_fail_closed_and_a_late_success_cannot_revive_them() {
    let scratch = Scratch::new("github-owner-closed-durable-reply");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    let (jobs, queued) = std_mpsc::channel();
    accounts.jobs = Some(jobs);
    accounts.github.enable();
    accounts.github_session(Some(1));
    let stamp = saving_owner(&mut accounts).await;
    let Work::GitHub {
        job: GitHubJob::Save { stamp: saved, .. },
        reply,
    } = queued.recv().unwrap()
    else {
        panic!("save")
    };
    assert_eq!(stamp, saved);
    drop(reply);
    let frames = one_reply(&mut accounts).await;
    assert_eq!(
        frames,
        vec![
            ControlToShell::GitHubError(ErrorKey::CredentialsUnavailable.into()),
            ControlToShell::GitHub(GitHubView::SignedOut {
                cleanup: Some(Cleanup::Running)
            })
        ]
    );
    let Work::GitHub {
        job: GitHubJob::Remove { generation },
        reply,
    } = queued.recv().unwrap()
    else {
        panic!("compensating remove")
    };
    assert_eq!(generation, stamp.generation + 1);
    drop(reply);
    assert_eq!(
        settle_owner(&mut accounts).await,
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: Some(Cleanup::NotDurable)
        })]
    );
    assert!(accounts.github.machine().user().is_none());
    drive(&mut accounts, GitHubInput::Saved { stamp, saved: true });
    assert_eq!(
        accounts.github_opened(),
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: Some(Cleanup::NotDurable)
        })]
    );
    accounts.github_session(None);
    accounts.github_session(Some(2));
    assert_eq!(
        accounts.github_opened(),
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: Some(Cleanup::NotDurable)
        })]
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn a_late_save_success_cannot_publish_a_new_attempt_that_is_still_saving() {
    let scratch = Scratch::new("github-owner-save-identity");
    let mut accounts = owner_accounts(
        &scratch,
        ObservedStore::default(),
        crate::test_support::net(),
    );
    let (jobs, queued) = std_mpsc::channel();
    accounts.jobs = Some(jobs);
    accounts.github.enable();
    accounts.github_session(Some(1));
    let old = saving_owner(&mut accounts).await;
    let Work::GitHub {
        job: GitHubJob::Save { .. },
        reply: old_save,
    } = queued.recv().unwrap()
    else {
        panic!("old save")
    };
    accounts.github_session(None);
    let Work::GitHub {
        job: GitHubJob::Remove { generation },
        reply,
    } = queued.recv().unwrap()
    else {
        panic!("old removal")
    };
    reply
        .send(Reply::Removed {
            generation,
            result: Removed::Clean,
        })
        .unwrap();
    one_reply(&mut accounts).await;
    accounts.github_session(Some(2));
    let current = saving_owner(&mut accounts).await;
    assert_ne!(current, old);
    let Work::GitHub {
        job: GitHubJob::Save { stamp, .. },
        reply: current_save,
    } = queued.recv().unwrap()
    else {
        panic!("current save")
    };
    assert_eq!(stamp, current);
    old_save
        .send(Reply::Saved {
            stamp: old,
            saved: true,
        })
        .unwrap();
    assert_eq!(
        one_reply(&mut accounts).await,
        vec![ControlToShell::GitHub(GitHubView::Requesting)]
    );
    assert!(accounts.github.machine().user().is_none());
    current_save
        .send(Reply::Saved {
            stamp: current,
            saved: false,
        })
        .unwrap();
    one_reply(&mut accounts).await;
    let Work::GitHub {
        job: GitHubJob::Remove { .. },
        reply,
    } = queued.recv().unwrap()
    else {
        panic!("current removal")
    };
    drop(reply);
    settle_owner(&mut accounts).await;
    assert_eq!(
        accounts.github_opened(),
        vec![ControlToShell::GitHub(GitHubView::SignedOut {
            cleanup: Some(Cleanup::NotDurable)
        })]
    );
    accounts.close(Instant::now(), Instant::now()).await;
}
