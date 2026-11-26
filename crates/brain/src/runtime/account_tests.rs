//! The account system over the pipes: its view's place in a connection, an op's answer, a
//! sign-in the real listener ends, and a Busy session lock retried when a shell connects.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use kumochi_protocol::Secret;
use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Action, AudioSession, ConnectionKind, ControlToBrain,
    ControlToShell, DataToShell, Effort, ErrorKey, ModelChoice, ModelInfo, ModelSlug, Narration,
    OutputGeneration, PartState, Phase, PlanState, SettingsOp, UserError,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use zeroize::Zeroizing;

use super::{Ended, PATIENCE, Shell, duplex};
#[cfg(not(windows))]
use crate::config::tuning::{OAuthTuning, TokenTuning};
use crate::connections::account::{Denied, Done, Input, Listened, Worker};
use crate::connections::catalog::Listing;
use crate::connections::clock::{Clock, FixedClock};
use crate::connections::error::VaultError;
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
use crate::runtime::account::{Access, Accounts, Event, HttpFactory, Net, exchange_task};
use crate::runtime::{PairStart, Seen, Stage};
use crate::tap::Tap;
use crate::test_support::{self, Captured, Scratch};

#[path = "account_tests/typed_replay.rs"]
mod typed_replay;

/// A profile lock the brain would hold, in a scratch folder.
#[cfg(not(windows))]
fn profile() -> (Scratch, Arc<crate::platform::SessionLock>) {
    let scratch = Scratch::new("account-profile");
    let lock = crate::platform::SessionLock::try_acquire(&scratch.dir().join(SESSION_LOCK))
        .expect("profile lock");
    (scratch, Arc::new(lock))
}

const SIGNED_OUT: AccountView = AccountView::SignedOut {
    revocation_unconfirmed: false,
};

#[cfg(not(windows))]
#[test]
fn native_account_subsystem_refuses_before_starting_without_a_trust_adapter() {
    let (_scratch, profile) = profile();
    assert!(matches!(
        crate::accounts(OAuthTuning::default(), TokenTuning::default(), profile),
        Err(crate::BrainError::NativeTrust(
            crate::platform::TrustRootsError::Unsupported
        ))
    ));
}

#[cfg(not(windows))]
#[tokio::test]
async fn native_account_startup_refuses_unsupported_without_publishing_retry() {
    use crate::platform::credential::CredentialStoreError;

    let captured = Captured::default();
    let _logs = captured.install();
    let net = test_support::net();
    let (scratch, profile) = profile();
    let worker = Worker::system(
        Arc::clone(&net.clock),
        profile,
        crate::connections::vault::HistoryLink::unbound(),
    );
    let held = worker.held();

    let refusal = Accounts::spawn(worker, net, OAuthTuning::default(), TokenTuning::default())
        .expect_err("no account to offer or retry");
    assert_eq!(refusal.kind(), std::io::ErrorKind::Unsupported);
    assert!(matches!(
        refusal
            .get_ref()
            .and_then(|error| error.downcast_ref::<CredentialStoreError>()),
        Some(CredentialStoreError::Unsupported)
    ));
    assert!(
        !held.load(Ordering::Acquire),
        "no session lock was acquired"
    );
    assert!(
        !captured.text().contains("session lock failed"),
        "an unsupported adapter is not a lock failure"
    );
    assert!(
        !scratch
            .dir()
            .join(kumochi_protocol::constants::AUTHORITY_FILE)
            .exists(),
        "refused before the authority opened"
    );
}

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    async fn submit_on(&mut self, thread: kumochi_protocol::message::ThreadId) {
        use kumochi_protocol::frame::{self, Pipe};
        use kumochi_protocol::message::{DataToBrain, ScreenChoice};
        use tokio::io::AsyncWriteExt;

        let submit = frame::encode(
            &DataToBrain::TextSubmit {
                turn: 1,
                screen: ScreenChoice::None,
                thread: Some(thread),
                text: "words".into(),
            },
            Pipe::Data,
        )
        .expect("typed submit");
        self.data_out.write_all(&submit).await.expect("sent");
    }

    /// The next control frame, whatever it is.
    async fn frame(&mut self) -> ControlToShell {
        loop {
            let frame = timeout(PATIENCE, self.next_control())
                .await
                .expect("in time")
                .expect("readable")
                .expect("open");
            if !matches!(
                frame,
                ControlToShell::Connections(_) | ControlToShell::ScreenHolds(_)
            ) {
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

#[tokio::test]
async fn audio_repair_replays_busy_without_reloading_the_saved_account() {
    let scratch = Scratch::new("audio-repair-busy-vault");
    let store = FakeStore::default();
    save_seeded_account(&scratch, &store, LATER);
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let held = SessionLock::try_acquire(&lock).expect("scratch lock");
    let opens = Arc::new(AtomicU64::new(0));
    let opening = Arc::clone(&opens);
    let models = Arc::new(AtomicU64::new(0));
    let http = SharedHttp::new(Probe {
        models: Arc::clone(&models),
        dropped: None,
    });
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(1_800_000_000));
    let worker = Worker::new(
        Box::new(move || {
            opening.fetch_add(1, Ordering::SeqCst);
            Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &lock)
        }),
        Arc::clone(&clock),
    );
    let net = Net {
        http: Arc::new(move || Ok(http.clone())),
        clock,
        ..test_support::net()
    };
    let mut accounts = Accounts::spawn_unconfigured(worker, net).expect("accounts");
    let busy = ControlToShell::Account(AccountView::Busy);
    assert_eq!(
        accounts.handle(Input::Connected),
        std::slice::from_ref(&busy)
    );
    let still_busy = timeout(PATIENCE, async {
        loop {
            let event = accounts.next().await;
            if matches!(event, Event::Machine(Input::Vault(Done::Loaded(_)))) {
                break event;
            }
            accounts.handle(event);
        }
    })
    .await
    .expect("ordinary load ends");
    assert!(matches!(
        still_busy,
        Event::Machine(Input::Vault(Done::Loaded(Err(VaultError::Busy))))
    ));
    assert!(accounts.handle(still_busy).is_empty());
    let before = opens.load(Ordering::SeqCst);
    assert_eq!(
        models.load(Ordering::SeqCst),
        0,
        "the held vault has no catalog"
    );
    drop(held);

    assert_eq!(
        accounts.handle(Event::AudioRepaired),
        std::slice::from_ref(&busy)
    );
    let pending = accounts.vault_load_pending();
    if pending {
        read_ended(&mut accounts).await;
    }
    assert_eq!(
        (
            opens.load(Ordering::SeqCst) - before,
            models.load(Ordering::SeqCst),
            pending
        ),
        (0, 0, false),
        "audio re-pair must only replay Busy, not load credentials or discover models"
    );
    assert_eq!(accounts.view(), AccountView::Busy);

    assert_eq!(accounts.handle(Input::Connected), [busy]);
    let offline =
        UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).unwrap();
    assert_eq!(
        read_ended(&mut accounts).await,
        [ControlToShell::Error(offline)]
    );
    assert_eq!(
        (
            opens.load(Ordering::SeqCst) - before,
            models.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    assert_eq!(
        accounts.view(),
        AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None
        }
    );
    accounts.close(Instant::now(), Instant::now()).await;
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
                _ = &mut close => panic!("close returned while the job ran"),
                () = tokio::task::yield_now() => {}
            }
        }
    };
    timeout(PATIENCE, past_deadline)
        .await
        .expect("the deadline passes");
    assert!(!finished.load(Ordering::SeqCst), "the job is still held");
    release.send(()).expect("the job waits for its release");
    assert_eq!(
        close.await,
        Stage::Late,
        "a release seen after the deadline is late"
    );
    assert!(
        finished.load(Ordering::SeqCst),
        "the job ended before close returned"
    );
    drop(held);
}

