use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
};

use kumochi_protocol::message::RepoName;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{ApiError, Duration, GitHubToken, Http, HttpError, Secret, fetch};
use crate::connections::http_client::{Request, Response, ResponseMeta};
use crate::test_support::SENTINEL;

struct Step {
    reply: Result<Response, HttpError>,
    delay: Duration,
}

struct Script {
    replies: Mutex<VecDeque<Step>>,
    urls: Mutex<Vec<String>>,
    active: Arc<AtomicUsize>,
}

impl Script {
    fn new(values: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(
                values
                    .into_iter()
                    .map(|value| Step {
                        reply: Ok(Response {
                            status: 200,
                            request_id: None,
                            meta: ResponseMeta::default(),
                            body: Secret::new(Zeroizing::new(serde_json::to_vec(&value).unwrap())),
                        }),
                        delay: Duration::ZERO,
                    })
                    .collect(),
            ),
            urls: Mutex::new(Vec::new()),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Http for Script {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        self.urls
            .lock()
            .unwrap()
            .push(request.endpoint.url()?.into_owned());
        let step = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("one scripted reply per request");
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(Arc::clone(&self.active));
        if !step.delay.is_zero() {
            tokio::time::sleep(step.delay).await;
        }
        step.reply
    }
}

fn token() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new("ghu_fixture".to_owned())).unwrap())
}

fn installations(total: u64, ids: &[u64]) -> Value {
    json!({"total_count":total,"installations":ids.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()})
}

fn repositories(total: u64, names: &[&str]) -> Value {
    json!({"total_count":total,"repositories":names.iter().zip(1_u64..).map(|(name, id)| json!({"full_name":name,"id":id})).collect::<Vec<_>>()})
}

fn numbered(total: u64, range: std::ops::Range<usize>) -> Value {
    json!({"total_count":total,"repositories":range.map(|n| json!({"full_name":format!("owner/r{n:03}"),"id":n + 1})).collect::<Vec<_>>()})
}

