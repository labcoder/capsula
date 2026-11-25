use super::*;
use crate::connections::account::Worker;
use crate::connections::anthropic::tls::{Script, Scripted};
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, HttpError, Request, Response, SharedHttp};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};
use crate::runtime::account::Net;
use crate::test_support::{self, Scratch};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use zeroize::Zeroizing;

mod staging;

#[tokio::test]
async fn production_composition_serves_the_key_route_and_refuses_unadmitted_sends_unasked() {
    let scratch = Scratch::new("dormant-key-transport");
    let mut brain = test_support::brain(&scratch, "");
    let unavailable = if cfg!(windows) {
        model_unavailable()
    } else {
        ErrorKey::UnsupportedPlatform.into()
    };
    assert!(matches!(
        brain.account.connections.route_input(),
        Some(Input::Engines(
            ConnectionKind::AnthropicKey,
            Availability::Unavailable(refusal),
        )) if refusal == unavailable
    ));
    assert_owned_key_refusal(&mut brain, unavailable).await;
    #[cfg(not(windows))]
    {
        brain.account.connections.stage_session(Some(1));
        assert_eq!(
            brain.account.stage_op(
                1,
                ConnectionOp::BeginAnthropicKeyStage {
                    request: 1,
                    len: 16
                }
            ),
            [ControlToShell::AnthropicKeyStage {
                request: 1,
                result: Err(KeyStageFailure::Unavailable),
            }]
        );
    }
    brain.close().await;
}

async fn assert_owned_key_refusal(brain: &mut Brain, unavailable: UserError) {
    use crate::context::{ModelRequest, Origin};
    use crate::coordinator::{BuildKey, SendKey};
    use crate::engines::{Asks, EngineFailure, Finish, RunRequest, SendGate, TurnEvent};
    use kumochi_protocol::message::{Effort, ModelChoice, ModelSlug, RunId, ThreadId};

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
    timeout(Duration::from_secs(5), async {
        brain
            .owned_engine(&request)
            .expect("owned refusal")
            .start(request, events, SendGate::new(key, Asks::default(), inputs))
            .await;
        assert_eq!(
            seen.recv().await,
            Some(TurnEvent::Finished(Finish::Failed(EngineFailure::Access(
                Some(unavailable)
            ))))
        );
        assert_eq!(asks.recv().await, None);
    })
    .await
    .expect("the unserved engine refuses without asking to send");
}

fn key() -> Secret<AnthropicKey> {
    AnthropicKey::parse(Secret::new(Zeroizing::new(
        b"sk-ant-api-zqxsentinellabel".to_vec(),
    )))
    .expect("fake key")
}

fn with_transport(mut accounts: Accounts, peer: Scripted, cap: u32) -> Accounts {
    accounts
        .connections
        .use_test_transport(Transport::offline(peer, cap, 0));
    accounts
}

pub(in crate::runtime) fn with_empty_catalog(accounts: Accounts) -> Accounts {
    with_transport(accounts, peer(EMPTY_CATALOG), 8)
}

fn accounts(scratch: &Scratch, peer: Scripted) -> Accounts {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        peer,
        8,
    );
    accounts.connections.loading = true;
    accounts.connections.view.anthropic = KeyView::Checking;
    accounts
}

fn wire(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const EMPTY_CATALOG: &str = r#"{"data":[],"has_more":false}"#;
const KEPT_CATALOG: &str = r#"{"data":[{"type":"model","id":"claude-haiku-5-5","display_name":"Kept model"}],"has_more":false}"#;

fn peer(body: &str) -> Scripted {
    Scripted(Arc::new(std::sync::Mutex::new(Script {
        responses: std::iter::repeat_with(|| wire(200, body)).take(8).collect(),
        ..Script::default()
    })))
}

fn starts(peer: &Scripted) -> usize {
    peer.0.lock().expect("catalog peer").starts
}

fn retained_key(connections: &Connections) -> Option<&str> {
    connections
        .verified
        .as_ref()
        .map(|key| key.test_key())
        .or_else(|| connections.key.as_ref().map(|key| key.expose().as_str()))
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

#[derive(Default)]
struct NoNetwork {
    factories: std::sync::atomic::AtomicUsize,
    requests: std::sync::atomic::AtomicUsize,
}

impl Http for Arc<NoNetwork> {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Err(HttpError::Connect))
    }
}

