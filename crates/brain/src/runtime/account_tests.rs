//! The account system over the pipes: its view's place in a connection, an op's answer, a
//! sign-in the real listener ends, and a Busy session lock retried when a shell connects.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, ControlToBrain, ControlToShell, DataToShell,
    Effort, ErrorKey, ModelChoice, ModelInfo, ModelSlug, Phase, PlanState, UserError,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use zeroize::Zeroizing;

use super::{Ended, PATIENCE, Shell, duplex};
use crate::connections::account::{Denied, Input, Listened, Worker};
use crate::connections::clock::{Clock, FixedClock};
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response, SharedHttp};
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::oauth::id_token::IdentityError;
use crate::connections::oauth::{Authorization, LaunchError, LaunchTarget, Launcher};
use crate::connections::records::{Account, Tokens};
use crate::connections::vault::fake::FakeStore;
use crate::connections::vault::{SaveResult, Vault};
use crate::context::ConnectionTag;
use crate::coordinator::{Availability, JobKind, ReadJob, Ready, StoreOp};
use crate::engines::responses::{AccessHandle, Bearer, Responses};
use crate::platform::{SessionLock, SessionLockError};
use crate::runtime::Seen;
use crate::runtime::account::{Access, Accounts, Event, HttpFactory, Net, exchange_task};
use crate::tap::Tap;
use crate::test_support::{self, Captured, Scratch};

