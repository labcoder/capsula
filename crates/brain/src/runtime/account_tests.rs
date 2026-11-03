//! The account system over the pipes: its view's place in a connection, an op's answer, a
//! sign-in the real listener ends, and a Busy session lock retried when a shell connects.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{AccountOp, AccountView, ControlToBrain, ControlToShell, ErrorKey};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, timeout};
use zeroize::Zeroizing;

use super::{Ended, PATIENCE, Shell, duplex};
use crate::connections::account::{Input, Listened, Worker};
use crate::connections::clock::FixedClock;
use crate::connections::oauth::{Authorization, LaunchError, LaunchTarget, Launcher};
use crate::connections::records::Account;
use crate::connections::vault::fake::FakeStore;
use crate::connections::vault::{SaveResult, Vault};
use crate::ipc;
use crate::platform::SessionLock;
use crate::runtime::account::{Accounts, Net};
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

const SIGNED_OUT: AccountView = AccountView::SignedOut {
    revocation_unconfirmed: false,
};

impl<S: AsyncRead + AsyncWrite> Shell<S> {
    /// The next control frame, whatever it is.
    async fn frame(&mut self) -> ControlToShell {
        let next = ipc::read::<ControlToShell>(&mut self.control_in, Pipe::Control);
        timeout(PATIENCE, next)
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
    accounts.close(Instant::now()).await;
}

/// L4: closing waits for the credential job already running, past its deadline, so the
/// instance capability's delete never overlaps it.
#[tokio::test]
async fn closing_waits_for_a_running_credential_job_past_its_deadline() {
    let scratch = Scratch::new("account-close");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let held = SessionLock::try_acquire(&lock).expect("lock");
    let gate: Arc<Mutex<Option<std_mpsc::Receiver<()>>>> = Arc::default();
    let finished = Arc::new(AtomicBool::new(false));
    let (opening, opened) = (Arc::clone(&gate), Arc::clone(&finished));
    let worker = Worker::new(
        Box::new(move || {
            let waiting = opening.lock().expect("gate").take();
            if let Some(waiting) = waiting {
                waiting.recv().ok();
                opened.store(true, Ordering::SeqCst);
            }
            Vault::with_service(FakeStore::default(), "test-accounts-close", &lock)
        }),
        Arc::new(FixedClock(1_800_000_000)),
    );
    let mut accounts = Accounts::spawn(worker, test_support::net()).expect("accounts");
    let (release, waiting) = std_mpsc::channel();
    *gate.lock().expect("gate") = Some(waiting);
    // Busy at startup, so a connection reads again, on the credentials thread.
    accounts.handle(Input::Connected);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).ok();
    });
    accounts.close(Instant::now()).await;
    assert!(
        finished.load(Ordering::SeqCst),
        "close returned while the job ran"
    );
    releaser.join().expect("released");
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
    accounts.close(Instant::now()).await;
}
