use super::*;
use std::collections::VecDeque;
use std::future::{Future, pending, poll_fn};
use std::sync::Mutex;
use std::task::Poll;
use std::time::Duration;

use crate::connections::clock::FixedClock;
use crate::connections::github::{ClientId, GitHubToken, Page, issues::IssueContent};
use crate::connections::http_client::ResponseMeta;
use crate::test_support::SENTINEL;
use kumochi_protocol::{
    Secret,
    message::{GitHubLogin, IssueBody, IssueTitle, RepoName},
};
use zeroize::Zeroizing;

#[derive(Clone)]
enum Step {
    Reply(Option<u64>),
    Fail(HttpError),
    Pending,
}

#[derive(Clone)]
struct Script {
    steps: Arc<Mutex<VecDeque<Step>>>,
    calls: Arc<AtomicUsize>,
    creates: Arc<AtomicUsize>,
}

impl Script {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            calls: Arc::new(AtomicUsize::new(0)),
            creates: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Http for Script {
    fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(
            request.endpoint,
            Endpoint::GitHub(GitHubEndpoint::CreateIssue { .. })
        ) {
            self.creates.fetch_add(1, Ordering::SeqCst);
        }
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("one scripted request");
        async move {
            match step {
                Step::Reply(date_s) => Ok(Response {
                    status: 200,
                    request_id: Some(SENTINEL.to_owned()),
                    meta: ResponseMeta {
                        date_s,
                        ..ResponseMeta::default()
                    },
                    body: Secret::new(Zeroizing::new(SENTINEL.as_bytes().to_vec())),
                }),
                Step::Fail(error) => Err(error),
                Step::Pending => pending().await,
            }
        }
    }
}

fn token() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new(format!("ghu_{SENTINEL}"))).unwrap())
}

fn get() -> Request {
    Request {
        endpoint: Endpoint::GitHub(GitHubEndpoint::User { token: token() }),
        fields: Vec::new(),
    }
}

fn post() -> Request {
    let title = IssueTitle::new(SENTINEL).unwrap();
    let body = IssueBody::new(SENTINEL).unwrap();
    Request {
        endpoint: Endpoint::GitHub(GitHubEndpoint::CreateIssue {
            token: token(),
            repo: RepoName::new(&format!("{SENTINEL}/{SENTINEL}")).unwrap(),
            body: IssueContent {
                title: &title,
                body: &body,
            }
            .into_payload()
            .unwrap(),
        }),
        fields: Vec::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn sixteen_reservations_are_never_refunded_and_the_seventeenth_never_dispatches() {
    for failed in [false, true] {
        let step = if failed {
            Step::Fail(HttpError::Connect)
        } else {
            Step::Reply(None)
        };
        let script = Script::new(vec![step; 16]);
        let control = Control::new();
        let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control));
        for _ in 0..16 {
            if failed {
                assert_eq!(http.execute(get()).await.unwrap_err(), HttpError::Connect);
            } else {
                assert_eq!(http.execute(get()).await.unwrap().status, 200);
            }
        }
        assert_eq!(http.execute(get()).await.unwrap_err(), HttpError::Request);
        assert_eq!(control.requests.load(Ordering::Acquire), 16);
        assert_eq!(script.calls.load(Ordering::SeqCst), 16);
        assert_eq!(script.creates.load(Ordering::SeqCst), 0);
        assert!(!control.may_have_sent());
    }
}

#[tokio::test(start_paused = true)]
async fn the_sixty_second_deadline_is_not_renewed_between_gets() {
    let started = Instant::now();
    let control = Control::new();
    let script = Script::new(vec![Step::Reply(None), Step::Pending]);
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control));
    assert_eq!(control.deadline(), started + Duration::from_secs(60));
    assert_eq!(http.execute(get()).await.unwrap().status, 200);
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(http.execute(get()).await.unwrap_err(), HttpError::Timeout);
    assert_eq!(Instant::now(), started + Duration::from_secs(60));
    assert_eq!(control.deadline(), started + Duration::from_secs(60));
    assert_eq!(control.requests.load(Ordering::Acquire), 2);
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    assert!(!control.may_have_sent());
    assert_eq!(http.execute(get()).await.unwrap_err(), HttpError::Request);
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_handoff_keeps_every_request_unsent() {
    let control = Control::new();
    let script = Script::new(Vec::new());
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            Instant::now() + Duration::from_secs(90),
            200,
            Arc::new(FixedClock(100)),
        );
    assert!(!control.cancel());
    assert!(control.cancelled());
    assert_eq!(http.execute(get()).await.unwrap_err(), HttpError::Request);
    assert_eq!(http.execute(post()).await.unwrap_err(), HttpError::Request);
    assert!(!control.cancel());
    assert!(!control.may_have_sent());
    assert_eq!(control.requests.load(Ordering::Acquire), 0);
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    assert_eq!(script.creates.load(Ordering::SeqCst), 0);
}

