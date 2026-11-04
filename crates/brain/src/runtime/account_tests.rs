//! The account system over the pipes: its view's place in a connection, an op's answer, a
//! sign-in the real listener ends, and a Busy session lock retried when a shell connects.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::{AccountOp, AccountView, ControlToBrain, ControlToShell, ErrorKey};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use zeroize::Zeroizing;

use super::{Ended, PATIENCE, Shell, duplex};
use crate::connections::account::{Input, Listened, Worker};
use crate::connections::clock::{Clock, FixedClock};
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response, SharedHttp};
use crate::connections::oauth::id_token::IdentityError;
use crate::connections::oauth::{Authorization, LaunchError, LaunchTarget, Launcher};
use crate::connections::records::{Account, Tokens};
use crate::connections::vault::fake::FakeStore;
use crate::connections::vault::{SaveResult, Vault};
use crate::platform::{SessionLock, SessionLockError};
use crate::runtime::account::{Accounts, Net, exchange_task};
use crate::tap::Tap;
use crate::test_support::{self, Captured, Scratch};

const SIGNED_OUT: AccountView = AccountView::SignedOut {
    revocation_unconfirmed: false,
};

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    /// The next control frame, whatever it is.
    async fn frame(&mut self) -> ControlToShell {
        timeout(PATIENCE, self.next_control())
            .await
            .expect("in time")
            .expect("readable")
            .expect("open")
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
    assert!(matches!(saved, SaveResult::Committed { .. }));
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "test-accounts-debug", &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn(worker, test_support::net()).expect("accounts");
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
    let mut accounts = Accounts::spawn(worker, test_support::net()).expect("accounts");
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
    let mut accounts = Accounts::spawn(worker, net).expect("accounts");
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
        Input::Listened {
            result: Listened::Launch(LaunchError::Launch),
            ..
        }
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
    let mut accounts = Accounts::spawn(worker, test_support::net()).expect("accounts");
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
            Endpoint::Discovery | Endpoint::Jwks(_) | Endpoint::Revocation(_) => {
                self.0.fetch_add(20, Ordering::SeqCst);
                (503, Vec::new())
            }
        };
        std::future::ready(Ok(Response {
            status,
            request_id: None,
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
    exchange_task(1, authorization, net, events).await;
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
            Endpoint::Discovery | Endpoint::Jwks(_) | Endpoint::Revocation(_) => (503, Vec::new()),
        };
        std::future::ready(Ok(Response {
            status,
            request_id: None,
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
        }),
    }));
    assert!(matches!(saved, SaveResult::Committed { .. }));
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
    let mut accounts = Accounts::spawn(worker, net).expect("accounts");
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
                body: Secret::new(Zeroizing::new(
                    serde_json::to_vec(&tokens).expect("fixture"),
                )),
            })
        }
    }
}

/// Saves an account whose bearer is due, then starts the account system on it with `http`.
fn due_account(scratch: &Scratch, store: &FakeStore, http: SharedHttp) -> Accounts {
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
                expires_at: 1_800_000_000,
                pending_identity_validation: false,
                refresh_sent: false,
            }),
        }));
    assert!(matches!(saved, SaveResult::Committed { .. }));
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let opening = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(opening.clone(), SHUTDOWN_SERVICE, &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let net = Net {
        http: Arc::new(move || Ok(http.clone())),
        ..test_support::net()
    };
    Accounts::spawn(worker, net).expect("accounts")
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