/// A credential thread that panics never sends its release, so closing reports it failed.
#[tokio::test]
async fn closing_after_the_credential_thread_panicked_fails_its_release() {
    let scratch = Scratch::new("account-close-panicked");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let held = SessionLock::try_acquire(&lock).expect("lock");
    let armed = Arc::new(AtomicBool::new(false));
    let opening = Arc::clone(&armed);
    let worker = Worker::new(
        Box::new(move || {
            assert!(
                !opening.load(Ordering::SeqCst),
                "{}",
                test_support::SENTINEL
            );
            Vault::with_service(FakeStore::default(), "test-accounts-panicked", &lock)
        }),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("accounts");
    armed.store(true, Ordering::SeqCst);
    // Busy at startup, so a connection opens the vault again, on the credentials thread.
    accounts.handle(Input::Connected);
    assert_eq!(
        accounts
            .close(Instant::now() + PATIENCE, Instant::now() + PATIENCE)
            .await,
        Stage::Failed
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
    // The check, the pending flag, the sent mark's save, the refresh and the rotated set's save.
    // The store is read only once the worker reports the save after the refresh.
    let (mut refreshed, mut saved) = (false, false);
    for _ in 0..8 {
        let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
        let rotated = matches!(input, Event::Machine(Input::Vault(Done::Rotated(..))));
        refreshed |= matches!(input, Event::Machine(Input::Refreshed { .. }));
        accounts.handle(input);
        saved = refreshed && rotated;
        if saved {
            break;
        }
    }
    assert!(saved, "the rotated set's save was reported");
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
    save_seeded_account(scratch, store, expires_at);
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

fn save_seeded_account(scratch: &Scratch, store: &FakeStore, expires_at: u64) {
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
}

/// Runs the account system through its sign-in's check, which comes before anything is ready.
pub(crate) async fn prove(accounts: &mut Accounts) {
    let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
    assert!(
        matches!(input, Event::Proof { .. }),
        "the check answers first"
    );
    accounts.handle(input);
}

/// Asks for the bearer and runs the account system until its refresh is out.
async fn send_refresh(accounts: &mut Accounts, sent: oneshot::Receiver<()>) {
    prove(accounts).await;
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
            _ = &mut close => panic!("close returned while the refresh was out"),
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

/// A rotated set whose save is refused is held, which ends the refresh wait; closing reports it
/// failed, and the old token stays marked as sent.
#[tokio::test]
async fn closing_with_a_refused_rotated_save_fails() {
    let scratch = Scratch::new("account-close-refresh-held");
    let store = FakeStore::default();
    let (http, sent, release) = gated();
    let mut accounts = due_account(&scratch, &store, http);
    send_refresh(&mut accounts, sent).await;
    store.0.lock().expect("fake lock").refusing = true;
    release.send(()).expect("the refresh waits for its release");
    assert_eq!(
        accounts
            .close(Instant::now() + PATIENCE, Instant::now() + PATIENCE)
            .await,
        Stage::Failed
    );
    store.0.lock().expect("fake lock").refusing = false;
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-old".to_owned(), true)
    );
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
    assert_eq!(
        accounts.close(Instant::now(), Instant::now()).await,
        Stage::Outstanding,
        "a refresh still out at its deadline is outstanding"
    );
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
        { "slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}] },
        { "slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}] },
    ] });
    (200, models.to_string())
}

#[tokio::test]
async fn route_default_a_queued_repair_cannot_overwrite_an_acknowledged_user_choice() {
    use crate::runtime::settings::{self, Change, Gate, Queued};
    use kumochi_protocol::message::{ConnectionKind, SettingsOp};

    for connection in [ConnectionKind::ClaudeCode, ConnectionKind::AnthropicKey] {
        let scratch = Scratch::new(&format!("queued-repair-{connection:?}"));
        let store = FakeStore::default();
        let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
        let mut brain = test_support::brain_with(&scratch, "", accounts);
        let picked = ModelChoice {
            connection,
            model: ModelSlug::new("same-slug").expect("slug"),
            effort: Effort::High,
        };
        let release = brain.hold_config_write();
        brain.settings_ops.push_back(Queued {
            change: Change::User(SettingsOp::SetDefaultModel(picked.clone())),
            epoch: brain.epoch + 1,
        });
        brain.pump_settings(None).expect("start held user write");
        timeout(PATIENCE, async {
            loop {
                let event = brain.account.next().await;
                let read = matches!(event, Event::Read { .. });
                brain.account_input(event, None).expect("catalog event");
                if read {
                    break;
                }
            }
        })
        .await
        .expect("catalog queued repair");
        assert!(
            brain
                .settings_ops
                .iter()
                .any(|op| op.epoch == settings::BRAIN),
            "repair queued behind user write"
        );
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            loop {
                if matches!(shell.frame().await, ControlToShell::Settings { .. }) {
                    break;
                }
            }
            release.send(Gate::Write).expect("release user write");
            loop {
                if let ControlToShell::Settings { view, answer: true } = shell.frame().await {
                    assert_eq!(
                        view.default_model,
                        Some(picked.clone()),
                        "user acknowledgement"
                    );
                    break;
                }
            }
            shell
                .send(&ControlToBrain::Settings(SettingsOp::FinishOnboarding))
                .await;
            loop {
                if let ControlToShell::Settings { view, answer: true } = shell.frame().await {
                    assert!(view.onboarded);
                    assert_eq!(
                        view.default_model,
                        Some(picked.clone()),
                        "queue drained past repair"
                    );
                    break;
                }
            }
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
        assert_eq!(
            scratch.config().view().default_model,
            Some(picked),
            "reload"
        );
    }
}

#[tokio::test]
async fn route_default_restart_and_catalog_preserve_explicit_intent() {
    use AfterRestart::{Kept, Refused, Repaired};
    use kumochi_protocol::message::ConnectionKind;
    let cases = [
        (
            "{ connection = 'ClaudeCode', model = 'same-slug', effort = 'High' }",
            Kept(ConnectionKind::ClaudeCode),
        ),
        (
            "{ connection = 'AnthropicKey', model = 'same-slug', effort = 'High' }",
            Kept(ConnectionKind::AnthropicKey),
        ),
        (
            "{ connection = 'ClaudeCode', model = 'same-slug', effort = 'Ultra' }",
            Kept(ConnectionKind::ClaudeCode),
        ),
        (
            "{ connection = 'AnthropicKey', model = 'same-slug', effort = 'Ultra' }",
            Kept(ConnectionKind::AnthropicKey),
        ),
        (
            "{ connection = 'Unknown', model = 'same-slug', effort = 'High' }",
            Refused,
        ),
        (
            "{ connection = 'ClaudeCode', model = 'same-slug' }",
            Refused,
        ),
        ("{ connection = 'AnthropicKey', effort = 'High' }", Refused),
        (
            "{ connection = 42, model = 'same-slug', effort = 'High' }",
            Refused,
        ),
        (
            "{ connection = 'ClaudeCode', model = 42, effort = 'High' }",
            Refused,
        ),
        (
            "{ connection = 'AnthropicKey', model = 'same-slug', effort = 42 }",
            Refused,
        ),
        ("'not a selection table'", Repaired),
        ("{}", Repaired),
        ("{ model = 'same-slug' }", Repaired),
        (
            "{ connection = 'ChatGpt', model = 42, effort = 'High' }",
            Repaired,
        ),
    ];
    for (n, (value, expected)) in cases.into_iter().enumerate() {
        Box::pin(preserved_default_after_restart(n, value, expected, None)).await;
    }
}

/// Claude Code ready and listing the saved default never starts the `ChatGPT` engine: the run asks
/// Claude Code, whose launch is refused here, so it fails with Claude Code's own reason.
#[tokio::test]
async fn a_ready_connection_without_an_engine_never_starts_chatgpts() {
    use kumochi_protocol::message::ConnectionKind;
    Box::pin(preserved_default_after_restart(
        99,
        "{ connection = 'ClaudeCode', model = 'same-slug', effort = 'High' }",
        AfterRestart::Kept(ConnectionKind::ClaudeCode),
        Some(ConnectionKind::ClaudeCode),
    ))
    .await;
}

/// What a hand-written `default_model` becomes once the brain restarts and reads the catalog.
#[derive(Clone, Copy, Debug)]
enum AfterRestart {
    /// Another connection's intent: kept on disk, shown, and refused at the run's first build.
    Kept(kumochi_protocol::message::ConnectionKind),
    /// Another connection's malformed intent: kept on disk, and a run has no model to use.
    Refused,
    /// Malformed intent on `ChatGpt` or no connection: unset, so the catalog writes its default.
    Repaired,
}

fn model_unavailable() -> UserError {
    UserError::new(
        ErrorKey::ModelUnavailable,
        Vec::new(),
        vec![Action::OpenSettings],
    )
    .expect("one action")
}

#[tokio::test]
async fn a_listed_default_with_unknown_efforts_is_kept_but_never_sent() {
    use kumochi_protocol::constants::CONFIG_FILE;

    let scratch = Scratch::new("mixed-effort-catalog");
    std::fs::create_dir_all(scratch.dir()).unwrap();
    let path = scratch.dir().join(CONFIG_FILE);
    let original = "default_model = { model = 'unsupported', effort = 'High' }\n";
    std::fs::write(&path, original).unwrap();
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([(
        200,
        json!({"models": [
            {"slug": "unsupported", "display_name": "Unsupported", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"future"}], "input_modalities": ["image"]},
            {"slug": "sendable", "display_name": "Sendable", "visibility": "list",
             "supported_reasoning_levels": [{"effort":"medium"}]},
        ]}).to_string(),
    )]), LATER);
    let starts = Arc::new(AtomicU64::new(0));
    let selected = crate::runtime::Selected::Account {
        engine: Box::new(CountStarts(Arc::clone(&starts))),
        asks: mpsc::unbounded_channel().1,
    };
    let mut brain = test_support::brain_on(&scratch, selected, accounts);
    finish_catalog(&mut brain).await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let DataToShell::Models { connection, models } =
            timeout(PATIENCE, shell.data()).await.unwrap()
        else {
            panic!("the catalog is published");
        };
        assert_eq!(connection, ConnectionKind::ChatGpt);
        assert_eq!(slugs(&models), ["sendable"]);
        shell.submit("words").await;
        assert_eq!(failed_with(&mut shell).await, Some(model_unavailable()));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert_eq!(
        starts.load(Ordering::Relaxed),
        0,
        "no request entered send admission"
    );
    brain.close().await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