const SIGNED_OUT: AccountView = AccountView::SignedOut {
    revocation_unconfirmed: false,
};

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    /// The next control frame, whatever it is.
    async fn frame(&mut self) -> ControlToShell {
        loop {
            let frame = timeout(PATIENCE, self.next_control())
                .await
                .expect("in time")
                .expect("readable")
                .expect("open");
            if !matches!(frame, ControlToShell::Connections(_)) {
                return frame;
            }
        }
    }

    /// The next control frame, which must be an account view.
    async fn account(&mut self) -> AccountView {
        match self.frame().await {
            ControlToShell::Account(view) => view,
            other => panic!("expected Account, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_view_follows_settings_and_a_sign_in_the_browser_refuses_ends_signed_out() {
    let scratch = Scratch::new("account-connection");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert!(matches!(
            shell.frame().await,
            ControlToShell::Settings { .. }
        ));
        assert_eq!(
            shell.account().await,
            SIGNED_OUT,
            "read before serving, sent once"
        );
        assert!(matches!(shell.frame().await, ControlToShell::Scenes { .. }));
        shell
            .send(&ControlToBrain::AccountOp(AccountOp::SignIn))
            .await;
        assert_eq!(shell.account().await, AccountView::SigningIn);
        match shell.frame().await {
            ControlToShell::Error(error) => assert_eq!(error.key(), ErrorKey::BrowserUnavailable),
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(shell.account().await, SIGNED_OUT);
        shell
            .send(&ControlToBrain::AccountOp(AccountOp::CancelSignIn))
            .await;
        assert_eq!(shell.account().await, SIGNED_OUT, "every op is answered");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_lock_busy_at_startup_is_retried_when_a_shell_connects() {
    let scratch = Scratch::new("account-busy-runtime");
    let held = SessionLock::try_acquire(&scratch.db().with_file_name(SESSION_LOCK)).expect("lock");
    let mut brain = test_support::brain(&scratch, "");
    drop(held);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert!(matches!(
            shell.frame().await,
            ControlToShell::Settings { .. }
        ));
        assert_eq!(
            shell.account().await,
            AccountView::Busy,
            "the startup read found it held"
        );
        assert!(matches!(shell.frame().await, ControlToShell::Scenes { .. }));
        assert_eq!(
            shell.account().await,
            SIGNED_OUT,
            "the connection's read took the lock"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

fn secret(text: &str) -> Secret<Zeroizing<String>> {
    Secret::new(Zeroizing::new(text.to_owned()))
}

/// Design §2, at the runtime: \`{:?}\` of the account system, while its exchange is outstanding
/// and after, never prints the client id, the code or the redirect.
#[tokio::test]
async fn the_account_system_never_prints_a_secret() {
    let scratch = Scratch::new("account-debug");
    let store = FakeStore::default();
    let registration = Secret::new(Account {
        client_id: Zeroizing::new("client-SENTINEL".to_owned()),
        subject: None,
        email: None,
        plan_enabled: false,
        tokens: None,
    });
    let seed = scratch.db().with_file_name("seed.lock");
    let saved = Vault::with_service(store.clone(), "test-accounts-debug", &seed)
        .expect("seed vault")
        .save_account(&registration);
    assert!(matches!(saved, SaveResult::Committed));
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "test-accounts-debug", &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("accounts");
    let mut printed = vec![format!(
        "{:?}",
        accounts.handle(Input::Op(AccountOp::SignIn))
    )];
    let frames = accounts.handle(Input::Listened {
        attempt: 1,
        result: Listened::Authorized(Authorization {
            code: secret("code-SENTINEL"),
            client_id: secret("client-SENTINEL"),
            nonce: secret("nonce-SENTINEL"),
            verifier: secret(&"v".repeat(43)),
            redirect: "http://127.0.0.1:1/auth/callback".to_owned(),
        }),
    });
    printed.push(format!("{accounts:?} {frames:?}"));
    // The listener's refused launch, the pending flag, the exchange and the flag's release.
    for _ in 0..4 {
        let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
        let frames = accounts.handle(input);
        printed.push(format!("{accounts:?} {frames:?}"));
    }
    assert!(
        printed.iter().any(|text| text.contains("Outstanding")),
        "an exchange was outstanding while printed: {printed:?}"
    );
    for text in &printed {
        assert!(!text.contains("SENTINEL"), "{text}");
    }
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Tells the test a job opened the vault, then holds the job until the test releases it.
type Gate = (std_mpsc::Sender<()>, std_mpsc::Receiver<()>);

/// L4: closing waits for the credential job already running, past its deadline, so the
/// instance capability's delete never overlaps it.
#[tokio::test]
async fn closing_waits_for_a_running_credential_job_past_its_deadline() {
    let scratch = Scratch::new("account-close");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let held = SessionLock::try_acquire(&lock).expect("lock");
    let gate: Arc<Mutex<Option<Gate>>> = Arc::default();
    let finished = Arc::new(AtomicBool::new(false));
    let (opening, opened) = (Arc::clone(&gate), Arc::clone(&finished));
    let worker = Worker::new(
        Box::new(move || {
            let waiting = opening.lock().expect("gate").take();
            if let Some((entered, release)) = waiting {
                entered.send(()).ok();
                release.recv().ok();
                opened.store(true, Ordering::SeqCst);
            }
            Vault::with_service(FakeStore::default(), "test-accounts-close", &lock)
        }),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("accounts");
    let (entered_tx, entered) = std_mpsc::channel();
    let (release, waiting) = std_mpsc::channel();
    *gate.lock().expect("gate") = Some((entered_tx, waiting));
    // Busy at startup, so a connection reads again, on the credentials thread.
    accounts.handle(Input::Connected);
    entered.recv().expect("the job is running");
    let captured = Captured::default();
    let _capturing = captured.install();
    let close = accounts.close(Instant::now(), Instant::now());
    tokio::pin!(close);
    let past_deadline = async {
        while !captured.text().contains("still running at the deadline") {
            tokio::select! {
                biased;
                () = &mut close => panic!("close returned while the job ran"),
                () = tokio::task::yield_now() => {}
            }
        }
    };
    timeout(PATIENCE, past_deadline)
        .await
        .expect("the deadline passes");
    assert!(!finished.load(Ordering::SeqCst), "the job is still held");
    release.send(()).expect("the job waits for its release");
    close.await;
    assert!(
        finished.load(Ordering::SeqCst),
        "the job ended before close returned"
    );
    drop(held);
}

/// CAPS-116: a browser launch that blocks runs off the core loop, which keeps running.
#[tokio::test]
async fn a_blocking_browser_launch_never_stalls_the_core_loop() {
    struct Stuck {
        entered: std_mpsc::Sender<()>,
        gate: Mutex<std_mpsc::Receiver<()>>,
    }
    impl Launcher for Stuck {
        fn launch(&self, _: &LaunchTarget) -> Result<(), LaunchError> {
            self.entered.send(()).ok();
            self.gate.lock().expect("gate").recv().ok();
            Err(LaunchError::Launch)
        }
    }
    let scratch = Scratch::new("account-launch");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(FakeStore::default(), "test-accounts-launch", &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let (entered, launching) = std_mpsc::channel();
    let (release, gate) = std_mpsc::channel();
    let net = Net {
        launcher: Arc::new(Stuck {
            entered,
            gate: Mutex::new(gate),
        }),
        ..test_support::net()
    };
    let mut accounts = Accounts::spawn_unconfigured(worker, net).expect("accounts");
    // Only a stalled loop lets this fire: it frees the launch so the test can end and fail.
    let (reached, watching) = std_mpsc::channel::<()>();
    let stalled = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let stalled = Arc::clone(&stalled);
        std::thread::spawn(move || {
            if watching.recv_timeout(PATIENCE).is_err() {
                stalled.store(true, Ordering::SeqCst);
            }
            release.send(()).ok();
        })
    };
    accounts.handle(Input::Op(AccountOp::SignIn));
    while launching.try_recv().is_err() {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    reached.send(()).ok();
    watchdog.join().expect("watchdog");
    assert!(
        !stalled.load(Ordering::SeqCst),
        "the loop stood still while the browser opened"
    );
    let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
    assert!(matches!(
        input,
        Event::Machine(Input::Listened {
            result: Listened::Launch(LaunchError::Launch),
            ..
        })
    ));
    accounts.close(Instant::now(), Instant::now()).await;
}

/// CAPS-116: the startup load runs on the credentials thread, and has ended once `spawn` returns.
#[tokio::test]
async fn the_startup_load_runs_on_the_credentials_thread() {
    let scratch = Scratch::new("account-startup-thread");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let threads = Arc::new(Mutex::new(Vec::new()));
    let opening = Arc::clone(&threads);
    let worker = Worker::new(
        Box::new(move || {
            let name = std::thread::current().name().map(str::to_owned);
            opening.lock().expect("threads").push(name);
            Vault::with_service(FakeStore::default(), "test-accounts-thread", &lock)
        }),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("accounts");
    assert_eq!(
        *threads.lock().expect("threads"),
        [Some("credentials".to_owned())]
    );
    assert_eq!(
        accounts.handle(Input::Connected),
        [ControlToShell::Account(SIGNED_OUT)],
        "loaded before spawn returned"
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Answers the token request, then moves the wall clock on while the identity check fetches
/// discovery, which it refuses.
struct SlowCheck(Arc<AtomicU64>);

impl Http for SlowCheck {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let (status, body) = match request.endpoint {
            Endpoint::Token => {
                let tokens = json!({"access_token":"fixture-access", "refresh_token":"fixture-refresh", "id_token":"fixture-id", "token_type":"Bearer", "scope":"openid chatgpt.tokens.use.direct", "expires_in":3600});
                (200, serde_json::to_vec(&tokens).expect("fixture"))
            }
            Endpoint::Discovery
            | Endpoint::Jwks(_)
            | Endpoint::Revocation(_)
            | Endpoint::Models(_)
            | Endpoint::AnthropicModels(_)
            | Endpoint::GitHub(_) => {
                self.0.fetch_add(20, Ordering::SeqCst);
                (503, Vec::new())
            }
        };
        std::future::ready(Ok(Response {
            status,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(body)),
        }))
    }
}

struct Ticking(Arc<AtomicU64>);

impl Clock for Ticking {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.0.load(Ordering::SeqCst))
    }
}

/// CAPS-117 R2: a token set is stamped when it arrives, so the identity check's time is never
/// added to its lifetime.
#[tokio::test]
async fn a_token_set_is_stamped_when_it_arrives_not_after_its_identity_check() {
    let wall = Arc::new(AtomicU64::new(1_000));
    let http = SharedHttp::new(SlowCheck(Arc::clone(&wall)));
    let net = Net {
        http: Arc::new(move || Ok(http.clone())),
        launcher: test_support::net().launcher,
        clock: Arc::new(Ticking(Arc::clone(&wall))),
    };
    let authorization = Authorization {
        code: secret("code"),
        client_id: secret("client"),
        nonce: secret("nonce"),
        verifier: secret(&"v".repeat(43)),
        redirect: "http://127.0.0.1:1/auth/callback".to_owned(),
    };
    let (events, mut results) = mpsc::unbounded_channel();
    exchange_task(1, 1, authorization, net, events).await;
    let Some(Input::Exchanged {
        attempt: 1,
        result: Ok(received),
    }) = results.recv().await
    else {
        panic!("one received set for attempt 1");
    };
    assert_eq!(
        wall.load(Ordering::SeqCst),
        1_020,
        "the check took its time"
    );
    assert_eq!(received.now, Some(1_000));
    assert!(matches!(
        received.identity,
        Err(IdentityError::Status { status: 503 })
    ));
}

/// Answers the refresh-token grant with a rotated set that carries no identity token.
struct Rotating;

impl Http for Rotating {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let (status, body) = match request.endpoint {
            Endpoint::Token => {
                let tokens = json!({"access_token":"access-new", "refresh_token":"refresh-new", "token_type":"Bearer", "scope":"openid offline_access chatgpt.tokens.use.direct", "expires_in":3600});
                (200, serde_json::to_vec(&tokens).expect("fixture"))
            }
            Endpoint::Discovery
            | Endpoint::Jwks(_)
            | Endpoint::Revocation(_)
            | Endpoint::Models(_)
            | Endpoint::AnthropicModels(_)
            | Endpoint::GitHub(_) => (503, Vec::new()),
        };
        std::future::ready(Ok(Response {
            status,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(body)),
        }))
    }
}

/// Design §3 rule 3: a caller that stops waiting cancels only its wait. The refresh it started
/// runs as its own task to the end, and the rotated set is saved.
#[tokio::test]
async fn a_dropped_wait_never_cancels_its_refresh() {
    let scratch = Scratch::new("account-refresh-dropped");
    let store = FakeStore::default();
    let service = "test-accounts-refresh";
    let reader = |name| {
        let lock = scratch.db().with_file_name(name);
        Vault::with_service(store.clone(), service, &lock).expect("vault")
    };
    let saved = reader("seed.lock").save_account(&Secret::new(Account {
        client_id: Zeroizing::new("client-saved".to_owned()),
        subject: Some(Zeroizing::new("subject-1".to_owned())),
        email: None,
        plan_enabled: true,
        tokens: Some(Tokens {
            access_token: Zeroizing::new("access-old".to_owned()),
            refresh_token: Zeroizing::new("refresh-old".to_owned()),
            id_token: None,
            scope: Zeroizing::new("openid offline_access chatgpt.tokens.use.direct".to_owned()),
            expires_at: 1_800_000_000,
            pending_identity_validation: false,
            refresh_sent: false,
            connection: 0,
        }),
    }));
    assert!(matches!(saved, SaveResult::Committed));
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let opening = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(opening.clone(), service, &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let http = SharedHttp::new(Rotating);
    let net = Net {
        http: Arc::new(move || Ok(http.clone())),
        ..test_support::net()
    };
    let mut accounts = Accounts::spawn_unconfigured(worker, net).expect("accounts");
    let (wait, _) = accounts.access();
    drop(wait);
    let refresh_token = || {
        let account = reader("reader.lock").load_account().expect("readable");
        account
            .and_then(|account| Some(account.expose().tokens.as_ref()?.refresh_token.to_string()))
    };
    // The pending flag, the sent mark, the refresh and the rotated set's save.
    for _ in 0..4 {
        let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
        accounts.handle(input);
    }
    assert_eq!(refresh_token(), Some("refresh-new".to_owned()));
    accounts.close(Instant::now(), Instant::now()).await;
}

const SHUTDOWN_SERVICE: &str = "test-accounts-shutdown";

/// Answers the refresh-token grant with a rotated set once released, and says when it's sent.
struct Gated {
    sent: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

/// A gated client, the signal that its request went out, and its release.
fn gated() -> (SharedHttp, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (sent_tx, sent) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let http = SharedHttp::new(Gated {
        sent: Mutex::new(Some(sent_tx)),
        release: Mutex::new(Some(released)),
    });
    (http, sent, release)
}

impl Http for Gated {
    fn execute(
        &self,
        _: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        if let Some(sent) = self.sent.lock().expect("gate").take() {
            sent.send(()).ok();
        }
        let release = self.release.lock().expect("gate").take();
        async move {
            if let Some(release) = release {
                release.await.ok();
            }
            let tokens = json!({"access_token":"access-new", "refresh_token":"refresh-new", "token_type":"Bearer", "scope":"openid offline_access chatgpt.tokens.use.direct", "expires_in":3600});
            Ok(Response {
                status: 200,
                request_id: None,
                meta: crate::connections::http_client::ResponseMeta::default(),
                body: Secret::new(Zeroizing::new(
                    serde_json::to_vec(&tokens).expect("fixture"),
                )),
            })
        }
    }
}

/// Saves an account whose bearer is due, then starts the account system on it with `http`.
fn due_account(scratch: &Scratch, store: &FakeStore, http: SharedHttp) -> Accounts {
    seeded(scratch, store, http, 1_800_000_000)
}

/// The connection [`seeded`] saves.
const SEEDED: u64 = 42;

/// Saves an account whose bearer expires at `expires_at`, then starts the account system on it
/// with `http` at 1,800,000,000.
fn seeded(scratch: &Scratch, store: &FakeStore, http: SharedHttp, expires_at: u64) -> Accounts {
    seeded_on(
        scratch,
        store,
        Arc::new(move || Ok(http.clone())),
        expires_at,
    )
}

/// [`seeded`], building each client with `http`.
fn seeded_on(scratch: &Scratch, store: &FakeStore, http: HttpFactory, expires_at: u64) -> Accounts {
    seeded_at(
        scratch,
        store,
        http,
        expires_at,
        Arc::new(FixedClock(1_800_000_000)),
    )
}

/// [`seeded_on`], with the account system's wall clock `clock`.
fn seeded_at(
    scratch: &Scratch,
    store: &FakeStore,
    http: HttpFactory,
    expires_at: u64,
    clock: Arc<dyn Clock>,
) -> Accounts {
    let seed = scratch.db().with_file_name("seed.lock");
    let saved = Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &seed)
        .expect("vault")
        .save_account(&Secret::new(Account {
            client_id: Zeroizing::new("client-saved".to_owned()),
            subject: Some(Zeroizing::new("subject-1".to_owned())),
            email: None,
            plan_enabled: true,
            tokens: Some(Tokens {
                access_token: Zeroizing::new("access-old".to_owned()),
                refresh_token: Zeroizing::new("refresh-old".to_owned()),
                id_token: None,
                scope: Zeroizing::new("openid offline_access chatgpt.tokens.use.direct".to_owned()),
                expires_at,
                pending_identity_validation: false,
                refresh_sent: false,
                connection: SEEDED,
            }),
        }));
    assert!(matches!(saved, SaveResult::Committed));
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let opening = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(opening.clone(), SHUTDOWN_SERVICE, &lock)),
        Arc::clone(&clock),
    );
    let net = Net {
        http,
        clock,
        ..test_support::net()
    };
    Accounts::spawn_unconfigured(worker, net).expect("accounts")
}

/// Asks for the bearer and runs the account system until its refresh is out.
async fn send_refresh(accounts: &mut Accounts, sent: oneshot::Receiver<()>) {
    let (wait, _) = accounts.access();
    drop(wait);
    // The pending flag, then the sent mark; then the refresh goes out.
    for _ in 0..2 {
        let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
        accounts.handle(input);
    }
    timeout(PATIENCE, sent)
        .await
        .expect("in time")
        .expect("the request went out");
}

/// The saved refresh token, and whether it's marked as sent.
fn saved_refresh(scratch: &Scratch, store: &FakeStore) -> (String, bool) {
    let lock = scratch.db().with_file_name("reader.lock");
    let account = Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &lock)
        .expect("vault")
        .load_account()
        .expect("readable")
        .expect("an account");
    let tokens = account.expose().tokens.as_ref().expect("tokens");
    (tokens.refresh_token.to_string(), tokens.refresh_sent)
}

/// The saved set's connection.
fn saved_connection(scratch: &Scratch, store: &FakeStore) -> u64 {
    let lock = scratch.db().with_file_name("reader.lock");
    let account = Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &lock)
        .expect("vault")
        .load_account()
        .expect("readable")
        .expect("an account");
    account.expose().tokens.as_ref().expect("tokens").connection
}

/// Panics on the refresh-token grant or, with `check`, answers it with a set to check and panics
/// on the check's discovery; either way after the token was marked as sent.
struct Panicking {
    check: bool,
}

impl Http for Panicking {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let body = (self.check && matches!(request.endpoint, Endpoint::Token)).then(|| {
            let tokens = json!({"access_token":"access-new", "refresh_token":"refresh-new", "id_token":"id.token.new", "token_type":"Bearer", "scope":"openid offline_access chatgpt.tokens.use.direct", "expires_in":3600});
            serde_json::to_vec(&tokens).expect("fixture")
        });
        async move {
            let Some(body) = body else {
                panic!("the transport panicked");
            };
            Ok(Response {
                status: 200,
                request_id: None,
                meta: crate::connections::http_client::ResponseMeta::default(),
                body: Secret::new(Zeroizing::new(body)),
            })
        }
    }
}

/// Runs the account system until `wait` is answered, and that answer's bearer text.
async fn answered(
    accounts: &mut Accounts,
    mut wait: oneshot::Receiver<Access>,
) -> Result<String, Denied> {
    let answer = timeout(PATIENCE, async {
        loop {
            if let Ok(answer) = wait.try_recv() {
                return answer;
            }
            let input = accounts.next().await;
            accounts.handle(input);
        }
    })
    .await
    .expect("answered in time");
    answer.map(|bearer| bearer.token.expose().to_string())
}

fn failed(key: ErrorKey, action: Action) -> Result<String, Denied> {
    let error = UserError::new(key, Vec::new(), vec![action]).expect("within limits");
    Err(Denied::Failed(error))
}

/// #98 M2, CAPS-151 M2: a refresh task that panics after its token was marked as sent still ends
/// its flight. The token may have gone out, so the session ends, saying the renewal wasn't
/// confirmed (OSC-223 gap 8), and it stays marked; a later caller is answered at once instead of
/// joining a dead flight.
#[tokio::test]
async fn a_refresh_task_that_panics_still_answers_its_callers() {
    let scratch = Scratch::new("account-refresh-panic");
    let store = FakeStore::default();
    let http = SharedHttp::new(Panicking { check: false });
    let mut accounts = due_account(&scratch, &store, http);
    let (wait, _) = accounts.access();
    assert_eq!(
        answered(&mut accounts, wait).await,
        failed(ErrorKey::RenewalUnconfirmed, Action::SignIn)
    );
    let (later, _) = accounts.access();
    assert_eq!(answered(&mut accounts, later).await, Err(Denied::Unusable));
    accounts.close(Instant::now(), Instant::now()).await;
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-old".to_owned(), true)
    );
}

/// #98 M2: a check task that panics leaves its set pending, and the next caller checks it again.
#[tokio::test]
async fn a_check_task_that_panics_leaves_the_set_pending() {
    let scratch = Scratch::new("account-check-panic");
    let store = FakeStore::default();
    let http = SharedHttp::new(Panicking { check: true });
    let mut accounts = due_account(&scratch, &store, http);
    let retry = failed(ErrorKey::SignInFailed, Action::Retry);
    let (wait, _) = accounts.access();
    assert_eq!(answered(&mut accounts, wait).await, retry);
    let (later, _) = accounts.access();
    assert_eq!(answered(&mut accounts, later).await, retry);
    accounts.close(Instant::now(), Instant::now()).await;
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-new".to_owned(), false)
    );
}