#[tokio::test]
async fn saved_key_startup_and_check_never_send_without_a_catalog_allowance() {
    use std::sync::atomic::Ordering::SeqCst;

    for configured in [false, true] {
        let scratch = Scratch::new("key-dormant-startup");
        let store = FakeStore::default();
        let lock = scratch.db().with_file_name("key-test.lock");
        save_key(&store, &lock);
        let worker = Worker::new(
            Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
            Arc::new(FixedClock(0)),
        );
        let network = Arc::new(NoNetwork::default());
        let counted = Arc::clone(&network);
        let net = Net {
            http: Arc::new(move || {
                counted.factories.fetch_add(1, SeqCst);
                Ok(SharedHttp::new(Arc::clone(&counted)))
            }),
            ..test_support::net()
        };
        let mut accounts = Accounts::spawn_unconfigured(worker, net).expect("worker");
        assert_eq!(accounts.connections.view.anthropic, KeyView::Checking);
        let peer = peer(EMPTY_CATALOG);
        if configured {
            accounts = with_transport(accounts, peer.clone(), 0);
        }
        for check in [false, true] {
            if check {
                accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
            }
            settle(&mut accounts).await;
            assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
            assert_eq!(accounts.connections.anthropic_revision(), Some(1));
            assert_eq!(retained_key(&accounts.connections), None);
            assert_eq!(network.factories.load(SeqCst), 0);
            assert_eq!(network.requests.load(SeqCst), 0);
            let script = peer.0.lock().expect("offline peer");
            assert_eq!(script.starts, 0);
            assert_eq!(script.requests, Vec::<Vec<u8>>::new());
        }
        accounts
            .close(tokio::time::Instant::now(), tokio::time::Instant::now())
            .await;
    }
}

#[cfg(not(windows))]
#[tokio::test]
async fn a_saved_key_stays_unsupported_in_runtime_and_reconnect_snapshots() {
    let scratch = Scratch::new("key-no-native-transport");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let accounts = Accounts::spawn_unconfigured(worker, test_support::net()).unwrap();
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    settle(&mut brain.account).await;
    let expected = ControlToShell::Connections(ConnectionsView {
        anthropic: KeyView::UnsupportedPlatform,
        anthropic_revision: Some(1),
        claude_code: ClaudeCodeView::NotFound,
    });
    assert_eq!(brain.connections_frame(), expected);
    assert_eq!(retained_key(&brain.account.connections), None);
    assert_eq!(
        brain
            .account
            .connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey)),
        std::slice::from_ref(&expected)
    );
    assert!(!brain.account.connections.loading);
    assert_eq!(brain.account.connections.connected(), expected);
    assert_owned_key_refusal(&mut brain, ErrorKey::UnsupportedPlatform.into()).await;
    brain.close().await;
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
    let mut accounts = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        peer(EMPTY_CATALOG),
        8,
    );
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
        retained_key(&accounts.connections),
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
        retained_key(&accounts.connections),
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
    let requests = peer(EMPTY_CATALOG);
    let (owned, worker_lock) = (store.clone(), lock.clone());
    let worker = Worker::new(
        Box::new(move || Vault::with_service(owned.clone(), "key-test", &worker_lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        requests.clone(),
        8,
    );
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    let sent = || starts(&requests);
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
    let mut accounts = accounts(&scratch, Scripted::default());
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
    let (reader, _writer) = tokio::io::duplex(256);
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        reader: Some(Box::new(reader)),
        ..Script::default()
    })));
    let mut accounts = accounts(&scratch, peer.clone());
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
    assert_eq!(accounts.connections.anthropic_revision(), Some(1));
    assert_eq!(retained_key(&accounts.connections), None);
    assert_eq!(starts(&peer), 1);
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
    let entered = Arc::new(Notify::new());
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        entered: Some(Arc::clone(&entered)),
        connect_gate: Some(Arc::new(Notify::new())),
        ..Script::default()
    })));
    let mut accounts = accounts(&scratch, peer.clone());
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
    assert_eq!(starts(&peer), 1);
    assert!(peer.0.lock().expect("peer").requests.is_empty());
    let event = event.expect("the deadline reports while the connection is still held");
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
    let mut accounts = accounts(&scratch, Scripted::default());
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

/// A saved key checked Ready, then a Check whose credential load is on its way.
async fn ready_then_checking(scratch: &Scratch, peer: Scripted) -> (Accounts, ConnectionTag) {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        peer,
        8,
    );
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    let tag = accounts
        .connections
        .verified
        .as_ref()
        .expect("verified")
        .tag;
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    assert!(accounts.connections.loading);
    (accounts, tag)
}

async fn step(accounts: &mut Accounts) -> bool {
    match timeout(Duration::from_millis(500), accounts.next()).await {
        Ok(event) => {
            accounts.handle(event);
            true
        }
        Err(_) => false,
    }
}

const IMAGE_CATALOG: &str = r#"{"data":[{"type":"model","id":"claude-opus-5-5","display_name":"Opus","capabilities":{"image_input":{"supported":true}}}],"has_more":false}"#;

