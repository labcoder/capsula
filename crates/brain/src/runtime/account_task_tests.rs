use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, Request, Response};

struct Counted(Arc<AtomicBool>);

impl Http for Counted {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.0.store(true, Ordering::SeqCst);
        std::future::ready(Err(HttpError::Transport))
    }
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