/// CAPS-151 H3: a refresh out at shutdown runs to its end, and its rotated set is saved before
/// the session lock is released, so no other instance can take the token meanwhile.
#[tokio::test]
async fn closing_saves_a_refresh_that_answers_in_time() {
    let scratch = Scratch::new("account-close-refresh");
    let store = FakeStore::default();
    let (http, sent, release) = gated();
    let mut accounts = due_account(&scratch, &store, http);
    send_refresh(&mut accounts, sent).await;
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let close = accounts.close(Instant::now(), Instant::now() + PATIENCE);
    tokio::pin!(close);
    for _ in 0..8 {
        tokio::select! {
            biased;
            () = &mut close => panic!("close returned while the refresh was out"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert!(
        matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
        "the lock is held while the refresh is out"
    );
    release.send(()).expect("the refresh waits for its release");
    close.await;
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-new".to_owned(), false)
    );
    assert_eq!(
        saved_connection(&scratch, &store),
        SEEDED,
        "a refreshed set keeps its sign-in's connection"
    );
    drop(SessionLock::try_acquire(&lock).expect("released"));
}

/// CAPS-151 H3: past its deadline, closing releases the lock with the refresh still out; its
/// token stays marked as sent, so the next start never sends it again.
#[tokio::test]
async fn closing_past_the_refresh_deadline_leaves_the_token_marked() {
    let scratch = Scratch::new("account-close-refresh-late");
    let store = FakeStore::default();
    let (http, sent, release) = gated();
    let mut accounts = due_account(&scratch, &store, http);
    send_refresh(&mut accounts, sent).await;
    accounts.close(Instant::now(), Instant::now()).await;
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-old".to_owned(), true)
    );
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    drop(SessionLock::try_acquire(&lock).expect("released"));
    drop(release);
}

/// A bearer that outlives every test.
const LATER: u64 = 1_900_000_000;

/// Answers each model-list request with the next reply, in order, and anything else with 503.
struct Catalog(Mutex<VecDeque<(u16, String)>>);

impl Catalog {
    fn answering(replies: impl IntoIterator<Item = (u16, String)>) -> SharedHttp {
        SharedHttp::new(Self(Mutex::new(replies.into_iter().collect())))
    }
}

impl Http for Catalog {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let (status, body) = match request.endpoint {
            Endpoint::Models(_) => self
                .0
                .lock()
                .expect("replies")
                .pop_front()
                .expect("a reply for each read"),
            _ => (503, String::new()),
        };
        std::future::ready(Ok(Response {
            status,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(body.into_bytes())),
        }))
    }
}