struct CancellingClock(Arc<Control>);

impl Clock for CancellingClock {
    fn unix_seconds(&self) -> Option<u64> {
        assert!(!self.0.cancel());
        Some(100)
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_in_the_final_access_check_wins_the_atomic_handoff() {
    let control = Control::new();
    let script = Script::new(Vec::new());
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            Instant::now() + Duration::from_secs(90),
            200,
            Arc::new(CancellingClock(Arc::clone(&control))),
        );
    assert_eq!(http.execute(post()).await.unwrap_err(), HttpError::Request);
    assert!(control.cancelled());
    assert!(!control.may_have_sent());
    assert_eq!(control.requests.load(Ordering::Acquire), 1);
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    assert_eq!(script.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn absent_or_expired_access_never_hands_a_create_to_the_transport() {
    for (case, reservations, stopped) in [
        ("absent", 1, false),
        ("monotonic", 1, true),
        ("wall", 1, true),
        ("operation", 0, true),
    ] {
        let control = Control::new();
        let script = Script::new(Vec::new());
        let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control));
        let until = Instant::now() + Duration::from_secs(if case == "monotonic" { 0 } else { 90 });
        let http = if case == "absent" {
            http
        } else {
            http.with_post_access(
                until,
                if case == "wall" { 100 } else { 200 },
                Arc::new(FixedClock(100)),
            )
        };
        if case == "operation" {
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert_eq!(
            http.execute(post()).await.unwrap_err(),
            HttpError::Request,
            "{case}"
        );
        assert_eq!(control.cancelled(), stopped, "{case}");
        assert!(!control.may_have_sent());
        assert_eq!(control.requests.load(Ordering::Acquire), reservations);
        assert_eq!(script.calls.load(Ordering::SeqCst), 0);
        assert_eq!(script.creates.load(Ordering::SeqCst), 0);
    }
}

struct MovingClock(Arc<AtomicU64>);

impl Clock for MovingClock {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.0.load(Ordering::SeqCst))
    }
}

