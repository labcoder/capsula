use super::*;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, HttpError, Request, Response, SharedHttp};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};
use crate::runtime::account::Net;
use crate::test_support::{self, Scratch};
use std::sync::Arc;
use tokio::sync::Notify;
use zeroize::Zeroizing;

mod staging;

#[tokio::test]
async fn production_key_dispatch_is_unserved_without_asking_to_send() {
    use crate::context::{ModelRequest, Origin};
    use crate::coordinator::{BuildKey, SendKey};
    use crate::engines::{Asks, EngineFailure, Finish, RunRequest, SendGate, TurnEvent};
    use kumochi_protocol::message::{Effort, ModelChoice, ModelSlug, RunId, ThreadId};

    let scratch = Scratch::new("dormant-key-transport");
    let mut brain = test_support::brain(&scratch, "");
    assert!(brain.account.connections.route_input().is_none());
    let key = SendKey::Build(BuildKey {
        run: RunId(1),
        round: 1,
        attempt: 1,
    });
    let request = RunRequest {
        key,
        run: RunId(1),
        thread: ThreadId(1),
        origin: Origin::Kumo,
        request: ModelRequest {
            choice: Some(ModelChoice {
                connection: ConnectionKind::AnthropicKey,
                model: ModelSlug::new("claude-sonnet-5-5").expect("model"),
                effort: Effort::Medium,
            }),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
        },
    };
    let (events, mut seen) = tokio::sync::mpsc::channel(1);
    let (inputs, mut asks) = tokio::sync::mpsc::unbounded_channel();
    brain
        .owned_engine(&request)
        .expect("owned refusal")
        .start(request, events, SendGate::new(key, Asks::default(), inputs))
        .await;
    assert_eq!(
        seen.recv().await,
        Some(TurnEvent::Finished(Finish::Failed(EngineFailure::Access(
            Some(model_unavailable())
        ))))
    );
    assert_eq!(asks.recv().await, None);
    brain.close().await;
}

fn key() -> Secret<AnthropicKey> {
    AnthropicKey::parse(Secret::new(Zeroizing::new(
        b"sk-ant-api-zqxsentinellabel".to_vec(),
    )))
    .expect("fake key")
}

fn accounts(scratch: &Scratch, http: SharedHttp) -> Accounts {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(move || Ok(http.clone())),
            ..test_support::net()
        },
    )
    .expect("account worker");
    accounts.connections.loading = true;
    accounts.connections.view.anthropic = KeyView::Checking;
    accounts
}

struct Held(Arc<Notify>);

impl Http for Held {
    async fn execute(&self, _: Request) -> Result<Response, HttpError> {
        self.0.notified().await;
        Ok(Response {
            status: 200,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        })
    }
}

struct EmptyCatalog;

impl Http for EmptyCatalog {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> {
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        }))
    }
}

fn save_key(store: &FakeStore, lock: &std::path::Path) {
    Vault::with_service(store.clone(), "key-test", lock)
        .expect("seed vault")
        .save_anthropic_key(b"sk-ant-api-fake-saved-value")
        .expect("saved key");
}

async fn settle(accounts: &mut Accounts) {
    timeout(Duration::from_secs(5), async {
        while accounts.connections.loading || accounts.connections.read.is_some() {
            let event = accounts.next().await;
            accounts.handle(event);
        }
    })
    .await
    .expect("the key settles");
}