fn listing() -> (u16, String) {
    let models = json!({ "models": [
        { "slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list" },
        { "slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list" },
    ] });
    (200, models.to_string())
}

fn slugs(models: &[ModelInfo]) -> Vec<&str> {
    models.iter().map(|model| model.slug.as_str()).collect()
}

/// Runs the account system until a catalog read ends, and what the shell hears then.
async fn read_ended(accounts: &mut Accounts) -> Vec<ControlToShell> {
    timeout(PATIENCE, async {
        loop {
            let event = accounts.next().await;
            let read = matches!(event, Event::Read { .. });
            let frames = accounts.handle(event);
            if read {
                return frames;
            }
        }
    })
    .await
    .expect("in time")
}

/// Design §6: a usable account's catalog is read at startup; a failed read offers Retry, which
/// reads it again. Signing out clears the list, which the shell hears as empty, and a read the
/// sign-out outdated is dropped.
#[tokio::test]
async fn the_catalog_is_read_retried_and_cleared_when_the_account_stops_being_usable() {
    let scratch = Scratch::new("account-catalog");
    let store = FakeStore::default();
    let http = Catalog::answering([(503, String::new()), listing()]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let offline = UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]);
    assert_eq!(
        read_ended(&mut accounts).await,
        [ControlToShell::Error(offline.expect("valid"))]
    );
    assert_eq!(accounts.models(), None);
    accounts.handle(Input::Op(AccountOp::Retry));
    assert_eq!(read_ended(&mut accounts).await, []);
    let expected = Some(vec!["gpt-6-sol", "gpt-6-luna"]);
    assert_eq!(accounts.models().map(slugs), expected);
    assert_eq!(accounts.models_unsent().as_deref().map(slugs), expected);
    assert_eq!(accounts.models_unsent(), None, "sent once");
    let models = accounts.models().expect("read").to_vec();
    accounts.handle(Input::Op(AccountOp::SignOut));
    assert_eq!(accounts.models(), None);
    assert_eq!(
        accounts.models_unsent(),
        Some(Vec::new()),
        "the shell hears it's gone"
    );
    accounts.handle(Event::Read {
        epoch: 0,
        result: Ok(models),
    });
    assert_eq!(
        accounts.models(),
        None,
        "a read from before the sign-out is dropped"
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

/// D13-H1: a usable account is ready on its saved set's connection with its catalog, and an
/// accepted sign-out is unavailable at once, before the sign-out finishes.
#[tokio::test]
async fn availability_is_the_saved_connection_until_a_sign_out_is_accepted() {
    let scratch = Scratch::new("account-availability");
    let store = FakeStore::default();
    let mut accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    assert_eq!(read_ended(&mut accounts).await, []);
    assert_eq!(
        accounts.availability(),
        Availability::Ready(Ready {
            connection: ConnectionTag(SEEDED),
            models: accounts.models().expect("read").to_vec(),
            trusted_draw: false,
        })
    );
    let frames = accounts.handle(Input::Op(AccountOp::SignOut));
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            ControlToShell::Account(AccountView::SigningOut { .. })
        )),
        "the sign-out was accepted and hasn't finished"
    );
    let sign_in = UserError::new(ErrorKey::SignInNeeded, Vec::new(), vec![Action::SignIn]);
    assert_eq!(
        accounts.availability(),
        Availability::Unavailable(sign_in.expect("valid"))
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

/// A round's ask gets the bearer only for the ready connection it names, so a round never sends
/// on an account its run wasn't bound to.
#[tokio::test]
async fn a_rounds_ask_gets_the_bearer_only_of_the_connection_it_names() {
    let scratch = Scratch::new("account-access-for");
    let store = FakeStore::default();
    let mut accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    assert_eq!(read_ended(&mut accounts).await, []);
    for (wanted, bearer) in [
        (Some(ConnectionTag(SEEDED + 1)), Err(Denied::Unusable)),
        (None, Err(Denied::Unusable)),
        (Some(ConnectionTag(SEEDED)), Ok("access-old".to_owned())),
    ] {
        let (answer, wait) = oneshot::channel();
        accounts.access_for(wanted, answer);
        assert_eq!(answered(&mut accounts, wait).await, bearer, "{wanted:?}");
    }
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Answers the model list with a 401 and the refresh-token grant with a rotated set, counting
/// the grants.
struct RefusingModels(Arc<AtomicU64>);

impl Http for RefusingModels {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let refused = matches!(request.endpoint, Endpoint::Models(_));
        if matches!(request.endpoint, Endpoint::Token) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        let rotated = Rotating.execute(request);
        async move {
            if !refused {
                return rotated.await;
            }
            Ok(Response {
                status: 401,
                request_id: None,
                meta: crate::connections::http_client::ResponseMeta::default(),
                body: Secret::new(Zeroizing::new(Vec::new())),
            })
        }
    }
}

/// Runs the account system until `wait` is answered with a bearer.
async fn granted(accounts: &mut Accounts, mut wait: oneshot::Receiver<Access>) -> Bearer {
    timeout(PATIENCE, async {
        loop {
            if let Ok(answer) = wait.try_recv() {
                return answer.expect("a bearer");
            }
            let input = accounts.next().await;
            accounts.handle(input);
        }
    })
    .await
    .expect("answered in time")
}

/// OSC-223: a bearer the provider refused marks its set as due, so the next access refreshes it
/// once, though it isn't near its expiry. A late refusal of the set it replaced, or one naming
/// a generation that never was, marks nothing.
#[tokio::test]
async fn a_refused_bearer_is_refreshed_on_the_next_access() {
    let scratch = Scratch::new("account-refused");
    let store = FakeStore::default();
    let refreshes = Arc::new(AtomicU64::new(0));
    let http = SharedHttp::new(RefusingModels(Arc::clone(&refreshes)));
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let offline = UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]);
    assert_eq!(
        read_ended(&mut accounts).await,
        [ControlToShell::Error(offline.expect("valid"))],
        "the catalog's 401"
    );
    let (wait, _) = accounts.access();
    let rotated = granted(&mut accounts, wait).await;
    assert_eq!(rotated.token.expose().as_str(), "access-new");
    accounts.handle(Input::Refused {
        generation: rotated
            .generation
            .checked_sub(1)
            .expect("a rotation moved it"),
    });
    let (wait, _) = accounts.access();
    assert_eq!(
        answered(&mut accounts, wait).await,
        Ok("access-new".to_owned())
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1, "one refresh in all");
    accounts.close(Instant::now(), Instant::now()).await;

    let scratch = Scratch::new("account-refused-stale");
    let store = FakeStore::default();
    let mut accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    assert_eq!(read_ended(&mut accounts).await, []);
    let (wait, _) = accounts.access();
    let generation = granted(&mut accounts, wait).await.generation;
    accounts.handle(Input::Refused {
        generation: generation + 1,
    });
    let (wait, _) = accounts.access();
    assert_eq!(
        answered(&mut accounts, wait).await,
        Ok("access-old".to_owned())
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Signs out and runs the account system until the view says so.
async fn sign_out(accounts: &mut Accounts) {
    let mut frames = accounts.handle(Input::Op(AccountOp::SignOut));
    let signed_out = |frames: &[ControlToShell]| {
        frames.iter().any(|frame| {
            matches!(
                frame,
                ControlToShell::Account(AccountView::SignedOut { .. })
            )
        })
    };
    timeout(PATIENCE, async {
        while !signed_out(&frames) {
            let event = accounts.next().await;
            frames = accounts.handle(event);
        }
    })
    .await
    .expect("in time");
}

/// Counts model-list requests, answers everything 503 and says when it's dropped.
struct Probe {
    models: Arc<AtomicU64>,
    dropped: Option<oneshot::Sender<()>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            dropped.send(()).ok();
        }
    }
}

