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
    let mut accounts = Accounts::spawn(
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

struct HeldStore {
    armed: Arc<std::sync::atomic::AtomicBool>,
    store: FakeStore,
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl CredentialStore for HeldStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        if target == "anthropic-key-staged.key-test"
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
impl Http for Held {
    async fn execute(&self, _: Request) -> Result<Response, HttpError> {
        self.0.notified().await;
        Ok(Response {
            status: 200,
            request_id: None,
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        })
    }
}

#[tokio::test]
async fn remove_behind_a_held_adoption_runs_on_the_same_worker() {
    let scratch = Scratch::new("key-worker-order");
    let store = FakeStore::default();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut held = Some(HeldStore {
        armed: Arc::clone(&armed),
        store: store.clone(),
        entered: entered_tx,
        resume: std::sync::Mutex::new(Some(resume_rx)),
    });
    let lock = scratch.db().with_file_name("key-test.lock");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(held.take().expect("one open"), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let mut accounts = Accounts::spawn(worker, test_support::net()).expect("worker");
    store
        .set(
            "anthropic-key-staged.key-test",
            &Secret::new(Zeroizing::new(b"sk-ant-api-fake-staged-value".to_vec())),
        )
        .expect("fake paste");
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::AnthropicKeyStaged));
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("adoption is held");
    })
    .await
    .expect("gate joined");
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey));
    resume.send(()).expect("release adoption");
    timeout(Duration::from_secs(5), async {
        while accounts.connections.view.anthropic != KeyView::Missing {
            let event = accounts.next().await;
            accounts.handle(event);
        }
    })
    .await
    .expect("remove completes after adoption");
    assert_eq!(
        store.get("anthropic-key.key-test").expect("fake read"),
        None
    );
    assert_eq!(
        store
            .get("anthropic-key-staged.key-test")
            .expect("fake read"),
        None
    );
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    accounts
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn changed_or_removed_keys_ignore_late_worker_and_check_results() {
    let scratch = Scratch::new("key-generation");
    let mut accounts = accounts(&scratch, SharedHttp::new(Held(Arc::new(Notify::new()))));
    let first = accounts.connections.generation;
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey));
    let current = accounts.connections.generation;
    assert_eq!(current, first + 1);
    assert_eq!(
        accounts.connection_event(ConnectionEvent::Loaded {
            generation: first,
            result: Ok(Some(key()))
        }),
        vec![]
    );
    assert_eq!(accounts.connections.key.as_ref().map(|_| ()), None);
    accounts.connection_event(ConnectionEvent::Loaded {
        generation: current,
        result: Ok(None),
    });
    accounts.connection_event(ConnectionEvent::Checked {
        generation: first,
        result: Ok(vec![]),
    });
    assert_eq!(accounts.connections.view.anthropic, KeyView::Missing);
    accounts.connection_event(ConnectionEvent::Op(ConnectionOp::AnthropicKeyStaged));
    accounts.connection_event(ConnectionEvent::Checked {
        generation: current,
        result: Ok(vec![]),
    });
    assert_eq!(accounts.connections.view.anthropic, KeyView::Checking);
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
        result: Ok(Some(key())),
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
        result: Ok(Some(key())),
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
        result: Ok(Some(key())),
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