async fn finish_catalog(brain: &mut crate::runtime::Brain) {
    timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            let read = matches!(event, Event::Read { .. });
            brain.account_input(event, None).unwrap();
            if read {
                break;
            }
        }
    })
    .await
    .unwrap();
    Box::pin(brain.drain_settings()).await;
}

fn crowded_catalog(earlier_effort: &str) -> (u16, String) {
    let mut rows: Vec<_> = (0..199)
        .map(|n| {
            json!({
                "slug": format!("m-{n}"), "display_name": "Model", "visibility": "list",
                "supported_reasoning_levels": [{"effort": "low"}],
            })
        })
        .collect();
    rows.push(json!({
        "slug": "earlier", "display_name": "Earlier", "visibility": "list",
        "supported_reasoning_levels": [{"effort": earlier_effort}],
    }));
    rows.push(json!({
        "slug": "saved", "display_name": "Saved", "visibility": "list",
        "supported_reasoning_levels": [{"effort": "low"}], "input_modalities": ["image"],
    }));
    (200, json!({"models": rows}).to_string())
}

#[tokio::test]
async fn a_catalog_row_gaining_support_does_not_replace_the_saved_overflow_model() {
    use kumochi_protocol::constants::CONFIG_FILE;
    use kumochi_protocol::message::{RunId, ThreadId};

    let scratch = Scratch::new("overflow-default-restart");
    let thread = ThreadId(2);
    let mut db = crate::store::Store::open(&scratch.db()).unwrap();
    db.recover().unwrap();
    db.begin_turn(thread, Some("Task"), RunId(1), "previous")
        .unwrap();
    db.end_run(thread, RunId(1), &[], crate::store::RunEnd::Done)
        .unwrap();
    drop(db);
    let path = scratch.dir().join(CONFIG_FILE);
    let original = "default_model = { model = 'saved', effort = 'High' }\n";
    std::fs::write(&path, original).unwrap();
    let saved = ModelChoice {
        model: ModelSlug::new("saved").unwrap(),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    for (earlier_effort, shifted, last) in [("future", false, "saved"), ("low", true, "earlier")] {
        let store = FakeStore::default();
        let accounts = seeded(
            &scratch,
            &store,
            Catalog::answering([crowded_catalog(earlier_effort)]),
            LATER,
        );
        let (sent, mut bodies) = mpsc::channel(1);
        let starts = Arc::new(AtomicU64::new(0));
        let engine: Box<dyn crate::engines::Engine> = if shifted {
            Box::new(CountStarts(Arc::clone(&starts)))
        } else {
            Box::new(CaptureModelBody(sent))
        };
        let selected = crate::runtime::Selected::Account {
            engine,
            asks: mpsc::unbounded_channel().1,
        };
        let mut brain = test_support::brain_on(&scratch, selected, accounts);
        finish_catalog(&mut brain).await;
        assert_eq!(brain.account.takes_images(&saved), !shifted);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            let DataToShell::Models { connection, models } =
                timeout(PATIENCE, shell.data()).await.unwrap()
            else {
                panic!("the catalog is published")
            };
            assert_eq!(connection, ConnectionKind::ChatGpt);
            assert_eq!(models.len(), 200);
            assert_eq!(models.last().map(|model| model.slug.as_str()), Some(last));
            shell.submit_on(thread).await;
            if shifted {
                assert_eq!(failed_with(&mut shell).await, Some(model_unavailable()));
                assert_eq!(starts.load(Ordering::Relaxed), 0);
            } else {
                assert_eq!(
                    timeout(PATIENCE, bodies.recv()).await.unwrap(),
                    Some(("saved".to_owned(), "low".to_owned()))
                );
                assert_eq!(failed_with(&mut shell).await, None);
            }
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

/// An uninstalled ready Claude Code returns its own reason. An Anthropic default without a
/// native transport is unsupported; other unavailable or invalid defaults lack a model.
fn refused_with(
    choice: Option<kumochi_protocol::message::ConnectionKind>,
    ready: Option<kumochi_protocol::message::ConnectionKind>,
) -> UserError {
    use kumochi_protocol::message::ConnectionKind;
    match (choice, ready) {
        (_, Some(ConnectionKind::ClaudeCode)) => UserError::new(
            ErrorKey::ClaudeCodeNotFound,
            Vec::new(),
            vec![Action::OpenSettings],
        )
        .expect("one action"),
        #[cfg(not(windows))]
        (Some(ConnectionKind::AnthropicKey), None) => ErrorKey::UnsupportedPlatform.into(),
        _ => model_unavailable(),
    }
}

/// An unreadable config.toml refuses a run as it did before saved connections: no default, so
/// the model check fails it with Open Settings, and the file is never written.
#[tokio::test]
async fn route_default_an_unreadable_config_still_refuses_with_open_settings() {
    use kumochi_protocol::constants::CONFIG_FILE;
    let scratch = Scratch::new("unreadable-default");
    std::fs::create_dir_all(scratch.dir()).expect("folder");
    let path = scratch.dir().join(CONFIG_FILE);
    std::fs::write(&path, "default_model = {").expect("config");
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let starts = Arc::new(AtomicU64::new(0));
    let selected = crate::runtime::Selected::Account {
        engine: Box::new(CountStarts(Arc::clone(&starts))),
        asks: mpsc::unbounded_channel().1,
    };
    let mut brain = test_support::brain_on(&scratch, selected, accounts);
    timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            let read = matches!(event, Event::Read { .. });
            brain.account_input(event, None).expect("catalog event");
            if read {
                break;
            }
        }
    })
    .await
    .expect("catalog read");
    Box::pin(brain.drain_settings()).await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("hello").await;
        assert_eq!(failed_with(&mut shell).await, Some(model_unavailable()));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert_eq!(starts.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read_to_string(&path).expect("file"),
        "default_model = {"
    );
    brain.close().await;
}

#[derive(Debug)]
struct CountStarts(Arc<AtomicU64>);
impl crate::engines::Engine for CountStarts {
    fn start(
        &self,
        _: crate::engines::RunRequest,
        events: mpsc::Sender<crate::engines::TurnEvent>,
        _: crate::engines::SendGate,
    ) -> crate::engines::Round {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            events
                .send(crate::engines::TurnEvent::Finished(
                    crate::engines::Finish::Failed(crate::engines::EngineFailure::Provider),
                ))
                .await
                .expect("events");
        })
    }
}

#[tokio::test]
async fn a_legacy_effort_sends_low_without_rewriting_the_task_choice() {
    Box::pin(stored_effort(
        "legacy-effort-execution",
        "ultra",
        Effort::Medium,
        "low",
    ))
    .await;
}

#[tokio::test]
async fn a_known_unsupported_effort_changes_the_body_not_the_task_choice() {
    Box::pin(stored_effort(
        "known-effort-execution",
        "max",
        Effort::Max,
        "medium",
    ))
    .await;
}

#[derive(Debug)]
struct CaptureModelBody(mpsc::Sender<(String, String)>);

impl crate::engines::Engine for CaptureModelBody {
    fn start(
        &self,
        request: crate::engines::RunRequest,
        events: mpsc::Sender<crate::engines::TurnEvent>,
        gate: crate::engines::SendGate,
    ) -> crate::engines::Round {
        let captured = self.0.clone();
        Box::pin(async move {
            let crate::engines::Answer::Grant(lease) = gate.ask().await else {
                return;
            };
            let body = crate::engines::responses::body(&request.request).unwrap();
            captured
                .send((
                    body["model"].as_str().unwrap().to_owned(),
                    body["reasoning"]["effort"].as_str().unwrap().to_owned(),
                ))
                .await
                .unwrap();
            drop(lease);
            events
                .send(crate::engines::TurnEvent::Finished(
                    crate::engines::Finish::Failed(crate::engines::EngineFailure::Provider),
                ))
                .await
                .unwrap();
        })
    }
}