impl Http for Probe {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        if matches!(request.endpoint, Endpoint::Models(_)) {
            self.models.fetch_add(1, Ordering::SeqCst);
        }
        std::future::ready(Ok(Response {
            status: 503,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(Vec::new())),
        }))
    }
}

/// P106-M2: a sign-out cancels the catalog read it outdates, here once the read holds the bearer
/// and is building its client, so the old account's bearer never goes out after it.
#[tokio::test]
async fn a_sign_out_cancels_a_catalog_read_before_its_request_goes_out() {
    let scratch = Scratch::new("account-catalog-cancelled");
    let store = FakeStore::default();
    let (entered_tx, entered) = oneshot::channel();
    let (release, held) = std_mpsc::channel::<()>();
    let (dropped_tx, dropped) = oneshot::channel();
    let models = Arc::new(AtomicU64::new(0));
    let probe = SharedHttp::new(Probe {
        models: Arc::clone(&models),
        dropped: Some(dropped_tx),
    });
    // Holds only the first client built, the catalog's; the sign-out's revocation gets its own.
    let gate = Mutex::new(Some((entered_tx, held, probe)));
    let factory: HttpFactory = Arc::new(move || {
        let Some((entered, held, probe)) = gate.lock().expect("gate").take() else {
            return Ok(Catalog::answering([]));
        };
        entered.send(()).ok();
        held.recv().ok();
        Ok(probe)
    });
    let mut accounts = seeded_on(&scratch, &store, factory, LATER);
    timeout(PATIENCE, entered)
        .await
        .expect("in time")
        .expect("the read builds its client");
    sign_out(&mut accounts).await;
    release.send(()).expect("the catalog's client is held");
    timeout(PATIENCE, dropped)
        .await
        .expect("in time")
        .expect("the catalog's client is dropped");
    assert_eq!(models.load(Ordering::SeqCst), 0, "no model list request");
    assert_eq!(accounts.models(), None);
    accounts.close(Instant::now(), Instant::now()).await;
}