#[tokio::test]
async fn a_staged_paste_is_refused_and_the_saved_key_still_checks_out() {
    let scratch = Scratch::new("key-staged-refused");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let owned = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(owned.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(|| Ok(SharedHttp::new(EmptyCatalog))),
            ..test_support::net()
        },
    )
    .expect("worker");
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    let paste = Secret::new(Zeroizing::new(b"sk-ant-api-fake-staged-value".to_vec()));
    store
        .set("anthropic-key-staged.key-test", &paste)
        .expect("fake paste");
    accounts.connections.stage_session(Some(1));
    let op = ConnectionOp::AnthropicKeyStaged {
        plan: KeyStageId([0; 16]),
    };
    assert_eq!(
        accounts.stage_op(1, op),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: KeyStageId([0; 16]),
            result: Err(KeyStageFailure::Changed)
        }]
    );
    assert_eq!(
        accounts.stage_op(1, op),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: KeyStageId([0; 16]),
            result: Err(KeyStageFailure::Changed)
        }]
    );
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    assert_eq!(
        accounts
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some("sk-ant-api-fake-saved-value")
    );
    assert_eq!(
        store.get("anthropic-key-staged.key-test").expect("staged"),
        Some(paste),
        "the paste is left for C7's cleanup"
    );
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    assert_eq!(
        accounts
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some("sk-ant-api-fake-saved-value")
    );
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

/// Blocks the first read of a current key's chunk until the test lets it go, as a native
/// Credential Manager call that hangs would.
struct HeldStore {
    armed: Arc<std::sync::atomic::AtomicBool>,
    store: FakeStore,
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl CredentialStore for HeldStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        if target.starts_with("key-test/gen/anthropic/")
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
            && let Some(resume) = self.resume.lock().expect("gate").take()
        {
            self.entered.send(()).expect("test listening");
            resume
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("test releases the worker");
        }
        self.store.get(target)
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.store.set(target, secret)
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.store.delete(target)
    }
}

/// CAPS-179: a removal sent while a native read is blocked wins. It runs after the read on the
/// same worker, and the late read's key is dropped by its stale generation.
#[tokio::test]
async fn a_remove_sent_while_a_native_read_is_blocked_wins() {
    let scratch = Scratch::new("key-worker-order");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut held = Some(HeldStore {
        armed: Arc::clone(&armed),
        store: store.clone(),
        entered: entered_tx,
        resume: std::sync::Mutex::new(Some(resume_rx)),
    });
    let worker_lock = lock.clone();
    let worker = Worker::new(
        Box::new(move || {
            Vault::with_service(held.take().expect("one open"), "key-test", &worker_lock)
        }),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read is held");
    })
    .await
    .expect("gate joined");
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 1,
        request: 1,
    }));
    resume.send(()).expect("release the read");
    timeout(Duration::from_secs(5), async {
        while accounts.connections.view.anthropic != KeyView::Missing {
            let event = accounts.next().await;
            accounts.handle(event);
        }
    })
    .await
    .expect("the removal completes after the read");
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    assert_eq!(accounts.connections.read.as_ref().map(|_| ()), None);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
    let mut vault = Vault::with_service(store.clone(), "key-test", &lock).expect("reopen");
    assert_eq!(
        vault
            .anthropic(crate::connections::anthropic::KeyOp::Load)
            .1
            .expect("read")
            .map(|_| ()),
        None,
        "the key stays removed"
    );
    assert!(
        store
            .names()
            .iter()
            .all(|name| !name.starts_with("key-test/gen/anthropic/")),
        "and its names are swept"
    );
}

/// Counts catalog requests, the only authenticated work a key admits.
struct CountedCatalog(Arc<std::sync::atomic::AtomicUsize>);

impl Http for CountedCatalog {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        }))
    }
}

fn authority_sql(lock: &std::path::Path, sql: &str) {
    rusqlite::Connection::open(lock.with_file_name(crate::connections::authority::FILE))
        .expect("authority file")
        .execute_batch(sql)
        .expect("authority statement");
}

const REFUSE_HEADS: &str = "CREATE TRIGGER refuse_heads BEFORE UPDATE ON heads \
                            BEGIN SELECT RAISE(ABORT, 'refused'); END;";