async fn stored_effort(name: &str, stored: &str, displayed: Effort, sent_effort: &str) {
    use kumochi_protocol::message::{RunId, ThreadId};

    let scratch = Scratch::new(name);
    let thread = ThreadId(2);
    let mut database = crate::store::Store::open(&scratch.db()).unwrap();
    database.recover().unwrap();
    database
        .begin_turn(thread, Some("Task"), RunId(1), "previous")
        .unwrap();
    database
        .end_run(thread, RunId(1), &[], crate::store::RunEnd::Done)
        .unwrap();
    drop(database);
    {
        let raw = rusqlite::Connection::open(scratch.db()).unwrap();
        raw.execute(
            "UPDATE threads SET model = 'saved', effort = ?1, connection = 'chatgpt' WHERE id = 2",
            [stored],
        )
        .unwrap();
        let read = crate::store::context::Snapshot::begin(&raw).unwrap();
        assert_eq!(
            read.choice(thread).unwrap(),
            Some(ModelChoice {
                model: ModelSlug::new("saved").unwrap(),
                effort: displayed,
                connection: ConnectionKind::ChatGpt,
            })
        );
        read.end().unwrap();
    }
    let store = FakeStore::default();
    let accounts = seeded(
        &scratch,
        &store,
        Catalog::answering([(
            200,
            json!({"models": [
                {"slug": "saved", "display_name": "Saved", "visibility": "list",
                 "supported_reasoning_levels": [{"effort":"low"}, {"effort":"medium"}]},
            ]})
            .to_string(),
        )]),
        LATER,
    );
    let (sent, mut bodies) = mpsc::channel(1);
    let selected = crate::runtime::Selected::Account {
        engine: Box::new(CaptureModelBody(sent)),
        asks: mpsc::unbounded_channel().1,
    };
    let mut brain = test_support::brain_on(&scratch, selected, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert!(matches!(
            timeout(PATIENCE, shell.data()).await.unwrap(),
            DataToShell::Models { .. }
        ));
        shell.submit_on(thread).await;
        assert_eq!(
            timeout(PATIENCE, bodies.recv()).await.unwrap(),
            Some(("saved".to_owned(), sent_effort.to_owned())),
        );
        assert_eq!(failed_with(&mut shell).await, None);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let reopened = rusqlite::Connection::open(scratch.db()).unwrap();
    let raw: (String, String, String) = reopened
        .query_row(
            "SELECT model, effort, connection FROM threads WHERE id = 2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        raw,
        ("saved".to_owned(), stored.to_owned(), "chatgpt".to_owned())
    );
}

/// `kind` ready, listing `same-slug`.
fn lists_same_slug(kind: kumochi_protocol::message::ConnectionKind) -> crate::coordinator::Input {
    crate::coordinator::Input::Engines(
        kind,
        crate::coordinator::Availability::Ready(crate::coordinator::Ready {
            connection: crate::context::ConnectionTag(1),
            models: vec![ModelInfo {
                slug: kumochi_protocol::message::ModelSlug::new("same-slug").expect("slug"),
                display_name: kumochi_protocol::message::ModelName::new("Same").expect("name"),
                supported_efforts: kumochi_protocol::message::EffortSet::new([
                    kumochi_protocol::message::Effort::Low,
                ]),
            }],
            trusted_draw: false,
            images: Vec::new(),
        }),
    )
}

/// `ready`: a connection made ready, listing `same-slug`, before the turn.
async fn preserved_default_after_restart(
    n: usize,
    value: &str,
    expected: AfterRestart,
    ready: Option<kumochi_protocol::message::ConnectionKind>,
) {
    use kumochi_protocol::constants::CONFIG_FILE;
    use kumochi_protocol::message::{ConnectionKind, Effort, SettingsOp};
    let scratch = Scratch::new(&format!("restart-default-{n}"));
    std::fs::create_dir_all(scratch.dir()).expect("folder");
    let path = scratch.dir().join(CONFIG_FILE);
    std::fs::write(&path, format!("default_model = {value}")).expect("config");
    let before: toml::Table = std::fs::read_to_string(&path)
        .expect("file")
        .parse()
        .expect("TOML");
    let mut config = scratch.config();
    if let AfterRestart::Kept(kind) = expected {
        let shown = config
            .view()
            .default_model
            .as_ref()
            .expect("retained identity");
        assert_eq!(shown.connection, kind);
        assert_eq!(shown.model.as_str(), "same-slug");
    }
    let pending = config
        .stage(SettingsOp::FinishOnboarding)
        .expect("unrelated write");
    pending.write().expect("written");
    config.commit(pending);
    drop(config);
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let starts = Arc::new(AtomicU64::new(0));
    let selected = crate::runtime::Selected::Account {
        engine: Box::new(CountStarts(Arc::clone(&starts))),
        asks: mpsc::unbounded_channel().1,
    };
    let mut brain = test_support::brain_on(&scratch, selected, accounts);
    timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            let read = matches!(event, Event::Read { .. });
            brain.account_input(event, None).expect("catalog event");
            if read {
                break;
            }
        }
    })
    .await
    .expect("catalog read");
    brain.drain_settings().await;
    if let Some(kind) = ready {
        brain.offline(lists_same_slug(kind));
    }
    let snapshot = brain.snapshot(crate::context::Origin::Task(
        kumochi_protocol::message::ThreadId(2),
    ));
    let after: toml::Table = std::fs::read_to_string(&path)
        .expect("file")
        .parse()
        .expect("TOML");
    assert_eq!(after["onboarded"].as_bool(), Some(true));
    let kind = match expected {
        AfterRestart::Kept(kind) => Some(kind),
        AfterRestart::Refused => None,
        AfterRestart::Repaired => {
            let choice = snapshot.choice.expect("repaired default");
            assert_eq!(
                (choice.connection, choice.model.as_str(), choice.effort),
                (ConnectionKind::ChatGpt, "gpt-6-luna", Effort::Low)
            );
            assert_eq!(after["default_model"]["model"].as_str(), Some("gpt-6-luna"));
            brain.close().await;
            return;
        }
    };
    assert_eq!(
        snapshot.choice.as_ref().map(|choice| choice.connection),
        kind
    );
    assert_eq!(
        after["default_model"], before["default_model"],
        "retained across write/restart/catalog: {n}"
    );
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("never send this through another route").await;
        assert_eq!(
            failed_with(&mut shell).await,
            Some(refused_with(kind, ready))
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert_eq!(
        starts.load(Ordering::Relaxed),
        0,
        "no engine, bearer or Grant path entered"
    );
    brain.close().await;
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

#[tokio::test]
async fn a_catalog_refused_offline_is_still_unsupported_for_every_new_shell() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let scratch = Scratch::new("account-catalog-unsupported");
    let factories = Arc::new(AtomicUsize::new(0));
    let found = Arc::clone(&factories);
    let accounts = seeded_on(
        &scratch,
        &FakeStore::default(),
        Arc::new(move || {
            found.fetch_add(1, Ordering::Relaxed);
            Err(HttpError::UnsupportedPlatform)
        }),
        LATER,
    );
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    finish_catalog(&mut brain).await;
    assert_eq!(brain.account.sendable(), None);
    assert!(matches!(
        brain.account.availability(),
        Availability::Unavailable(error)
            if error.key() == ErrorKey::UnsupportedPlatform && error.actions().is_empty()
    ));
    assert_unsupported_reconnects(&mut brain).await;
    assert_eq!(factories.load(Ordering::Relaxed), 1);
    brain.close().await;
}

async fn assert_unsupported_reconnects(brain: &mut crate::runtime::Brain) {
    let expected = ControlToShell::AccountUnsupported(brain.account.view());
    for _ in 0..2 {
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            loop {
                let frame = shell.frame().await;
                if matches!(
                    frame,
                    ControlToShell::Account(_) | ControlToShell::AccountUnsupported(_)
                ) {
                    assert_eq!(frame, expected);
                    break;
                }
            }
            drop(shell);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .expect("the late shell receives a snapshot");
        assert_eq!(ended, Ended::Lost);
    }
    assert_eq!(
        brain.account.handle(Input::Op(AccountOp::Retry)),
        [expected]
    );
}

#[tokio::test]
async fn revocation_only_startup_keeps_the_debt_and_replays_unsupported() {
    use crate::connections::records::{Origin, Revocable, RevocationList};

    let scratch = Scratch::new("account-revoke-unsupported");
    let store = FakeStore::default();
    let seed = || {
        Vault::with_service(
            store.clone(),
            SHUTDOWN_SERVICE,
            &scratch.dir().join("seed.lock"),
        )
        .expect("fixture vault")
    };
    let debt = RevocationList {
        entries: vec![Revocable {
            client_id: Zeroizing::new("fixture-client".to_owned()),
            refresh_token: Zeroizing::new("fixture-revocation".to_owned()),
            added_at: 1_800_000_000,
            origin: Origin::Exchange,
        }],
    };
    assert!(matches!(
        seed().save_revocations(&Secret::new(debt.clone())),
        SaveResult::Committed
    ));
    let opening = store.clone();
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(opening.clone(), SHUTDOWN_SERVICE, &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(|| Err(HttpError::UnsupportedPlatform)),
            ..test_support::net()
        },
    )
    .expect("fixture account service");
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            let revoked = matches!(
                event,
                Event::Machine(Input::RevocationUnsupported | Input::Revoked(_))
            );
            brain
                .account_input(event, None)
                .expect("revocation completion");
            if revoked {
                break;
            }
        }
    })
    .await
    .expect("revocation pass ends");
    assert!(matches!(
        brain.account.view(),
        AccountView::SignedOut { .. }
    ));
    assert_unsupported_reconnects(&mut brain).await;
    brain.close().await;
    assert_eq!(seed().load_revocations().unwrap(), Some(Secret::new(debt)));
}