#[tokio::test(start_paused = true)]
async fn access_expiry_after_an_earlier_get_is_rechecked_before_handoff() {
    for monotonic in [false, true] {
        let control = Control::new();
        let script = Script::new(vec![Step::Reply(None)]);
        let wall = Arc::new(AtomicU64::new(100));
        let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
            .with_post_access(
                Instant::now() + Duration::from_secs(10),
                200,
                Arc::new(MovingClock(Arc::clone(&wall))),
            );
        assert_eq!(http.execute(get()).await.unwrap().status, 200);
        if monotonic {
            tokio::time::advance(Duration::from_secs(10)).await;
        } else {
            wall.store(200, Ordering::SeqCst);
        }
        assert_eq!(http.execute(post()).await.unwrap_err(), HttpError::Request);
        assert!(control.cancelled());
        assert!(!control.may_have_sent());
        assert_eq!(control.requests.load(Ordering::Acquire), 2);
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
        assert_eq!(script.creates.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_after_post_handoff_retains_uncertainty_at_the_original_deadline() {
    let started = Instant::now();
    let control = Control::new();
    let script = Script::new(vec![Step::Pending]);
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            started + Duration::from_secs(90),
            200,
            Arc::new(FixedClock(100)),
        );
    tokio::time::advance(Duration::from_secs(40)).await;
    assert_eq!(http.execute(post()).await.unwrap_err(), HttpError::Timeout);
    assert_eq!(Instant::now(), started + Duration::from_secs(60));
    assert!(control.may_have_sent());
    assert!(control.cancel());
    assert_eq!(control.requests.load(Ordering::Acquire), 1);
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.creates.load(Ordering::SeqCst), 1);
    assert_eq!(http.execute(post()).await.unwrap_err(), HttpError::Request);
    assert_eq!(script.creates.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_wakes_pending_gets_and_posts_with_their_actual_send_certainty() {
    for (post_request, expected, creates) in [
        (false, HttpError::Request, 0),
        (true, HttpError::Timeout, 1),
    ] {
        let started = Instant::now();
        let control = Control::new();
        let script = Script::new(vec![Step::Pending]);
        let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
            .with_post_access(
                started + Duration::from_secs(90),
                200,
                Arc::new(FixedClock(100)),
            );
        let request = if post_request { post() } else { get() };
        let response = http.execute(request);
        tokio::pin!(response);
        poll_fn(|cx| {
            assert!(response.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.cancel(), post_request);
        let result = tokio::time::timeout(Duration::from_secs(1), response)
            .await
            .expect("cancel wakes the pending transport");
        assert_eq!(result.unwrap_err(), expected);
        assert_eq!(Instant::now(), started);
        assert_eq!(control.may_have_sent(), post_request);
        assert_eq!(script.creates.load(Ordering::SeqCst), creates);
    }
}

#[tokio::test(start_paused = true)]
async fn another_access_window_cannot_dispatch_a_second_post() {
    let control = Control::new();
    let script = Script::new(vec![Step::Reply(None)]);
    let first = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            Instant::now() + Duration::from_secs(90),
            200,
            Arc::new(FixedClock(100)),
        );
    assert_eq!(first.execute(post()).await.unwrap().status, 200);
    let second = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            Instant::now() + Duration::from_secs(90),
            200,
            Arc::new(FixedClock(100)),
        );
    assert_eq!(
        second.execute(post()).await.unwrap_err(),
        HttpError::Request
    );
    assert_eq!(control.requests.load(Ordering::Acquire), 2);
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.creates.load(Ordering::SeqCst), 1);
    assert!(control.may_have_sent());
    assert!(!control.cancelled());
    assert!(control.cancel());
}

#[tokio::test(start_paused = true)]
async fn only_the_first_positive_completed_get_date_is_retained() {
    let control = Control::new();
    let script = Script::new(vec![
        Step::Reply(Some(9)),
        Step::Reply(None),
        Step::Reply(Some(0)),
        Step::Reply(Some(42)),
        Step::Reply(Some(84)),
    ]);
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control))
        .with_post_access(
            Instant::now() + Duration::from_secs(90),
            200,
            Arc::new(FixedClock(100)),
        );
    assert_eq!(http.execute(post()).await.unwrap().status, 200);
    assert_eq!(control.server_date_s(), None);
    for expected in [None, None, Some(42), Some(42)] {
        assert_eq!(http.execute(get()).await.unwrap().status, 200);
        assert_eq!(control.server_date_s(), expected);
    }
    assert_eq!(script.calls.load(Ordering::SeqCst), 5);
    assert_eq!(script.creates.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn only_the_four_typed_github_gets_and_create_can_reach_the_transport() {
    let control = Control::new();
    let script = Script::new(vec![Step::Reply(None); 4]);
    let http = BoundedHttp::new(SharedHttp::new(script.clone()), Arc::clone(&control));
    for endpoint in [
        GitHubEndpoint::User { token: token() },
        GitHubEndpoint::Installations {
            token: token(),
            page: Page::new(1).unwrap(),
        },
        GitHubEndpoint::Repositories {
            token: token(),
            installation: 1,
            page: Page::new(1).unwrap(),
        },
        GitHubEndpoint::Issues {
            token: token(),
            repo: RepoName::new("owner/repo").unwrap(),
            creator: GitHubLogin::new("owner").unwrap(),
            since_s: 1,
            page: Page::new(1).unwrap(),
        },
    ] {
        assert_eq!(
            http.execute(Request {
                endpoint: Endpoint::GitHub(endpoint),
                fields: Vec::new()
            })
            .await
            .unwrap()
            .status,
            200
        );
    }
    for endpoint in [
        Endpoint::Discovery,
        Endpoint::Token,
        Endpoint::GitHub(GitHubEndpoint::DeviceCode {
            client_id: ClientId::new("offline-client").unwrap(),
        }),
    ] {
        assert_eq!(
            http.execute(Request {
                endpoint,
                fields: Vec::new()
            })
            .await
            .unwrap_err(),
            HttpError::Request
        );
    }
    assert_eq!(control.requests.load(Ordering::Acquire), 4);
    assert_eq!(script.calls.load(Ordering::SeqCst), 4);
    assert_eq!(script.creates.load(Ordering::SeqCst), 0);
    assert!(!control.may_have_sent());
}

#[tokio::test(start_paused = true)]
async fn outer_debug_never_exposes_request_response_or_access_content() {
    let control = Control::new();
    let script = Script::new(vec![Step::Reply(Some(42))]);
    let http = BoundedHttp::new(SharedHttp::new(script), Arc::clone(&control)).with_post_access(
        Instant::now() + Duration::from_secs(90),
        200,
        Arc::new(FixedClock(100)),
    );
    let request = post();
    let request_debug = format!("{request:?}");
    let response = http.execute(request).await.unwrap();
    let debug = format!("{http:?} {control:?} {request_debug} {response:?}");
    let bytes = format!("{:?}", SENTINEL.as_bytes());
    for marker in [
        SENTINEL,
        bytes.as_str(),
        bytes.trim_start_matches('[').trim_end_matches(']'),
    ] {
        assert!(!debug.contains(marker));
    }
}