async fn expect_removal_receipt(
    accounts: &mut Accounts,
    request: u64,
    view: KeyView,
    result: Result<(), KeyRemovalFailure>,
) {
    let event = timeout(Duration::from_secs(5), accounts.next())
        .await
        .expect("removal outcome");
    assert_eq!(
        accounts.handle(event),
        [
            ControlToShell::Connections(ConnectionsView {
                anthropic: view,
                anthropic_revision: None,
                claude_code: ClaudeCodeView::NotFound,
            }),
            ControlToShell::AnthropicKeyRemoved { request, result },
        ]
    );
}

/// CAPS-176 233-2-H1, design §5: a Remove whose commit fails stays pending. A Check queued
/// behind it or sent after its failure publishes no key and requests nothing; each retries the
/// removal instead, and the first one that commits ends Missing.
#[tokio::test]
async fn a_failed_removal_stays_pending_until_a_retry_commits_it() {
    let scratch = Scratch::new("key-remove-pending");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let (owned, worker_lock) = (store.clone(), lock.clone());
    let worker = Worker::new(
        Box::new(move || Vault::with_service(owned.clone(), "key-test", &worker_lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(move || Ok(SharedHttp::new(CountedCatalog(Arc::clone(&counted))))),
            ..test_support::net()
        },
    )
    .expect("worker");
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    let sent = || requests.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(sent(), 1);
    authority_sql(&lock, REFUSE_HEADS);
    accounts.connections.stage_session(Some(1));
    let start = accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 1,
        request: 1,
    }));
    assert!(matches!(
        start.as_slice(),
        [ControlToShell::Connections(ConnectionsView {
            anthropic: KeyView::Checking,
            ..
        })]
    ));
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    expect_removal_receipt(
        &mut accounts,
        1,
        KeyView::Unconfirmed,
        Err(KeyRemovalFailure::Unconfirmed),
    )
    .await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    assert_eq!(
        sent(),
        1,
        "a Check queued behind the failed removal requests nothing"
    );
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(
        sent(),
        1,
        "Check retries the pending removal without reading a key"
    );
    authority_sql(&lock, "DROP TRIGGER refuse_heads;");
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 1,
        request: 2,
    }));
    expect_removal_receipt(&mut accounts, 2, KeyView::Missing, Ok(())).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Missing);
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Missing);
    assert_eq!(sent(), 1, "no Check after the failure requests anything");
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
    let mut vault = Vault::with_service(store.clone(), "key-test", &lock).expect("reopen");
    assert_eq!(
        vault
            .anthropic(crate::connections::anthropic::KeyOp::Load)
            .1
            .expect("read")
            .map(|_| ()),
        None,
        "the retried removal committed"
    );
}

#[tokio::test]
async fn changed_or_removed_keys_ignore_late_worker_and_check_results() {
    let scratch = Scratch::new("key-generation");
    let mut accounts = accounts(&scratch, SharedHttp::new(Held(Arc::new(Notify::new()))));
    let first = accounts.connections.generation;
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 1,
        request: 1,
    }));
    let current = accounts.connections.generation;
    assert_eq!(current, first + 1);
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Loaded {
            generation: first,
            result: (Some(1), Ok(Some(key())))
        }),
        vec![]
    );
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    accounts.connection_event(ConnectionEvent::Loaded {
        generation: current,
        result: (None, Ok(None)),
    });
    accounts.connection_event(ConnectionEvent::Checked {
        generation: first,
        result: Ok(vec![]),
    });
    assert_eq!(accounts.connections.view.anthropic, KeyView::Missing);
    accounts.stage_op(
        1,
        ConnectionOp::AnthropicKeyStaged {
            plan: KeyStageId([0; 16]),
        },
    );
    accounts.connection_event(ConnectionEvent::Checked {
        generation: current,
        result: Ok(vec![]),
    });
    assert_eq!(accounts.connections.view.anthropic, KeyView::Missing);
    assert_eq!(accounts.connections.models.len(), 0);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test(start_paused = true)]