#[tokio::test]
async fn an_ordinary_bearer_refresh_refusal_reaches_all_waiters_and_later_shells() {
    use crate::connections::oauth::exchange::ExchangeError;

    let scratch = Scratch::new("account-bearer-unsupported");
    let wall = Arc::new(AtomicU64::new(1_800_000_000));
    let starts = Arc::new(AtomicU64::new(0));
    let calls = Arc::clone(&starts);
    let http = Catalog::answering([listing()]);
    let accounts = seeded_at(
        &scratch,
        &FakeStore::default(),
        Arc::new(move || match calls.fetch_add(1, Ordering::Relaxed) {
            0 => Ok(http.clone()),
            _ => Err(HttpError::UnsupportedPlatform),
        }),
        LATER,
        Arc::new(Ticking(Arc::clone(&wall))),
    );
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    finish_catalog(&mut brain).await;
    assert!(brain.account.models().is_some());
    wall.store(LATER, Ordering::SeqCst);
    let (first, _) = brain.account.access();
    let (answer, second) = oneshot::channel();
    brain
        .account
        .access_for(Some(ConnectionTag(SEEDED)), answer);
    timeout(PATIENCE, async {
        loop {
            let event = brain.account.next().await;
            let refreshed = matches!(
                event,
                Event::Machine(Input::Refreshed {
                    result: Err(ExchangeError::Http(HttpError::UnsupportedPlatform)),
                    ..
                })
            );
            brain
                .account_input(event, None)
                .expect("refresh completion");
            if refreshed {
                break;
            }
        }
    })
    .await
    .expect("refresh ends");
    let (third, _) = brain.account.access();
    let (answer, fourth) = oneshot::channel();
    brain
        .account
        .access_for(Some(ConnectionTag(SEEDED)), answer);
    for wait in [first, second, third, fourth] {
        assert!(matches!(
            timeout(PATIENCE, wait).await.expect("bearer answer in time").unwrap(),
            Err(Denied::Failed(reason))
                if reason.key() == ErrorKey::UnsupportedPlatform && reason.actions().is_empty()
        ));
    }
    assert_eq!(brain.account.models(), None);
    assert_unsupported_reconnects(&mut brain).await;
    assert_eq!(starts.load(Ordering::Relaxed), 2);
    brain.close().await;
}

#[tokio::test]
async fn idle_talk_back_repair_does_not_retry_the_account_catalog() {
    let scratch = Scratch::new("voice-repair-catalog");
    super::voice::seed_voice_settings(
        &scratch,
        "installed = [\"spoken-replies\"]\ntalk_back = false",
    );
    let requests = Arc::new(AtomicU64::new(0));
    let http = SharedHttp::new(Probe {
        models: Arc::clone(&requests),
        dropped: None,
    });
    let account = seeded(&scratch, &FakeStore::default(), http, LATER);
    let mut brain = test_support::brain_with(&scratch, "0 text Hello there.", account);
    let (fetcher, _fetch) = super::voice::held_fetch();
    let (release, _) = super::voice::attach_tts(&mut brain, None, fetcher);
    release.send(()).unwrap();
    let offline = ControlToShell::Error(
        UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).unwrap(),
    );

    let mut tap = Tap::from_env();
    for (generation, start) in [
        (1, PairStart::Ordinary),
        (2, PairStart::AudioRepair),
        (3, PairStart::Ordinary),
    ] {
        let ((control, data), shell) = duplex();
        let output = OutputGeneration(generation);
        let client = register_catalog_pair(shell, output, &offline);
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(
                brain.serve_with_profile(
                    control,
                    data,
                    &mut tap,
                    AudioSession::QueuedRequired,
                    None,
                    start,
                    None
                ),
                client
            )
        })
        .await
        .expect("queued pair completes");
        assert_eq!(ended, Ended::Lost);
        match generation {
            1 => assert_eq!(
                requests.load(Ordering::SeqCst),
                1,
                "initial catalog request"
            ),
            2 => {
                let pending = brain.account.catalog_read_pending();
                if pending {
                    assert_eq!(
                        read_ended(&mut brain.account).await,
                        std::slice::from_ref(&offline)
                    );
                }
                assert_eq!(
                    (requests.load(Ordering::SeqCst), pending),
                    (1, false),
                    "the audio re-pair must not start another catalog request"
                );
                brain
                    .account_input(Input::Op(AccountOp::Retry), None)
                    .unwrap();
                assert_eq!(
                    read_ended(&mut brain.account).await,
                    std::slice::from_ref(&offline)
                );
                assert_eq!(requests.load(Ordering::SeqCst), 2, "explicit Retry");
            }
            _ => assert_eq!(requests.load(Ordering::SeqCst), 3, "genuine reconnect"),
        }
    }
    brain.close().await;
}