#[tokio::test]
async fn multiple_intersections_are_complete_case_insensitive_and_keep_github_spelling() {
    let http = Script::new(vec![
        installations(2, &[11, 22]),
        repositories(2, &["Owner/Bravo", "owner/alpha"]),
        repositories(2, &["OWNER/BRAVO", "owner/charlie"]),
    ]);
    let result = fetch(&http, &token()).await.unwrap();
    assert_eq!(
        result
            .names
            .iter()
            .map(RepoName::as_str)
            .collect::<Vec<_>>(),
        ["owner/alpha", "Owner/Bravo", "owner/charlie"]
    );
    assert_eq!(
        *http.urls.lock().unwrap(),
        [
            "https://api.github.com/user/installations?per_page=100&page=1",
            "https://api.github.com/user/installations/11/repositories?per_page=100&page=1",
            "https://api.github.com/user/installations/22/repositories?per_page=100&page=1",
        ]
    );
    assert_eq!(http.replies.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn no_installations_or_no_repository_rows_is_a_complete_empty_snapshot() {
    for (values, requests) in [
        (vec![installations(0, &[])], 1),
        (vec![installations(1, &[1]), repositories(0, &[])], 2),
    ] {
        let http = Script::new(values);
        assert_eq!(fetch(&http, &token()).await.unwrap().names.len(), 0);
        assert_eq!(http.urls.lock().unwrap().len(), requests);
    }
}

#[tokio::test]
async fn two_hundred_repositories_are_complete_but_one_more_never_grants_partial_authority() {
    let http = Script::new(vec![
        installations(1, &[1]),
        numbered(200, 0..100),
        numbered(200, 100..200),
    ]);
    let result = fetch(&http, &token()).await.unwrap();
    assert_eq!(
        result
            .names
            .iter()
            .map(RepoName::as_str)
            .collect::<Vec<_>>(),
        (0..200)
            .map(|n| format!("owner/r{n:03}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(http.urls.lock().unwrap().len(), 3);
    assert_eq!(
        http.urls.lock().unwrap()[2],
        "https://api.github.com/user/installations/1/repositories?per_page=100&page=2"
    );
    for count in [100, 101] {
        let mut values = vec![installations(1, &[1]), numbered(count, 0..100)];
        if count == 101 {
            values.push(numbered(101, 100..101));
        }
        let http = Script::new(values);
        let result = fetch(&http, &token()).await.unwrap();
        assert_eq!(
            result
                .names
                .iter()
                .map(RepoName::as_str)
                .collect::<Vec<_>>(),
            (0..count)
                .map(|n| format!("owner/r{n:03}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(http.replies.lock().unwrap().len(), 0);
    }
    for values in [
        vec![installations(1, &[1]), numbered(201, 0..100)],
        vec![
            installations(2, &[1, 2]),
            numbered(100, 0..100),
            numbered(101, 100..200),
            numbered(101, 200..201),
        ],
    ] {
        let http = Script::new(values);
        assert_eq!(
            fetch(&http, &token()).await.unwrap_err(),
            ApiError::Response
        );
        assert_eq!(http.replies.lock().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn twelve_total_page_requests_succeed_and_a_thirteenth_is_never_dispatched() {
    for (count, succeeds) in [(11, true), (12, false)] {
        let ids: Vec<_> = (1..=count).collect();
        let mut values = vec![installations(count, &ids)];
        values.extend((0..count).map(|_| repositories(0, &[])));
        let http = Script::new(values);
        let result = fetch(&http, &token()).await;
        if succeeds {
            assert_eq!(result.unwrap().names.len(), 0);
            assert_eq!(http.replies.lock().unwrap().len(), 0);
        } else {
            assert_eq!(result.unwrap_err(), ApiError::Response);
            assert_eq!(http.replies.lock().unwrap().len(), 1);
        }
        assert_eq!(http.urls.lock().unwrap().len(), 12);
    }
}

#[tokio::test]
async fn installation_pages_refuse_missing_extra_overflowing_and_duplicate_rows() {
    for value in [
        installations(1, &[]),
        installations(0, &[1]),
        installations(101, &[1]),
        installations(201, &[]),
        installations(u64::MAX, &[]),
        installations(2, &[1, 1]),
    ] {
        let http = Script::new(vec![value]);
        assert_eq!(
            fetch(&http, &token()).await.unwrap_err(),
            ApiError::Response
        );
        assert_eq!(http.urls.lock().unwrap().len(), 1);
    }
    let ids: Vec<_> = (1..=100).collect();
    for second in [
        installations(102, &[101]),
        installations(101, &[]),
        installations(101, &[1]),
    ] {
        let http = Script::new(vec![installations(101, &ids), second]);
        assert_eq!(
            fetch(&http, &token()).await.unwrap_err(),
            ApiError::Response
        );
        assert_eq!(http.urls.lock().unwrap().len(), 2);
        assert_eq!(
            http.urls.lock().unwrap()[1],
            "https://api.github.com/user/installations?per_page=100&page=2"
        );
    }
}

#[tokio::test]
async fn repository_pages_refuse_truncation_changed_totals_and_duplicate_names() {
    for value in [
        repositories(1, &[]),
        repositories(0, &["owner/extra"]),
        repositories(101, &["owner/short"]),
        repositories(u64::MAX, &[]),
        repositories(2, &["owner/repo", "OWNER/REPO"]),
    ] {
        let http = Script::new(vec![installations(1, &[1]), value]);
        assert_eq!(
            fetch(&http, &token()).await.unwrap_err(),
            ApiError::Response
        );
        assert_eq!(http.urls.lock().unwrap().len(), 2);
    }
    for second in [
        numbered(102, 100..101),
        repositories(101, &[]),
        repositories(101, &["OWNER/R000"]),
        repositories(101, &["owner/a", "owner/b"]),
    ] {
        let http = Script::new(vec![installations(1, &[1]), numbered(101, 0..100), second]);
        assert_eq!(
            fetch(&http, &token()).await.unwrap_err(),
            ApiError::Response
        );
        assert_eq!(http.urls.lock().unwrap().len(), 3);
        assert_eq!(
            http.urls.lock().unwrap()[2],
            "https://api.github.com/user/installations/1/repositories?per_page=100&page=2"
        );
    }
}

#[tokio::test]
async fn errors_and_malformed_later_replies_never_return_an_earlier_partial_snapshot() {
    for reply in [
        Err(HttpError::Transport),
        Ok(Response {
            status: 403,
            request_id: None,
            meta: ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(SENTINEL.as_bytes().to_vec())),
        }),
        Ok(Response {
            status: 200,
            request_id: None,
            meta: ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(b"{".to_vec())),
        }),
    ] {
        let expected = match &reply {
            Err(kind) => ApiError::Http(*kind),
            Ok(value) if value.status == 403 => ApiError::Refused {
                status: 403,
                meta: ResponseMeta::default(),
            },
            Ok(_) => ApiError::Response,
        };
        let http = Script::new(vec![
            installations(2, &[1, 2]),
            repositories(1, &["owner/partial"]),
        ]);
        http.replies.lock().unwrap().push_back(Step {
            reply,
            delay: Duration::ZERO,
        });
        let error = fetch(&http, &token()).await.unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains(SENTINEL));
        assert_eq!(http.urls.lock().unwrap().len(), 3);
    }
}

async fn pending_once<T>(mut future: std::pin::Pin<&mut impl std::future::Future<Output = T>>) {
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}

#[tokio::test(start_paused = true)]
async fn one_shared_deadline_stops_later_installations_and_drops_the_inflight_request() {
    let mut values = vec![installations(6, &[1, 2, 3, 4, 5, 6])];
    values.extend((0..6).map(|_| repositories(0, &[])));
    let http = Script::new(values);
    for step in http.replies.lock().unwrap().iter_mut() {
        step.delay = Duration::from_secs(10);
    }
    let token = token();
    let future = fetch(&http, &token);
    tokio::pin!(future);
    pending_once(future.as_mut()).await;
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(10)).await;
        pending_once(future.as_mut()).await;
    }
    assert_eq!(http.urls.lock().unwrap().len(), 6);
    assert_eq!(http.active.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(9)).await;
    pending_once(future.as_mut()).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        future.await.unwrap_err(),
        ApiError::Http(HttpError::Timeout)
    );
    assert_eq!(http.urls.lock().unwrap().len(), 6);
    assert_eq!(http.replies.lock().unwrap().len(), 1);
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_partial_traversal_cancels_the_request_without_detached_work() {
    let http = Script::new(vec![
        installations(2, &[1, 2]),
        repositories(1, &["owner/partial"]),
        repositories(0, &[]),
    ]);
    http.replies.lock().unwrap().back_mut().unwrap().delay = Duration::from_secs(10);
    let token = token();
    {
        let future = fetch(&http, &token);
        tokio::pin!(future);
        pending_once(future.as_mut()).await;
        assert_eq!(http.urls.lock().unwrap().len(), 3);
        assert_eq!(http.active.load(Ordering::SeqCst), 1);
    }
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(http.urls.lock().unwrap().len(), 3);
    assert_eq!(http.active.load(Ordering::SeqCst), 0);
}