async fn held_check_expires_and_valid_input_cannot_rescue_a_failed_attempt() {
    let scratch = Scratch::new("key-deadline");
    let mut accounts = accounts(&scratch, SharedHttp::new(Held(Arc::new(Notify::new()))));
    let generation = accounts.connections.generation;
    accounts.connection_event(ConnectionEvent::Loaded {
        generation,
        result: (Some(1), Ok(Some(key()))),
    });
    loop {
        let event = accounts.next().await;
        let checked = matches!(event, Event::Connection(ConnectionEvent::Checked { .. }));
        accounts.handle(event);
        if checked {
            break;
        }
    }
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(
        accounts
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some("sk-ant-api-zqxsentinellabel")
    );
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Checked {
            generation,
            result: Ok(vec![])
        }),
        vec![]
    );
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test(start_paused = true)]
async fn key_deadline_also_covers_a_held_client_factory() {
    let scratch = Scratch::new("key-factory-deadline");
    let mut accounts = accounts(&scratch, SharedHttp::new(Held(Arc::new(Notify::new()))));
    let entered = Arc::new(Notify::new());
    let notify = Arc::clone(&entered);
    let (resume, hold) = std::sync::mpsc::channel();
    let hold = std::sync::Mutex::new(hold);
    accounts.net.http = Arc::new(move || {
        notify.notify_one();
        hold.lock()
            .expect("factory gate")
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("factory released");
        Err(HttpError::Configuration)
    });
    let generation = accounts.connections.generation;
    let started = tokio::time::Instant::now();
    accounts.connection_event(ConnectionEvent::Loaded {
        generation,
        result: (Some(1), Ok(Some(key()))),
    });
    entered.notified().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let event = tokio::select! {
        biased;
        event = accounts.next() => Some(event),
        () = std::future::ready(()) => None,
    };
    resume.send(()).expect("release factory");
    let event = event.expect("the deadline reports while the client factory is still held");
    accounts.handle(event);
    let elapsed = started.elapsed();
    assert_eq!(elapsed, Duration::from_secs(15));
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn connection_replay_is_tagged_and_outer_carriers_redact_the_key() {
    let scratch = Scratch::new("key-replay");
    let mut accounts = accounts(&scratch, SharedHttp::new(Held(Arc::new(Notify::new()))));
    let generation = accounts.connections.generation;
    let event = ConnectionEvent::Loaded {
        generation,
        result: (Some(1), Ok(Some(key()))),
    };
    let text = format!("{event:?} {accounts:?}");
    assert!(!text.contains(test_support::SENTINEL), "no text marker");
    assert!(
        !text.contains(&format!("{:?}", test_support::SENTINEL.as_bytes())),
        "no byte-list marker"
    );
    accounts.connection_event(event);
    assert_eq!(
        accounts.connections.connected(),
        ControlToShell::Connections(ConnectionsView {
            anthropic: KeyView::Checking,
            anthropic_revision: Some(1),
            claude_code: ClaudeCodeView::NotFound
        })
    );
    accounts.connection_event(ConnectionEvent::Checked {
        generation,
        result: Ok(vec![]),
    });
    assert_eq!(
        accounts.connections.models_unsent(),
        Some(DataToShell::Models {
            connection: ConnectionKind::AnthropicKey,
            models: vec![]
        })
    );
    assert_eq!(accounts.connections.models_unsent(), None);
    accounts.connections.read = Some(tokio::spawn(std::future::pending()));
    accounts.connection_event(ConnectionEvent::Checked {
        generation,
        result: Err(CheckError::Rejected(
            kumochi_protocol::message::KeyRejection::Refused,
        )),
    });
    accounts.connection_event(ConnectionEvent::Checked {
        generation,
        result: Ok(vec![]),
    });
    assert_eq!(
        accounts.connections.view.anthropic,
        KeyView::Rejected(kumochi_protocol::message::KeyRejection::Refused)
    );
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}