async fn register_catalog_pair(
    mut shell: Shell<tokio::io::DuplexStream>,
    output: OutputGeneration,
    offline: &ControlToShell,
) {
    let talk_back = output != OutputGeneration(1);
    shell
        .send(&ControlToBrain::QueuedAudioOutput(
            24_000,
            output,
            Narration::Plain,
        ))
        .await;
    let (mut ready, mut registered, mut signed_in, mut failed) = (false, false, false, false);
    while !(ready && registered && signed_in && failed) {
        match shell
            .next_control()
            .await
            .unwrap()
            .expect("pair stays open")
        {
            ControlToShell::Settings {
                answer: false,
                view,
            } => assert_eq!(view.talk_back, talk_back),
            ControlToShell::Account(AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None,
                ..
            }) => signed_in = true,
            ControlToShell::Voice(view) => ready = view.spoken_replies == PartState::Ready,
            ControlToShell::QueuedAudioRegistered(actual) => {
                assert_eq!(actual, output);
                registered = true;
            }
            ControlToShell::Ping => shell.send(&ControlToBrain::Pong).await,
            frame if &frame == offline => failed = true,
            _ => {}
        }
    }
    if !talk_back {
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetTalkBack(true)))
            .await;
        loop {
            match shell
                .next_control()
                .await
                .unwrap()
                .expect("settings answered")
            {
                ControlToShell::Settings { answer: true, view } => {
                    assert!(view.talk_back);
                    break;
                }
                ControlToShell::Ping => shell.send(&ControlToBrain::Pong).await,
                _ => {}
            }
        }
    }
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
        result: Ok(Listing::fixture(models, Vec::new())),
    });
    assert_eq!(
        accounts.models(),
        None,
        "a read from before the sign-out is dropped"
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn an_all_unknown_catalog_is_retryable_and_recovers_on_a_usable_list() {
    let scratch = Scratch::new("all-unknown-efforts");
    let store = FakeStore::default();
    let http = Catalog::answering([
        (200, json!({"models": [
            {"slug": "unknown", "display_name": "Unknown", "visibility": "list",
             "supported_reasoning_levels": [{"effort": "future"}], "default_reasoning_level": "low"},
        ]}).to_string()),
        listing(),
    ]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let offline =
        UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).unwrap();
    assert_eq!(
        read_ended(&mut accounts).await,
        [ControlToShell::Error(offline)]
    );
    assert_eq!(accounts.models(), None);
    assert_eq!(accounts.models_unsent(), None);
    accounts.handle(Input::Op(AccountOp::Retry));
    assert_eq!(read_ended(&mut accounts).await, []);
    assert_eq!(
        accounts.models_unsent().as_deref().map(slugs),
        Some(vec!["gpt-6-sol", "gpt-6-luna"]),
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test]
async fn positional_catalogs_are_not_cached_and_retry_reads_the_next_response() {
    let scratch = Scratch::new("positional-catalog");
    let store = FakeStore::default();
    let http = Catalog::answering([
        (200, "[[]]".to_owned()),
        (200, r#"[[{"slug":"planted","display_name":"Planted","visibility":"list","supported_reasoning_levels":[{"effort":"low"}]}]]"#.to_owned()),
        (200, r#"{"models":[{"slug":"planted","display_name":"Planted","visibility":"list","supported_reasoning_levels":[{"effort":"low"}]},["other","Other","list"]]}"#.to_owned()),
        listing(),
    ]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let offline =
        UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).unwrap();
    for _ in 0..3 {
        assert_eq!(
            read_ended(&mut accounts).await,
            [ControlToShell::Error(offline.clone())]
        );
        assert_eq!(
            accounts.models(),
            None,
            "no positional shape authorizes any model"
        );
        assert_eq!(accounts.models_unsent(), None);
        accounts.handle(Input::Op(AccountOp::Retry));
    }
    assert_eq!(read_ended(&mut accounts).await, []);
    assert_eq!(
        accounts.models_unsent().as_deref().map(slugs),
        Some(vec!["gpt-6-sol", "gpt-6-luna"])
    );
    accounts.close(Instant::now(), Instant::now()).await;
}

/// Image input is the catalog's own fact about its own connection: a listed image model on
/// `ChatGPT` takes images; a text-only model, another connection, or a signed-out account doesn't.
#[tokio::test]
async fn only_a_listed_image_model_on_the_accounts_connection_takes_images() {
    let scratch = Scratch::new("account-images");
    let store = FakeStore::default();
    let body = json!({ "models": [
        { "slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text", "image"] },
        { "slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text"] },
    ] });
    let http = Catalog::answering([(200, body.to_string())]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let choice = |slug: &str, connection| ModelChoice {
        model: ModelSlug::new(slug).expect("slug"),
        effort: Effort::Low,
        connection,
    };
    let sol = choice("gpt-6-sol", ConnectionKind::ChatGpt);
    assert!(!accounts.takes_images(&sol), "nothing read yet");
    assert_eq!(read_ended(&mut accounts).await, []);
    assert!(accounts.takes_images(&sol));
    assert!(!accounts.takes_images(&choice("gpt-6-luna", ConnectionKind::ChatGpt)));
    assert!(!accounts.takes_images(&choice("gpt-6-sol", ConnectionKind::AnthropicKey)));
    assert!(!accounts.takes_images(&choice("gpt-6-sol", ConnectionKind::ClaudeCode)));
    accounts.handle(Input::Op(AccountOp::SignOut));
    assert!(!accounts.takes_images(&sol), "cleared with the list");
    accounts.close(Instant::now(), Instant::now()).await;
}

/// The coordinator hears the same listing's image models with the account's availability: none
/// before a read, exactly the listed ones after it, and nothing once signed out.
#[tokio::test]
async fn availability_carries_exactly_the_listings_image_models() {
    let scratch = Scratch::new("account-availability-images");
    let store = FakeStore::default();
    let body = json!({ "models": [
        { "slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text", "image"] },
        { "slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text"] },
    ] });
    let http = Catalog::answering([(200, body.to_string())]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let images = |accounts: &Accounts| match accounts.availability() {
        Availability::Ready(ready) => Some(ready.images),
        Availability::Unavailable(_) | Availability::Finding => None,
    };
    assert_eq!(images(&accounts).unwrap_or_default(), []);
    assert_eq!(read_ended(&mut accounts).await, []);
    assert_eq!(
        images(&accounts),
        Some(vec![ModelSlug::new("gpt-6-sol").expect("slug")])
    );
    accounts.handle(Input::Op(AccountOp::SignOut));
    assert_eq!(images(&accounts), None);
    accounts.close(Instant::now(), Instant::now()).await;
}

/// The image gate's four states, each from its own condition, and closed again after sign-out.
#[tokio::test]
async fn the_image_gate_opens_only_for_a_listed_image_model_where_windows_can_be_captured() {
    use kumochi_protocol::message::{ModelName, ScreenHolds, ScreenOff};
    let scratch = Scratch::new("account-image-gate");
    let store = FakeStore::default();
    let body = json!({ "models": [
        { "slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text", "image"] },
        { "slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}],
          "input_modalities": ["text"] },
    ] });
    let http = Catalog::answering([(200, body.to_string())]);
    let mut accounts = seeded(&scratch, &store, http, LATER);
    let choice = |slug: &str, connection| ModelChoice {
        model: ModelSlug::new(slug).expect("slug"),
        effort: Effort::Low,
        connection,
    };
    let sol = choice("gpt-6-sol", ConnectionKind::ChatGpt);
    let off = ScreenHolds::Off;
    assert_eq!(
        accounts.screen_holds(Some(&sol), true, true),
        off(ScreenOff::NotReady)
    );
    assert_eq!(
        accounts.screen_holds(Some(&sol), true, false),
        off(ScreenOff::TurnedOff),
        "the switch comes before the catalog"
    );
    assert_eq!(read_ended(&mut accounts).await, []);
    assert_eq!(
        accounts.screen_holds(Some(&sol), true, true),
        ScreenHolds::On {
            model: ModelName::new("GPT-6 Sol").expect("name"),
            connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
            slug: kumochi_protocol::message::ModelSlug::new("gpt-6-sol").expect("slug"),
            revision: kumochi_protocol::message::NoticeRevision::FIRST,
        }
    );
    assert_eq!(
        accounts.screen_holds(Some(&sol), false, true),
        off(ScreenOff::Unsupported)
    );
    assert_eq!(
        accounts.screen_holds(None, true, true),
        off(ScreenOff::NotReady)
    );
    assert_eq!(
        accounts.screen_holds(None, true, false),
        off(ScreenOff::TurnedOff)
    );
    assert_eq!(
        accounts.screen_holds(Some(&sol), true, false),
        off(ScreenOff::TurnedOff)
    );
    assert_eq!(
        accounts.screen_holds(Some(&sol), false, false),
        off(ScreenOff::Unsupported)
    );
    for other in [
        choice("gpt-6-luna", ConnectionKind::ChatGpt),
        choice("gpt-6-sol", ConnectionKind::AnthropicKey),
        choice("gpt-6-nova", ConnectionKind::ChatGpt),
    ] {
        assert_eq!(
            accounts.screen_holds(Some(&other), true, true),
            off(ScreenOff::ModelCantSee)
        );
        assert_eq!(
            accounts.screen_holds(Some(&other), true, false),
            off(ScreenOff::TurnedOff)
        );
    }
    accounts.handle(Input::Op(AccountOp::SignOut));
    assert_eq!(
        accounts.screen_holds(Some(&sol), true, true),
        off(ScreenOff::NotReady)
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
            images: Vec::new(),
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
    prove(&mut accounts).await;
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
    use kumochi_protocol::message::{ScreenHolds, ScreenOff};
    let scratch = Scratch::new("account-catalog-brain");
    let store = FakeStore::default();
    let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    let _rig = crate::runtime::hold::tests::attach_capture_fixture(&mut brain);
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
        loop {
            let next = timeout(PATIENCE, shell.next_control())
                .await
                .expect("in time");
            if let Some(ControlToShell::ScreenHolds(state)) = next.expect("readable") {
                assert_eq!(
                    state,
                    ScreenHolds::Off(ScreenOff::ModelCantSee),
                    "luna can't see"
                );
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

/// [`Catalog`]'s answers, each held until its release fires.
struct HeldCatalog(Catalog, Mutex<Option<oneshot::Receiver<()>>>);

impl Http for HeldCatalog {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        let release = self.1.lock().expect("gate").take();
        let answer = self.0.execute(request);
        async move {
            if let Some(release) = release {
                release.await.ok();
            }
            answer.await
        }
    }
}

/// A signed-in brain on `http` with a fixture screen worker and Luna, which can't see, already
/// saved as the default model.
fn luna_with_a_screen(
    scratch: &Scratch,
    store: &FakeStore,
    http: SharedHttp,
) -> (
    crate::runtime::Brain,
    crate::supervise::fixture::Rig<crate::screen::Holds>,
) {
    let accounts = seeded(scratch, store, http, LATER);
    let mut brain = test_support::brain_with(scratch, "", accounts);
    let rig = crate::runtime::hold::tests::attach_capture_fixture(&mut brain);
    let pending = brain
        .config
        .stage(kumochi_protocol::message::SettingsOp::SetDefaultModel(
            ModelChoice {
                model: ModelSlug::new("gpt-6-luna").expect("valid"),
                effort: Effort::Low,
                connection: ConnectionKind::ChatGpt,
            },
        ))
        .expect("staged");
    pending.write().expect("written");
    brain.config.commit(pending);
    (brain, rig)
}

/// With the default model already saved, a catalog read that lands while a shell is connected
/// moves the screen gate, and the shell hears the new state.
#[tokio::test]
async fn a_catalog_read_while_connected_sends_the_screen_gate_it_moved() {
    use kumochi_protocol::message::{ScreenHolds, ScreenOff};
    let scratch = Scratch::new("account-catalog-screen-gate");
    let store = FakeStore::default();
    let (release, released) = oneshot::channel();
    let http = SharedHttp::new(HeldCatalog(
        Catalog(Mutex::new(VecDeque::from([listing()]))),
        Mutex::new(Some(released)),
    ));
    let (mut brain, rig) = luna_with_a_screen(&scratch, &store, http);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let mut gates = Vec::new();
        loop {
            let next = timeout(PATIENCE, shell.next_control())
                .await
                .expect("in time");
            match next.expect("readable").expect("open") {
                ControlToShell::ScreenHolds(state) => gates.push(state),
                ControlToShell::Scenes { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            gates,
            [ScreenHolds::Off(ScreenOff::NotReady)],
            "the read is held"
        );
        release.send(()).expect("the read waits");
        loop {
            let next = timeout(PATIENCE, shell.next_control())
                .await
                .expect("in time");
            if let Some(ControlToShell::ScreenHolds(state)) = next.expect("readable") {
                assert_eq!(state, ScreenHolds::Off(ScreenOff::ModelCantSee));
                break;
            }
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    drop(rig);
}

/// A sign-out the shell sends clears the catalog at once, so its own control frame sends the
/// gate it moved, before the credential thread or the revocation answers anything.
#[tokio::test]
async fn a_sign_out_from_the_shell_sends_the_gate_it_moved_at_once() {
    use kumochi_protocol::message::{ScreenHolds, ScreenOff};
    let scratch = Scratch::new("account-sign-out-screen-gate");
    let store = FakeStore::default();
    let (mut brain, rig) = luna_with_a_screen(&scratch, &store, Catalog::answering([listing()]));
    let (out, mut output) = mpsc::channel(64);
    let queue = std::cell::RefCell::new(crate::runtime::data::DataQueue::new(
        crate::runtime::window(),
    ));
    let ready = tokio::sync::Notify::new();
    let mut tap = Tap::from_env();
    let mut link = crate::runtime::Link::new(&out, &queue, &ready, &mut tap);
    while brain.account.models().is_none() {
        let input = timeout(PATIENCE, brain.account.next())
            .await
            .expect("in time");
        brain.account_input(input, Some(&mut link)).expect("room");
    }
    brain.screen_holds_changed(&mut link).expect("room");
    let gates = |output: &mut mpsc::Receiver<Vec<u8>>| {
        std::iter::from_fn(|| output.try_recv().ok())
            .filter_map(|bytes| {
                match kumochi_protocol::frame::decode(&bytes[kumochi_protocol::frame::HEADER_LEN..])
                    .expect("a control frame")
                {
                    ControlToShell::ScreenHolds(state) => Some(state),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        gates(&mut output),
        [ScreenHolds::Off(ScreenOff::ModelCantSee)]
    );
    let sign_out = ControlToBrain::AccountOp(AccountOp::SignOut);
    assert!(matches!(
        brain.control_frame(sign_out, 0, &mut link, &mut None),
        Ok(None)
    ));
    assert_eq!(gates(&mut output), [ScreenHolds::Off(ScreenOff::NotReady)]);
    drop(link);
    brain.close().await;
    drop(rig);
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
                    require_recovery: None,
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

/// A Claude Code connection a launch would be admitted on, on the lane's fake signed in.
#[cfg(windows)]
async fn ready_claude(
    base: &crate::engines::claude_code::lane::tests::Base,
) -> crate::runtime::claude::Claude {
    let (host, _switches) = crate::engines::claude_code::lane::tests::host(base, "in");
    let lane = crate::engines::claude_code::lane::Lane::start(host).expect("lane");
    let mut claude = crate::runtime::claude::Claude::start(
        lane,
        crate::runtime::claude::SignInStore::FileWithDacl,
    );
    timeout(PATIENCE, async {
        while claude.view() != crate::runtime::claude::tests::READY {
            let wake = claude.next().await;
            claude.wake(wake);
        }
    })
    .await
    .expect("ready");
    claude
}

/// OSC-754: a wake that makes Claude Code ready writes the row's default, haiku at Low, with no op
/// from the shell.
#[cfg(windows)]
#[tokio::test]
async fn a_wake_that_makes_claude_code_ready_writes_its_default() {
    let base = crate::engines::claude_code::lane::tests::Base::new("default-on-wake");
    let scratch = Scratch::new("default-on-wake");
    let mut brain = test_support::brain(&scratch, "");
    let (host, _switches) = crate::engines::claude_code::lane::tests::host(&base, "in");
    brain.attach_claude(crate::runtime::claude::Claude::start(
        crate::engines::claude_code::lane::Lane::start(host).expect("lane"),
        crate::runtime::claude::SignInStore::FileWithDacl,
    ));
    timeout(PATIENCE, async {
        while brain.claude.view() != crate::runtime::claude::tests::READY {
            let wake = brain.claude.next().await;
            brain.claude_done(wake, None).expect("no link");
        }
    })
    .await
    .expect("ready");
    brain.drain_settings().await;
    assert_eq!(
        scratch.config().view().default_model,
        Some(ModelChoice {
            model: ModelSlug::new("haiku").expect("valid"),
            effort: Effort::Low,
            connection: ConnectionKind::ClaudeCode,
        })
    );
    brain.close().await;
}

/// M1: Claude Code's default goes in only while `ChatGPT` has no account at all, never while a
/// `ChatGPT` account's session has ended, and a queued repair recomputed at write time never
/// turns into the other connection's.
#[cfg(windows)]
#[tokio::test]
async fn claude_codes_default_waits_for_chatgpt_to_have_no_account() {
    let repair = |model: &str, connection| crate::runtime::settings::Queued {
        change: crate::runtime::settings::Change::User(
            kumochi_protocol::message::SettingsOp::SetDefaultModel(ModelChoice {
                model: ModelSlug::new(model).expect("valid"),
                effort: Effort::Low,
                connection,
            }),
        ),
        epoch: crate::runtime::settings::BRAIN,
    };
    let base = crate::engines::claude_code::lane::tests::Base::new("default-claude-only");
    let scratch = Scratch::new("default-claude-only");
    let mut brain = test_support::brain(&scratch, "");
    brain.attach_claude(ready_claude(&base).await);
    assert_eq!(
        brain.repaired_default(),
        Some(ModelChoice {
            model: ModelSlug::new("haiku").expect("valid"),
            effort: Effort::Low,
            connection: ConnectionKind::ClaudeCode,
        })
    );
    brain
        .settings_ops
        .push_back(repair("gpt-6-luna", ConnectionKind::ChatGpt));
    brain.pump_settings(None).expect("no link");
    assert!(brain.settings_ops.is_empty(), "taken");
    assert!(
        brain.config_write.is_none(),
        "ChatGPT's repair isn't written as Claude Code's"
    );
    brain
        .claude_op(
            kumochi_protocol::message::ConnectionOp::FindClaudeCode,
            None,
        )
        .expect("no link");
    assert!(
        brain.config_write.is_some(),
        "Claude Code's report queues its own repair"
    );
    brain.close().await;

    let base = crate::engines::claude_code::lane::tests::Base::new("default-chatgpt-ended");
    let scratch = Scratch::new("default-chatgpt-ended");
    let store = FakeStore::default();
    let timed_out: TokenAnswer = || Err(HttpError::Timeout);
    let mut accounts = seeded(
        &scratch,
        &store,
        SharedHttp::new(Renewing(timed_out)),
        1_800_000_000,
    );
    timeout(PATIENCE, async {
        while !matches!(
            accounts.view(),
            AccountView::SignedIn {
                recovery: Some(AccountRecovery::SignInAgain),
                ..
            }
        ) {
            let event = accounts.next().await;
            accounts.handle(event);
        }
    })
    .await
    .expect("the session ends in time");
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    brain.attach_claude(ready_claude(&base).await);
    assert_eq!(brain.repaired_default(), None);
    brain.close().await;
}

/// An ordinary sign-in is ready only once its check proves it, with no bearer out before then;
/// a refresh keeps its connection, so it stays ready throughout with no second check.
#[tokio::test]
async fn an_ordinary_sign_in_is_ready_once_proved_and_stays_ready_through_a_refresh() {
    let scratch = Scratch::new("account-proof-refresh");
    let store = FakeStore::default();
    let mut accounts = due_account(&scratch, &store, SharedHttp::new(Rotating));
    assert_eq!(accounts.availability(), Availability::Finding);
    assert!(accounts.reads_fenced());
    assert!(
        !accounts.catalog_read_pending(),
        "no bearer before the check"
    );
    prove(&mut accounts).await;
    let ready = |accounts: &Accounts| {
        matches!(accounts.availability(), Availability::Ready(ready)
            if ready.connection == ConnectionTag(SEEDED))
    };
    assert!(ready(&accounts));
    assert!(!accounts.reads_fenced());
    assert!(
        accounts.catalog_read_pending(),
        "the held catalog read goes"
    );
    // The store is read only once the worker reports the save after the refresh.
    let (mut refreshed, mut saved) = (false, false);
    for _ in 0..8 {
        let input = timeout(PATIENCE, accounts.next()).await.expect("in time");
        assert!(!matches!(input, Event::Proof { .. }), "no second check");
        let rotated = matches!(input, Event::Machine(Input::Vault(Done::Rotated(..))));
        refreshed |= matches!(input, Event::Machine(Input::Refreshed { .. }));
        accounts.handle(input);
        assert!(ready(&accounts));
        saved = refreshed && rotated;
        if saved {
            break;
        }
    }
    assert!(saved, "the rotated set's save was reported");
    assert_eq!(saved_refresh(&scratch, &store).0, "refresh-new");
    assert_eq!(saved_connection(&scratch, &store), SEEDED);
    accounts.close(Instant::now(), Instant::now()).await;
}

/// An unproved sign-in isn't ready and gets no bearer, and a later answer can't lift that:
/// a failure stays failed.
#[tokio::test]
async fn an_unproved_sign_in_stays_unready_without_a_bearer() {
    use crate::connections::vault::Proof;

    let scratch = Scratch::new("account-proof-unproved");
    let store = FakeStore::default();
    let mut accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
    let unproved = Event::Proof {
        connection: SEEDED,
        epoch: 1,
        proof: Proof::Unproved,
    };
    assert!(accounts.handle(unproved).is_empty());
    let refused = |accounts: &Accounts| {
        matches!(accounts.availability(), Availability::Unavailable(error)
            if error.key() == kumochi_protocol::message::ErrorKey::SignInNeeded)
    };
    assert!(refused(&accounts));
    assert!(accounts.reads_fenced());
    assert!(!accounts.catalog_read_pending());
    let (answer, denied) = oneshot::channel();
    accounts.access_for(Some(ConnectionTag(SEEDED)), answer);
    assert!(matches!(denied.await, Ok(Err(Denied::Unusable))));
    let late = timeout(PATIENCE, accounts.next()).await.expect("in time");
    assert!(matches!(
        late,
        Event::Proof {
            proof: Proof::Ordinary,
            ..
        }
    ));
    accounts.handle(late);
    assert!(refused(&accounts));
    assert!(!accounts.catalog_read_pending());
}

/// The engine a brain answers with when runs need the account; it never answers.
fn account_engine() -> crate::runtime::Selected {
    crate::runtime::Selected::Account {
        engine: Box::new(CountStarts(Arc::new(AtomicU64::new(0)))),
        asks: mpsc::unbounded_channel().1,
    }
}

/// A turn's first build, as the coordinator hands it to the reader.
fn first_build(brain: &mut crate::runtime::Brain) -> crate::coordinator::BuildContext {
    use crate::coordinator::{Effect, Input as CoreInput};

    let begun = brain
        .handle(CoreInput::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale");
    let run = begun
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BeginTurn { run, .. }) => Some(*run),
            _ => None,
        })
        .expect("a turn begins");
    brain
        .handle(CoreInput::TurnStored { run })
        .expect("current")
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build),
            _ => None,
        })
        .expect("a build starts")
}

/// A turn that arrives while the sign-in's check is out carries `ChatGPT`'s block into its
/// first build, so the reader reads no history for it; once the check proves the sign-in, the
/// next arrival doesn't.
#[tokio::test]
async fn a_turn_before_the_proof_arrives_blocked_from_chatgpt_history() {
    for proved in [false, true] {
        let scratch = Scratch::new("account-proof-arrival");
        let accounts = seeded(
            &scratch,
            &FakeStore::default(),
            Catalog::answering([listing()]),
            LATER,
        );
        let mut brain = test_support::brain_on(&scratch, account_engine(), accounts);
        if proved {
            let event = timeout(PATIENCE, brain.account.next())
                .await
                .expect("in time");
            assert!(matches!(event, Event::Proof { .. }));
            brain.account_input(event, None).expect("no shell");
        } else {
            brain.availability_changed(None).expect("no shell");
        }
        let arrived = first_build(&mut brain).arrived;
        assert_eq!(arrived.chatgpt_blocked(), !proved, "proved: {proved}");
        brain.close().await;
    }
}

/// A saved sign-in the local check reads back is held before its check answers, so a turn in
/// that interval reads no `ChatGPT` history; once an ordinary answer comes, the next doesn't.
#[tokio::test]
async fn a_locally_checked_sign_in_is_fenced_until_its_check_answers() {
    use std::collections::BTreeMap;

    for proved in [false, true] {
        let scratch = Scratch::new("account-proof-local-check");
        let store = FakeStore::default();
        let seed = scratch.db().with_file_name("seed.lock");
        Vault::with_service(store.clone(), SHUTDOWN_SERVICE, &seed)
            .expect("vault")
            .host_id()
            .expect("host");
        save_seeded_account(&scratch, &store, LATER);
        let prefix = format!("{SHUTDOWN_SERVICE}/gen/chatgpt/");
        let names: Vec<_> = store
            .names()
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .collect();
        assert_eq!(names.len(), 1);
        let snapshot = store.snapshot();
        store.rollback(&BTreeMap::new(), &names);
        let lock = scratch.db().with_file_name(SESSION_LOCK);
        let opening = store.clone();
        let worker = Worker::new(
            Box::new(move || Vault::with_service(opening.clone(), SHUTDOWN_SERVICE, &lock)),
            Arc::new(FixedClock(1_800_000_000)),
        );
        let http = Catalog::answering([listing()]);
        let net = Net {
            http: Arc::new(move || Ok(http.clone())),
            ..test_support::net()
        };
        let accounts = Accounts::spawn_unconfigured(worker, net).expect("accounts");
        assert!(!accounts.reads_fenced(), "nothing readable is held yet");
        store.rollback(&snapshot, &names);
        let mut brain = test_support::brain_on(&scratch, account_engine(), accounts);
        brain
            .account_input(
                Input::Op(kumochi_protocol::message::AccountOp::CheckSavedSignIn),
                None,
            )
            .expect("no shell");
        let checked = timeout(PATIENCE, brain.account.next())
            .await
            .expect("in time");
        assert!(matches!(
            checked,
            Event::Machine(Input::Vault(Done::CheckedSavedSignIn(_)))
        ));
        brain.account_input(checked, None).expect("no shell");
        if proved {
            let proof = timeout(PATIENCE, brain.account.next())
                .await
                .expect("in time");
            assert!(matches!(proof, Event::Proof { .. }));
            brain.account_input(proof, None).expect("no shell");
        }
        let arrived = first_build(&mut brain).arrived;
        assert_eq!(arrived.chatgpt_blocked(), !proved, "proved: {proved}");
        brain.close().await;
    }
}

/// From the producer to the build: a current recovered answer gives the first build its exact
/// cutoff; after the vault's own later answer for that check is dropped and an ordinary refresh
/// rotates the set, the build still keeps to it; and once that account can't be ready, the next
/// build reads no `ChatGPT` history at all, while a ready Claude Code still arrives.
#[tokio::test]
async fn a_recovered_answer_scopes_builds_until_the_account_stops_being_ready() {
    use crate::connections::vault::Proof;

    let cut = crate::store::Boundary::fixture(SEEDED, 3, 9);
    for arm in ["first", "refreshed", "unavailable"] {
        let scratch = Scratch::new("account-proof-recovered");
        let store = FakeStore::default();
        let accounts = due_account(&scratch, &store, SharedHttp::new(Rotating));
        let mut brain = test_support::brain_on(&scratch, account_engine(), accounts);
        let recovered = Event::Proof {
            connection: SEEDED,
            epoch: 1,
            proof: Proof::Recovered(cut),
        };
        brain.account_input(recovered, None).expect("no shell");
        if arm == "refreshed" {
            // The store is read only once the worker reports the save after the refresh.
            let (mut dropped, mut refreshed, mut rotated) = (false, false, false);
            for _ in 0..12 {
                if dropped && rotated {
                    break;
                }
                let event = timeout(PATIENCE, brain.account.next())
                    .await
                    .expect("in time");
                rotated |=
                    refreshed && matches!(event, Event::Machine(Input::Vault(Done::Rotated(..))));
                refreshed |= matches!(event, Event::Machine(Input::Refreshed { .. }));
                dropped |= matches!(
                    event,
                    Event::Proof {
                        proof: Proof::Ordinary,
                        ..
                    }
                );
                brain.account_input(event, None).expect("no shell");
            }
            assert!(dropped, "the vault's own answer came after");
            assert!(rotated, "the rotated set's save was reported");
            assert_eq!(saved_refresh(&scratch, &store).0, "refresh-new");
            assert_eq!(saved_connection(&scratch, &store), SEEDED);
        }
        if arm == "unavailable" {
            brain
                .account_input(Input::PlanUnavailable, None)
                .expect("no shell");
        }
        #[cfg(windows)]
        let claude = if arm == "unavailable" {
            let base =
                crate::engines::claude_code::lane::tests::Base::new("account-proof-recovered");
            brain.attach_claude(ready_claude(&base).await);
            let Availability::Ready(ready) = brain.claude.availability() else {
                panic!("Claude Code is ready");
            };
            Some(ready.connection)
        } else {
            None
        };
        let build = first_build(&mut brain);
        if arm == "unavailable" {
            assert!(build.arrived.chatgpt_blocked());
            assert_eq!(build.require_recovery, None);
            #[cfg(windows)]
            {
                let choice = ModelChoice {
                    model: ModelSlug::new("haiku").expect("valid"),
                    effort: Effort::Low,
                    connection: ConnectionKind::ClaudeCode,
                };
                assert_eq!(
                    Some(build.arrived.replay(Some(&choice))),
                    claude,
                    "Claude Code still arrives ready"
                );
            }
        } else {
            assert!(!build.arrived.chatgpt_blocked(), "{arm}");
            assert_eq!(build.arrived.recovered(), Some(cut), "{arm}");
            assert_eq!(build.require_recovery, Some(cut), "{arm}");
        }
        brain.close().await;
    }
}
