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
