use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, Request, Response};
use crate::connections::oauth::{LaunchError, LaunchTarget};

struct Counted(Arc<AtomicBool>);

impl Http for Counted {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.0.store(true, Ordering::SeqCst);
        std::future::ready(Err(HttpError::Transport))
    }
}

struct Models;

impl Http for Models {
    fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, HttpError>> + Send {
        assert!(matches!(
            request.endpoint,
            crate::connections::http_client::Endpoint::Models(_)
        ));
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"models":[{"slug":"fake-model","display_name":"Fake model","visibility":"list","supported_reasoning_levels":[{"effort":"low"}]}]}"#.to_vec())),
        }))
    }
}

fn save_checkable_sign_in(
    vault: &mut crate::connections::vault::Vault<crate::connections::vault::fake::FakeStore>,
) {
    use crate::connections::vault::SaveResult;

    vault.host_id().unwrap();
    assert!(matches!(
        vault.save_account(&Secret::new(Account {
            client_id: Zeroizing::new("fake-client".into()),
            subject: Some(Zeroizing::new("fake-subject".into())),
            email: None,
            plan_enabled: true,
            tokens: Some(crate::connections::records::Tokens {
                access_token: Zeroizing::new("fake-access".into()),
                refresh_token: Zeroizing::new("fake-refresh".into()),
                id_token: None,
                scope: Zeroizing::new("chatgpt.tokens.use.direct".into()),
                expires_at: 1_800_003_600,
                pending_identity_validation: false,
                refresh_sent: false,
                connection: 42,
            }),
        })),
        SaveResult::Committed
    ));
}

#[tokio::test]
async fn a_recovered_saved_sign_in_loads_models_only_on_explicit_request() {
    recovered_by(AccountOp::CheckSavedSignIn, "account-local-check").await;
}

/// A Forget that finds the sign-in readable again recovers it exactly as Check does.
#[tokio::test]
async fn a_forget_that_reads_the_sign_in_again_recovers_it_as_check_does() {
    recovered_by(AccountOp::ForgetSavedSignIn(1), "account-local-forget").await;
}