/// The models the key's route reports may take a picture now.
fn pictured(accounts: &Accounts) -> Vec<String> {
    match accounts.connections.route_input() {
        Some(Input::Engines(_, Availability::Ready(ready))) => ready
            .images
            .iter()
            .map(|slug| slug.as_str().to_owned())
            .collect(),
        _ => panic!("the key's route isn't ready"),
    }
}

#[tokio::test]
async fn only_a_verifying_check_of_the_current_key_makes_its_picture_facts_fresh() {
    let scratch = Scratch::new("key-picture-fresh");
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        responses: [
            wire(200, IMAGE_CATALOG),
            wire(500, "{}"),
            wire(200, IMAGE_CATALOG),
        ]
        .into(),
        ..Script::default()
    })));
    let (mut accounts, _) = ready_then_checking(&scratch, peer).await;
    assert_eq!(
        pictured(&accounts),
        ["claude-opus-5-5"],
        "the last conclusive fact"
    );
    let stale = accounts.connections.generation;
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(pictured(&accounts), Vec::<String>::new(), "text only");
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    assert_ne!(accounts.connections.generation, stale);
    let late = CatalogModel {
        info: kumochi_protocol::message::ModelInfo {
            slug: kumochi_protocol::message::ModelSlug::new("claude-opus-5-5").expect("slug"),
            display_name: kumochi_protocol::message::ModelName::new("Opus").expect("name"),
            supported_efforts: None,
        },
        capabilities: crate::connections::anthropic::models::Capabilities {
            images: true,
            pdfs: true,
            tool_argument_streaming: true,
            effort: true,
        },
    };
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Checked {
            generation: stale,
            result: Ok(vec![late]),
        }),
        vec![]
    );
    assert_eq!(
        pictured(&accounts),
        Vec::<String>::new(),
        "an older Check can't restore it"
    );
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    assert_eq!(pictured(&accounts), ["claude-opus-5-5"]);
    accounts.connection_event(ConnectionEvent::Refused {
        tag: accounts
            .connections
            .verified
            .as_ref()
            .expect("verified")
            .tag,
        attempt: None,
    });
    assert!(
        !accounts.connections.fresh,
        "a refused key's facts are never fresh"
    );
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_check_the_key_worker_wont_take_leaves_text_and_no_picture_fact() {
    let scratch = Scratch::new("key-picture-closed-worker");
    let (mut accounts, _) = ready_then_checking(&scratch, peer(IMAGE_CATALOG)).await;
    settle(&mut accounts).await;
    assert_eq!(pictured(&accounts), ["claude-opus-5-5"]);
    drop(accounts.jobs.take());
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(pictured(&accounts), Vec::<String>::new());
    assert!(
        accounts.connections.verified.is_some(),
        "the verified key keeps its text"
    );
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_check_waiting_on_its_load_cant_bring_a_refused_key_back() {
    let scratch = Scratch::new("key-refused-held-load");
    let peer = peer(EMPTY_CATALOG);
    let (mut accounts, tag) = ready_then_checking(&scratch, peer.clone()).await;
    accounts.connection_event(ConnectionEvent::Refused { tag, attempt: None });
    while step(&mut accounts).await {}
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert!(accounts.connections.verified.is_none());
    assert_eq!(starts(&peer), 1, "only the startup check read the catalog");
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut accounts).await;
    assert_eq!(accounts.connections.view.anthropic, KeyView::Ready);
    assert_ne!(
        accounts
            .connections
            .verified
            .as_ref()
            .expect("verified")
            .tag,
        tag
    );
    assert_eq!(starts(&peer), 2);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_refusal_during_a_checks_catalog_read_drops_it_and_its_key() {
    let scratch = Scratch::new("key-refused-catalog-read");
    let (mut accounts, tag) = ready_then_checking(&scratch, peer(EMPTY_CATALOG)).await;
    assert!(step(&mut accounts).await, "the load");
    assert!(
        accounts.connections.read.is_some(),
        "the catalog read is under way"
    );
    accounts.connection_event(ConnectionEvent::Refused { tag, attempt: None });
    assert_eq!(retained_key(&accounts.connections), None);
    while step(&mut accounts).await {}
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert!(accounts.connections.verified.is_none());
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_check_ending_after_the_fence_closed_cant_bring_a_refused_key_back() {
    let scratch = Scratch::new("key-refused-late-check");
    let (mut accounts, tag) = ready_then_checking(&scratch, peer(EMPTY_CATALOG)).await;
    assert!(step(&mut accounts).await, "the load");
    assert!(
        accounts.connections.read.is_some(),
        "the catalog read is under way"
    );
    accounts
        .connections
        .verified
        .as_ref()
        .expect("verified")
        .fence
        .revoke();
    assert!(step(&mut accounts).await, "the catalog read");
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    accounts.connection_event(ConnectionEvent::Refused { tag, attempt: None });
    assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
    assert!(accounts.connections.verified.is_none());
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_refusal_settles_a_waiting_reconcile_and_ignores_other_tags() {
    let scratch = Scratch::new("key-refused-reconcile");
    let (mut accounts, tag) = ready_then_checking(&scratch, peer(EMPTY_CATALOG)).await;
    accounts.connections.check_request = Some((accounts.connections.generation, 7));
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Refused {
            tag: ConnectionTag(tag.0 ^ 1),
            attempt: None,
        }),
        []
    );
    assert!(accounts.connections.loading);
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Refused { tag, attempt: None }),
        [
            ControlToShell::Connections(ConnectionsView {
                anthropic: KeyView::Unconfirmed,
                anthropic_revision: Some(1),
                claude_code: ClaudeCodeView::NotFound
            }),
            ControlToShell::AnthropicKeyReconciled {
                request: 7,
                read: None
            }
        ]
    );
    assert_eq!(accounts.connections.check_request, None);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