/// The saved registration's client id, if there's one.
fn registration(scratch: &Scratch, store: &FakeStore) -> Option<String> {
    let lock = scratch.db().with_file_name("reader.lock");
    Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &lock)
        .expect("vault")
        .load_account()
        .expect("readable")
        .map(|account| account.expose().client_id.to_string())
}

/// P106-M1: a server error offers Retry whatever code it carries, so the plan stays Enabled and
/// an ordinary sign-out keeps the registration.
#[tokio::test]
async fn a_server_error_naming_the_plan_offers_retry_and_its_sign_out_keeps_the_registration() {
    let scratch = Scratch::new("account-catalog-server-error");
    let store = FakeStore::default();
    let refused = json!({ "error": { "code": "subscription_sharing_user_not_eligible" } });
    let http = Catalog::answering([(503, refused.to_string())]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let offline = UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]);
    assert_eq!(
        read_ended(&mut accounts).await,
        [ControlToShell::Error(offline.expect("valid"))],
        "no Unavailable view: the plan stays Enabled"
    );
    sign_out(&mut accounts).await;
    accounts.close(Instant::now(), Instant::now()).await;
    assert_eq!(
        registration(&scratch, &store),
        Some("client-saved".to_owned())
    );
}

/// Pick 2: a catalog that refuses the plan shows the account Unavailable, with no action.
#[tokio::test]
async fn a_plan_the_catalog_refuses_shows_the_account_unavailable() {
    let scratch = Scratch::new("account-catalog-refused");
    let store = FakeStore::default();
    let refused = json!({ "error": { "code": "subscription_sharing_user_not_eligible" } });
    let http = Catalog::answering([(403, refused.to_string())]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let unavailable = UserError::new(ErrorKey::PlanUnavailable, Vec::new(), Vec::new());
    assert_eq!(
        read_ended(&mut accounts).await,
        [
            ControlToShell::Error(unavailable.expect("valid")),
            ControlToShell::Account(AccountView::SignedIn {
                email: None,
                plan: PlanState::Unavailable,
                recovery: None,
            }),
        ]
    );
    assert_eq!(accounts.models(), None);
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Design §6: a connection hears the catalog on the data pipe, then the settled settings with the
/// default model the read set, which is saved.
#[tokio::test]
async fn a_connection_hears_the_catalog_and_the_default_model_it_set() {
    let scratch = Scratch::new("account-catalog-brain");
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    let luna = ModelChoice {
        model: ModelSlug::new("gpt-6-luna").expect("valid"),
        effort: Effort::Low,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    };
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        match timeout(PATIENCE, shell.data()).await.expect("in time") {
            DataToShell::Models { connection, models } => {
                assert_eq!(
                    connection,
                    kumochi_protocol::message::ConnectionKind::ChatGpt
                );
                assert_eq!(slugs(&models), ["gpt-6-sol", "gpt-6-luna"]);
            }
            other => panic!("expected Models, got {other:?}"),
        }
        loop {
            if let ControlToShell::Settings {
                view,
                answer: false,
            } = shell.frame().await
                && view.default_model.is_some()
            {
                assert_eq!(view.default_model, Some(luna.clone()));
                break;
            }
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(scratch.config().view().default_model, Some(luna));
}

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    /// Submits `text` as a new task and reads its run to `Done` and its data's end.
    async fn finish_task(&mut self, text: &str) {
        self.submit(text).await;
        let (run, _) = self.state().await;
        assert_eq!(self.state().await, (run, Phase::Done));
        loop {
            match self.data().await {
                DataToShell::RunEnd { run: ended } if ended == run => return,
                DataToShell::Models { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

/// A usable account at startup lets a task's `Done` start its title job, and a sign-out fences
/// jobs before anything else that sign-out does: the next task's `Done` starts none.
#[tokio::test]
async fn a_title_job_starts_only_while_the_account_can_send() {
    let scratch = Scratch::new("account-jobs");
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    let mut seen = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.finish_task("first").await;
        timeout(PATIENCE, async {
            while !matches!(
                seen.recv().await.expect("the brain reports"),
                Seen::StoreOpQueued(StoreOp::ReadJob(ReadJob {
                    kind: JobKind::Title,
                    ..
                }))
            ) {}
        })
        .await
        .expect("the account could send at startup, so the title job starts");
        shell
            .send(&ControlToBrain::AccountOp(AccountOp::SignOut))
            .await;
        while !matches!(
            shell.frame().await,
            ControlToShell::Account(AccountView::SignedOut { .. })
        ) {}
        // The first task's jobs, compaction after its title included, end with the sign-out.
        while seen.try_recv().is_ok() {}
        shell.finish_task("second").await;
        // Done is told in the same step that would start the run's job, after it, so a job
        // queued for the second task would already be here.
        let jobs = std::iter::from_fn(|| seen.try_recv().ok())
            .filter(|seen| matches!(seen, Seen::StoreOpQueued(StoreOp::ReadJob(_))))
            .count();
        assert_eq!(jobs, 0, "signed out, so no job starts");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

/// Lists the catalog and answers the refresh-token grant with what `token` gives, inside the
/// refresh's own task, so a panic there is that task's.
struct Renewing(TokenAnswer);

/// How the token endpoint answers a refresh.
type TokenAnswer = fn() -> Result<Response, HttpError>;

impl Http for Renewing {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let token = self.0;
        let models = matches!(request.endpoint, Endpoint::Models(_));
        async move {
            if !models {
                return token();
            }
            let (status, body) = listing();
            Ok(Response {
                status,
                request_id: None,
                meta: crate::connections::http_client::ResponseMeta::default(),
                body: Secret::new(Zeroizing::new(body.into_bytes())),
            })
        }
    }
}

/// No model server: a round that gets its bearer never gets further.
struct Unreachable;

impl Connect for Unreachable {
    type Io = tokio::io::DuplexStream;

    fn connect(&self) -> impl std::future::Future<Output = std::io::Result<Self::Io>> + Send {
        std::future::ready(Err(std::io::Error::other("no model server in tests")))
    }
}

/// The error of the next `Failed` run state the shell hears, answering pings on the way.
async fn failed_with<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>) -> Option<UserError> {
    loop {
        match shell.frame().await {
            ControlToShell::RunState {
                phase: Phase::Failed,
                error,
                ..
            } => return error,
            ControlToShell::Ping => shell.send(&ControlToBrain::Pong).await,
            _ => {}
        }
    }
}

/// P169-M1 (OSC-223 gap 8): a named run waiting on a renewal whose outcome is unknown fails
/// saying so, on the frames the shell reads, and so does the next task's admission. The account's
/// end reaches the run before its engine's own answer, so the reason has to travel with the
/// account. An explicit `invalid_grant` still says the session ended.
#[tokio::test]
async fn a_run_waiting_on_an_unconfirmed_renewal_fails_saying_so() {
    let refusal = || {
        Ok(Response {
            status: 400,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"error":"invalid_grant"}"#.to_vec())),
        })
    };
    let cases: [(&str, TokenAnswer, ErrorKey); 3] = [
        (
            "timeout",
            || Err(HttpError::Timeout),
            ErrorKey::RenewalUnconfirmed,
        ),
        (
            "panic",
            || panic!("the transport panicked"),
            ErrorKey::RenewalUnconfirmed,
        ),
        ("invalid-grant", refusal, ErrorKey::SessionEnded),
    ];
    for (name, token, key) in cases {
        let scratch = Scratch::new(&format!("account-run-renewal-{name}"));
        let store = FakeStore::default();
        let now = Arc::new(AtomicU64::new(1_800_000_000));
        let http = SharedHttp::new(Renewing(token));
        let accounts = seeded_at(
            &scratch,
            &store,
            Arc::new(move || Ok(http.clone())),
            1_800_003_600,
            Arc::new(Ticking(Arc::clone(&now))),
        );
        let (access, asks) = AccessHandle::new();
        let engine = Responses::new(ModelClient::new(Unreachable), Budget::limited(None), access);
        let selected = crate::runtime::Selected::Account {
            engine: Box::new(engine),
            asks,
        };
        let mut brain = test_support::brain_on(&scratch, selected, accounts);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let expected =
            UserError::new(key, Vec::new(), vec![Action::SignIn]).expect("within limits");
        let client = async {
            assert!(matches!(
                timeout(PATIENCE, shell.data()).await.expect("in time"),
                DataToShell::Models { .. }
            ));
            loop {
                if let ControlToShell::Settings { view, .. } = shell.frame().await
                    && view.default_model.is_some()
                {
                    break;
                }
            }
            // The bearer is now due, so the run's round renews it, and waits.
            now.store(1_800_003_600, Ordering::SeqCst);
            shell.submit("first").await;
            assert_eq!(
                failed_with(&mut shell).await,
                Some(expected.clone()),
                "{name}: the run"
            );
            shell.submit("second").await;
            assert_eq!(
                failed_with(&mut shell).await,
                Some(expected.clone()),
                "{name}: the next task"
            );
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost, "{name}");
        brain.close().await;
    }
}

/// P169-M1 re-review: a renewal a catalog read started, with no run to fail, still tells the shell
/// why the session ended, though the read it was for is cancelled.
#[tokio::test]
async fn a_catalog_renewal_that_goes_unconfirmed_tells_the_shell_why() {
    let unlisted: TokenAnswer = || {
        Ok(Response {
            status: 408,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(
                br#"{"error":"temporarily_unavailable"}"#.to_vec(),
            )),
        })
    };
    let timed_out: TokenAnswer = || Err(HttpError::Timeout);
    let expected = UserError::new(
        ErrorKey::RenewalUnconfirmed,
        Vec::new(),
        vec![Action::SignIn],
    )
    .expect("within limits");
    for (name, token) in [("timeout", timed_out), ("unlisted", unlisted)] {
        let scratch = Scratch::new(&format!("account-catalog-renewal-{name}"));
        let store = FakeStore::default();
        let mut accounts = seeded(
            &scratch,
            &store,
            SharedHttp::new(Renewing(token)),
            1_800_000_000,
        );
        let frames = timeout(PATIENCE, async {
            let mut frames = Vec::new();
            loop {
                let event = accounts.next().await;
                frames.extend(accounts.handle(event));
                if frames.iter().any(|frame| {
                    matches!(
                        frame,
                        ControlToShell::Account(AccountView::SignedIn {
                            recovery: Some(AccountRecovery::SignInAgain),
                            ..
                        })
                    )
                }) {
                    return frames;
                }
            }
        })
        .await
        .expect("the session ends in time");
        let errors: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                ControlToShell::Error(error) => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, std::slice::from_ref(&expected), "{name}");
        accounts.close(Instant::now(), Instant::now()).await;
    }
}