async fn recovered_by(op: AccountOp, service: &'static str) {
    use crate::connections::vault::{Vault, fake::FakeStore};
    use kumochi_protocol::message::SavedSignInFailure;

    let scratch = crate::test_support::Scratch::new(service);
    let store = FakeStore::default();
    let lock = scratch
        .dir()
        .join(kumochi_protocol::constants::SESSION_LOCK);
    {
        let mut vault = Vault::with_service(store.clone(), service, &lock).unwrap();
        save_checkable_sign_in(&mut vault);
    }
    let names: Vec<_> = store
        .names()
        .into_iter()
        .filter(|name| name.starts_with(&format!("{service}/gen/chatgpt/")))
        .collect();
    assert_eq!(names.len(), 1);
    let snapshot = store.snapshot();
    store.rollback(&BTreeMap::new(), &names);
    let net = Net {
        http: Arc::new(|| Ok(SharedHttp::new(Models))),
        ..crate::test_support::net()
    };
    let opening = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(opening.clone(), service, &lock)),
        Arc::clone(&net.clock),
    );
    let mut accounts = Accounts::spawn_unconfigured(worker, net).unwrap();
    let before = accounts.availability();
    assert_eq!(
        accounts.view(),
        AccountView::SavedSignInUnavailable {
            reason: SavedSignInFailure::Missing,
            checkable: true,
            forget: Some(1),
            checking: false,
            revocation_unconfirmed: false,
        },
    );
    store.rollback(&snapshot, &names);
    accounts.handle(Input::Op(op));
    let checked = tokio::time::timeout(Duration::from_secs(5), accounts.next())
        .await
        .unwrap();
    assert!(matches!(
        checked,
        Event::Machine(Input::Vault(Done::CheckedSavedSignIn(_)))
    ));
    assert!(matches!(
        accounts.handle(checked).as_slice(),
        [ControlToShell::Account(AccountView::SignedIn {
            recovery: Some(kumochi_protocol::message::AccountRecovery::LoadModels),
            ..
        })],
    ));
    let proof = tokio::time::timeout(Duration::from_secs(5), accounts.next())
        .await
        .unwrap();
    assert!(matches!(proof, Event::Proof { .. }));
    assert!(accounts.handle(proof).is_empty());
    accounts.handle(Input::Connected);
    assert!(
        accounts.catalog.read.is_none(),
        "neither Check nor a connected shell requested a catalog or bearer"
    );
    assert!(accounts.tickets.is_empty());
    assert_eq!(
        accounts.availability(),
        before,
        "a local Check cannot change the route"
    );
    assert_eq!(accounts.sendable(), None);
    accounts.handle(Input::Op(AccountOp::LoadModels));
    let read = tokio::time::timeout(Duration::from_secs(5), accounts.next())
        .await
        .unwrap();
    assert!(matches!(
        accounts.handle(read).as_slice(),
        [ControlToShell::Account(AccountView::SignedIn {
            recovery: None,
            ..
        })],
    ));
    let [model] = accounts
        .models()
        .expect("catalog available on the same link")
    else {
        panic!("one model");
    };
    assert_eq!(model.slug.as_str(), "fake-model");
    assert!(matches!(accounts.availability(), Availability::Ready(_)));
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn account_close_fences_launch_before_tokio_polls_the_aborted_listener() {
    struct Opener(Arc<AtomicUsize>);
    impl Launcher for Opener {
        fn launch(&self, _: &LaunchTarget) -> Result<(), LaunchError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let scratch = crate::test_support::Scratch::new("account-close-browser-fence");
    let mut accounts = crate::test_support::accounts(&scratch);
    let opened = Arc::new(AtomicUsize::new(0));
    accounts.net.launcher = Arc::new(Opener(Arc::clone(&opened)));
    let wait = Duration::from_secs(10);
    let (holding, held) = oneshot::channel();
    let (release, released) = std_mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _spawn = crate::platform::spawn_lock();
        holding.send(()).unwrap();
        released.recv_timeout(wait).unwrap();
    });
    tokio::time::timeout(wait, held).await.unwrap().unwrap();

    accounts.handle(Input::Op(AccountOp::SignIn));
    let listener = accounts.listener.as_ref().expect("sign-in listener");
    let pending = listener.task.abort_handle();
    let cancelled = Arc::clone(&listener.cancelled);
    tokio::time::timeout(wait, async {
        while Arc::strong_count(&cancelled) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the blocking worker owns a launch permission");

    accounts.handle(Input::Op(AccountOp::CancelSignIn));
    release.send(()).unwrap();
    holder.join().unwrap();
    let started = std::time::Instant::now();
    while Arc::strong_count(&cancelled) > 2 {
        assert!(
            started.elapsed() < wait,
            "the blocking worker did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !pending.is_finished(),
        "the listener has not been polled since Close"
    );
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    accounts.close(Instant::now(), Instant::now()).await;
}

#[tokio::test(start_paused = true)]
async fn refresh_and_revocation_time_out_before_a_held_factory_can_dispatch() {
    for refreshing in [true, false] {
        let (entered, building) = oneshot::channel();
        let (built, built_client) = oneshot::channel();
        let (release, held) = std_mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicBool::new(false));
        let factory: HttpFactory = {
            let entered = Mutex::new(Some(entered));
            let held = Mutex::new(held);
            let built = Mutex::new(Some(built));
            let sent = Arc::clone(&sent);
            let finished = Arc::clone(&finished);
            Arc::new(move || {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                held.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .ok();
                finished.store(true, Ordering::SeqCst);
                built.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(SharedHttp::new(Counted(Arc::clone(&sent))))
            })
        };
        let (events, mut replied) = mpsc::unbounded_channel();
        let deadline = Instant::now() + Duration::from_secs(8);
        let work = tokio::spawn(async move {
            if refreshing {
                let net = Net {
                    http: factory,
                    clock: Arc::new(FixedClock(1_800_000_000)),
                    ..crate::test_support::net()
                };
                refresh_task(
                    1,
                    Secret::new(Zeroizing::new("fixture-client".into())),
                    Secret::new(Zeroizing::new("fixture-refresh".into())),
                    deadline,
                    net,
                    events,
                )
                .await;
            } else {
                revoke_task(
                    Vec::new(),
                    deadline,
                    OAuthTuning::default().backoff,
                    factory,
                    events,
                )
                .await;
            }
        });
        building.await.unwrap();
        tokio::time::advance(Duration::from_secs(8)).await;
        match (refreshing, replied.recv().await.unwrap()) {
            (
                true,
                Input::Refreshed {
                    generation: 1,
                    result: Err(ExchangeError::PreDispatchTimeout),
                    ..
                },
            ) => {}
            (false, Input::Revoked(confirmed)) => assert!(confirmed.is_empty()),
            _ => panic!("expected a definitely unsent result"),
        }
        assert_eq!(Instant::now(), deadline);
        assert!(
            !finished.load(Ordering::SeqCst),
            "setup is still held when the result settles"
        );
        release.send(()).unwrap();
        built_client.await.unwrap();
        work.await.unwrap();
        assert!(!sent.load(Ordering::SeqCst));
    }
}