const TWO_MODELS: &str = r#"{"data":[{"type":"model","id":"claude-haiku-5-5","display_name":"Kept model"},{"type":"model","id":"claude-sonnet-5-5","display_name":"Second model"}],"has_more":false}"#;

/// A Check that ends inconclusive keeps the verified catalog, so a shell that connects afterwards
/// is offered the models a send is still granted on. A 401 withdraws them and replays nothing,
/// from the moment the post closes the key's fence, before the account has heard the tag.
#[tokio::test]
async fn a_reconnect_is_offered_the_catalog_a_send_is_still_granted_on() {
    for (case, withdrawn) in [("kept", false), ("refused", true), ("fence closed", true)] {
        let scratch = Scratch::new(&format!("key-reconnect-offer-{}", case.replace(' ', "-")));
        let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
            responses: [wire(200, TWO_MODELS), wire(500, "{}")].into(),
            ..Script::default()
        })));
        let (mut accounts, tag) = ready_then_checking(&scratch, peer.clone()).await;
        settle(&mut accounts).await;
        assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
        assert_eq!(
            starts(&peer),
            2,
            "the startup read and the inconclusive Check"
        );
        match case {
            "refused" => {
                accounts.connection_event(ConnectionEvent::Refused { tag, attempt: None });
            }
            "fence closed" => accounts
                .connections
                .verified
                .as_ref()
                .expect("verified")
                .fence
                .revoke(),
            _ => {}
        }
        assert!(
            accounts.connections.models_unsent().is_some(),
            "the connected shell takes its update"
        );
        accounts.connections.connected();
        let offered = accounts
            .connections
            .models_unsent()
            .map(|models| match models {
                DataToShell::Models { connection, models } => (
                    connection,
                    models
                        .iter()
                        .map(|model| {
                            (
                                model.slug.as_str().to_owned(),
                                model.display_name.as_str().to_owned(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => panic!("only the key's models"),
            });
        let kept = [
            ("claude-haiku-5-5".to_owned(), "Kept model".to_owned()),
            ("claude-sonnet-5-5".to_owned(), "Second model".to_owned()),
        ];
        assert_eq!(
            offered,
            (!withdrawn).then(|| (ConnectionKind::AnthropicKey, kept.to_vec())),
            "{case}"
        );
        accounts
            .close(tokio::time::Instant::now(), tokio::time::Instant::now())
            .await;
    }
}

/// A catalog update still pending when a 401 closes the key's fence carries no models, on the
/// current link and on a reconnect alike, before the account has heard the refused tag.
#[tokio::test]
async fn a_pending_catalog_update_offers_no_models_once_the_fence_is_closed() {
    for reconnect in [true, false] {
        let scratch = Scratch::new(&format!("key-pending-offer-closed-{reconnect}"));
        let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
            responses: [wire(200, TWO_MODELS), wire(500, "{}")].into(),
            ..Script::default()
        })));
        let (mut accounts, _) = ready_then_checking(&scratch, peer.clone()).await;
        settle(&mut accounts).await;
        assert_eq!(accounts.connections.view.anthropic, KeyView::Unconfirmed);
        accounts
            .connections
            .verified
            .as_ref()
            .expect("verified")
            .fence
            .revoke();
        if reconnect {
            accounts.connections.connected();
        }
        assert_eq!(
            accounts.connections.models_unsent(),
            Some(DataToShell::Models {
                connection: ConnectionKind::AnthropicKey,
                models: Vec::new(),
            }),
            "reconnect: {reconnect}"
        );
        assert_eq!(
            accounts.connections.models_unsent(),
            None,
            "reconnect: {reconnect}"
        );
        accounts
            .close(tokio::time::Instant::now(), tokio::time::Instant::now())
            .await;
    }
}
