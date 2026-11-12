use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::connections::github::{GitHubEndpoint, Page};
use crate::connections::http_client::{
    Endpoint, Http, HttpError, Request, Response, ResponseMeta, SharedHttp,
};
use crate::test_support::SENTINEL;
use kumochi_protocol::message::{
    ApprovalRev, DraftId, GitHubLogin, IssueBody, IssueTitle, LookupResult, StateVersion, ThreadId,
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Debug)]
enum Expected {
    User,
    Installations,
    Repositories,
    Issues { since: u64, page: u8 },
}

struct Step {
    endpoint: Expected,
    status: u16,
    date: Option<u64>,
    body: Value,
}

struct Script {
    steps: Mutex<Vec<Step>>,
    calls: AtomicUsize,
}

impl Http for Arc<Script> {
    fn execute(&self, request: Request) -> impl Future<Output = Result<Response, HttpError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.fields.is_empty());
        let step = {
            let mut steps = self.steps.lock().unwrap();
            let at = steps
                .iter()
                .position(|step| match (&step.endpoint, &request.endpoint) {
                    (Expected::User, Endpoint::GitHub(GitHubEndpoint::User { .. })) => true,
                    (
                        Expected::Installations,
                        Endpoint::GitHub(GitHubEndpoint::Installations { page, .. }),
                    ) => *page == Page::new(1).unwrap(),
                    (
                        Expected::Repositories,
                        Endpoint::GitHub(GitHubEndpoint::Repositories {
                            installation, page, ..
                        }),
                    ) => *installation == 9 && *page == Page::new(1).unwrap(),
                    (
                        Expected::Issues {
                            since,
                            page: expected,
                        },
                        Endpoint::GitHub(GitHubEndpoint::Issues {
                            repo,
                            creator,
                            since_s,
                            page,
                            ..
                        }),
                    ) => {
                        repo.as_str() == "owner/repo"
                            && creator.as_str() == "verified-owner"
                            && since_s == since
                            && *page == Page::new(*expected).unwrap()
                    }
                    _ => false,
                })
                .expect("one scripted response for this typed endpoint and page");
            steps.remove(at)
        };
        std::future::ready(Ok(Response {
            status: step.status,
            request_id: Some(SENTINEL.to_owned()),
            meta: ResponseMeta {
                date_s: step.date,
                ..ResponseMeta::default()
            },
            body: Secret::new(Zeroizing::new(serde_json::to_vec(&step.body).unwrap())),
        }))
    }
}

fn reply(endpoint: Expected, date: Option<u64>, body: Value) -> Step {
    Step {
        endpoint,
        status: 200,
        date,
        body,
    }
}

fn authority(date: Option<u64>) -> Vec<Step> {
    vec![
        reply(
            Expected::User,
            date,
            json!({"id":7,"login":"verified-owner"}),
        ),
        reply(
            Expected::Installations,
            None,
            json!({"total_count":1,"installations":[{"id":9}]}),
        ),
        reply(
            Expected::Repositories,
            None,
            json!({"total_count":2,"repositories":[
                {"full_name":"owner/repo"},{"full_name":"owner/other"}
            ]}),
        ),
    ]
}

fn bounded(steps: Vec<Step>) -> (BoundedHttp, Arc<Control>, Arc<Script>) {
    let script = Arc::new(Script {
        steps: Mutex::new(steps),
        calls: AtomicUsize::new(0),
    });
    let control = Control::new();
    let http = BoundedHttp::new(SharedHttp::new(Arc::clone(&script)), Arc::clone(&control));
    (http, control, script)
}

fn token() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new(format!("ghu_{SENTINEL}"))).unwrap())
}

fn action() -> Action {
    Action {
        draft: DraftId(1),
        thread: ThreadId(2),
        version: StateVersion(2),
        stage: Stage::Preflight { unresolved: None },
        repo: Some(RepoName::new("owner/repo").unwrap()),
        account: Some(GitHubUser {
            id: 7,
            login: GitHubLogin::new("old-owner").unwrap(),
        }),
        title: IssueTitle::new("  Local café  ").unwrap(),
        body: IssueBody::new("first\r\n\tcafé\r\n").unwrap(),
        approval: None,
        consumed: Some(ApprovalRev(1)),
        uncertain: 0,
    }
}

fn unknown() -> Action {
    Action {
        stage: Stage::Unknown {
            mark: Mark {
                at_s: NonZeroU64::new(120).unwrap(),
                presend: Bounded::new(vec![NonZeroU64::new(3).unwrap()]).unwrap(),
            },
            lookup: LookupResult::Pending,
        },
        version: StateVersion(4),
        approval: Some(ApprovalRev(2)),
        uncertain: 1,
        ..action()
    }
}

fn row(number: u64, created_at: &str) -> Value {
    json!({"number":number,"title":"  Local café  ","body":"first\n\tcafé\n","created_at":created_at})
}

#[tokio::test(start_paused = true)]
async fn scripted_authority_responses_are_independent_of_request_order() {
    let (http, control, script) = bounded(authority(Some(100)));
    let token = token();
    let installations = api::installations(&http, &token, Page::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(installations.installations, [9]);
    assert_eq!(installations.total_count, 1);
    assert_eq!(control.server_date_s(), None);
    let user = api::user(&http, &token).await.unwrap();
    assert_eq!(user.id, 7);
    assert_eq!(user.login.as_str(), "verified-owner");
    assert_eq!(control.server_date_s(), Some(100));
    let repositories = api::repositories(&http, &token, 9, Page::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(repositories.total_count, 2);
    assert_eq!(
        repositories
            .repositories
            .iter()
            .map(RepoName::as_str)
            .collect::<Vec<_>>(),
        ["owner/repo", "owner/other"]
    );
    assert_eq!(control.server_date_s(), Some(100));
    assert_eq!(script.calls.load(Ordering::SeqCst), 3);
    assert!(script.steps.lock().unwrap().is_empty());
    assert!(!control.may_have_sent());
}

#[tokio::test(start_paused = true)]
async fn preflight_uses_server_dates_fresh_identity_and_exact_original_content() {
    let mut steps = authority(Some(100));
    let mut wrong_title = row(5, "1970-01-01T00:01:50Z");
    wrong_title["title"] = json!("Local café");
    let mut raw_crlf = row(8, "1970-01-01T00:01:50Z");
    raw_crlf["body"] = json!("first\r\n\tcafé\r\n");
    steps.push(reply(
        Expected::Issues { since: 99, page: 1 },
        Some(120),
        json!([
            row(9, "1970-01-01T00:01:50Z"),
            wrong_title,
            row(3, "1970-01-01T00:01:40Z"),
            raw_crlf,
        ]),
    ));
    let (http, control, script) = bounded(steps);
    let action = action();
    assert_eq!(action.body.as_str(), "first\n\tcafé\n");
    let prepared = preflight(http, token(), &action, &control).await.unwrap();
    assert_eq!(prepared.user.id, 7);
    assert_eq!(prepared.user.login.as_str(), "verified-owner");
    assert_eq!(
        prepared
            .repos
            .names
            .iter()
            .map(RepoName::as_str)
            .collect::<Vec<_>>(),
        ["owner/other", "owner/repo"]
    );
    assert_eq!(prepared.mark.at_s.get(), 120);
    assert_eq!(
        prepared
            .mark
            .presend
            .iter()
            .map(|number| number.get())
            .collect::<Vec<_>>(),
        [9, 3]
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 4);
    assert!(script.steps.lock().unwrap().is_empty());
    let body = IssueContent {
        title: &action.title,
        body: &action.body,
    }
    .into_payload()
    .unwrap();
    let post = Request {
        endpoint: Endpoint::GitHub(GitHubEndpoint::CreateIssue {
            token: token(),
            repo: RepoName::new("owner/repo").unwrap(),
            body,
        }),
        fields: Vec::new(),
    };
    assert_eq!(
        prepared.http.execute(post).await.unwrap_err(),
        HttpError::Request
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 4);
    assert!(!control.may_have_sent());
}

#[tokio::test(start_paused = true)]
async fn missing_or_backwards_preflight_dates_never_produce_a_mark() {
    for (anchor, date, calls) in [
        (None, Some(120), 3),
        (Some(0), Some(120), 3),
        (Some(100), None, 4),
        (Some(100), Some(0), 4),
        (Some(100), Some(99), 4),
    ] {
        let mut steps = authority(anchor);
        if calls == 4 {
            steps.push(reply(
                Expected::Issues { since: 99, page: 1 },
                date,
                json!([]),
            ));
        }
        let (http, control, script) = bounded(steps);
        let failure = preflight(http, token(), &action(), &control)
            .await
            .unwrap_err();
        assert_eq!(
            failure,
            Failure {
                reason: FailReason::PreflightFailed,
                unauthorized: false
            }
        );
        assert_eq!(script.calls.load(Ordering::SeqCst), calls);
        assert!(!control.may_have_sent());
    }
}

#[tokio::test(start_paused = true)]
async fn an_anchor_from_the_repository_get_is_used_without_local_time() {
    let mut steps = authority(None);
    steps[2].date = Some(1);
    steps.push(reply(
        Expected::Issues { since: 0, page: 1 },
        Some(2),
        json!([]),
    ));
    let (http, control, script) = bounded(steps);
    let prepared = preflight(http, token(), &action(), &control).await.unwrap();
    assert_eq!(prepared.mark.at_s.get(), 2);
    assert_eq!(prepared.mark.presend.len(), 0);
    assert_eq!(script.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn identity_and_complete_exact_membership_are_required_without_retargeting() {
    for (case, reason, calls) in [
        ("unbound account", FailReason::NotConnected, 0),
        ("unbound repo", FailReason::NotFound, 0),
        ("numeric mismatch", FailReason::NotConnected, 1),
        ("unlisted", FailReason::NotFound, 3),
        ("case differs", FailReason::NotFound, 3),
        ("incomplete", FailReason::PreflightFailed, 3),
    ] {
        let mut action = action();
        let mut steps = authority(Some(100));
        match case {
            "unbound account" => action.account = None,
            "unbound repo" => action.repo = None,
            "numeric mismatch" => steps[0].body = json!({"id":8,"login":"verified-owner"}),
            "unlisted" => {
                steps[2].body =
                    json!({"total_count":1,"repositories":[{"full_name":"owner/other"}]});
            }
            "case differs" => {
                steps[2].body =
                    json!({"total_count":1,"repositories":[{"full_name":"Owner/repo"}]});
            }
            "incomplete" => {
                steps[2].body =
                    json!({"total_count":2,"repositories":[{"full_name":"owner/repo"}]});
            }
            _ => unreachable!(),
        }
        steps.truncate(calls);
        let (http, control, script) = bounded(steps);
        let failure = preflight(http, token(), &action, &control)
            .await
            .unwrap_err();
        assert_eq!(
            failure,
            Failure {
                reason,
                unauthorized: false
            },
            "{case}"
        );
        assert_eq!(script.calls.load(Ordering::SeqCst), calls);
        assert_eq!(
            action.repo.as_ref().map(RepoName::as_str),
            if case == "unbound repo" {
                None
            } else {
                Some("owner/repo")
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn only_an_actual_unauthorized_response_sets_the_owner_revocation_marker() {
    for (phase, status, reason) in [
        (0, 401, FailReason::NotConnected),
        (2, 401, FailReason::NotConnected),
        (3, 401, FailReason::NotConnected),
        (0, 403, FailReason::PermissionDenied),
        (0, 500, FailReason::PreflightFailed),
    ] {
        let mut steps = authority(Some(100));
        steps.push(reply(
            Expected::Issues { since: 99, page: 1 },
            Some(120),
            json!([]),
        ));
        steps[phase].status = status;
        steps.truncate(phase + 1);
        let (http, control, script) = bounded(steps);
        assert_eq!(
            preflight(http, token(), &action(), &control)
                .await
                .unwrap_err(),
            Failure {
                reason,
                unauthorized: status == 401
            }
        );
        assert_eq!(script.calls.load(Ordering::SeqCst), phase + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn check_reports_candidates_and_ambiguity_without_ever_returning_a_receipt() {
    let mut edited = row(77, "1970-01-01T00:02:01Z");
    edited["title"] = json!("remotely edited");
    for (case, rows, expected) in [
        (
            "unrelated identical candidate",
            json!([row(77, "1970-01-01T00:02:01Z")]),
            "one",
        ),
        (
            "presend excluded",
            json!([
                row(3, "1970-01-01T00:02:00Z"),
                row(77, "1970-01-01T00:02:01Z")
            ]),
            "one",
        ),
        (
            "only presend",
            json!([row(3, "1970-01-01T00:02:00Z")]),
            "none",
        ),
        (
            "older match",
            json!([row(44, "1970-01-01T00:01:59Z")]),
            "none",
        ),
        ("remote edit", json!([edited]), "none"),
        (
            "same second",
            json!([row(77, "1970-01-01T00:02:00Z")]),
            "failed",
        ),
        ("future", json!([row(77, "1970-01-01T00:02:11Z")]), "failed"),
        (
            "several",
            json!([
                row(77, "1970-01-01T00:02:01Z"),
                row(78, "1970-01-01T00:02:02Z")
            ]),
            "several",
        ),
        (
            "ambiguity after several",
            json!([
                row(77, "1970-01-01T00:02:01Z"),
                row(78, "1970-01-01T00:02:02Z"),
                row(79, "1970-01-01T00:02:11Z")
            ]),
            "failed",
        ),
    ] {
        let mut steps = authority(Some(100));
        steps.push(reply(
            Expected::Issues {
                since: 119,
                page: 1,
            },
            Some(130),
            rows,
        ));
        let (http, control, script) = bounded(steps);
        let action = unknown();
        let result = check(&http, &token(), &action).await.unwrap();
        match (expected, result) {
            ("one", Lookup::Unplaced(number)) => assert_eq!(number.get(), 77),
            ("none", Lookup::None) | ("failed", Lookup::Failed) | ("several", Lookup::Several) => {}
            _ => panic!("{case}: unexpected lookup outcome"),
        }
        assert!(matches!(action.stage, Stage::Unknown { .. }));
        assert_eq!(script.calls.load(Ordering::SeqCst), 4);
        assert!(!control.may_have_sent());
    }
}

#[tokio::test(start_paused = true)]
async fn check_requires_a_positive_page_one_date_not_before_the_stored_mark() {
    for date in [None, Some(0), Some(119)] {
        let mut steps = authority(Some(100));
        steps.push(reply(
            Expected::Issues {
                since: 119,
                page: 1,
            },
            date,
            json!([]),
        ));
        let (http, _, script) = bounded(steps);
        assert!(matches!(
            check(&http, &token(), &unknown()).await.unwrap(),
            Lookup::Failed
        ));
        assert_eq!(script.calls.load(Ordering::SeqCst), 4);
    }
}

#[tokio::test(start_paused = true)]
async fn a_full_second_page_is_inconclusive_for_both_preflight_and_check() {
    for recovering in [false, true] {
        let mut steps = authority(Some(100));
        let since = if recovering { 119 } else { 99 };
        for page in [1, 2] {
            let first = if page == 1 { 1 } else { 101 };
            let rows: Vec<Value> = (first..first + 100)
                .map(|number| row(number, "1970-01-01T00:02:01Z"))
                .collect();
            steps.push(reply(
                Expected::Issues { since, page },
                Some(130),
                json!(rows),
            ));
        }
        let (http, control, script) = bounded(steps);
        if recovering {
            assert!(matches!(
                check(&http, &token(), &unknown()).await.unwrap(),
                Lookup::Failed
            ));
        } else {
            assert_eq!(
                preflight(http, token(), &action(), &control)
                    .await
                    .unwrap_err(),
                Failure {
                    reason: FailReason::PreflightFailed,
                    unauthorized: false,
                }
            );
        }
        assert_eq!(script.calls.load(Ordering::SeqCst), 5);
        assert!(!control.may_have_sent());
    }
}

#[tokio::test(start_paused = true)]
async fn check_rejects_non_unknown_actions_and_marks_actual_401_without_guessing() {
    let (http, _, script) = bounded(Vec::new());
    assert_eq!(
        check(&http, &token(), &action()).await.unwrap_err(),
        Failure {
            reason: FailReason::PreflightFailed,
            unauthorized: false,
        }
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    for mismatch in [false, true] {
        let mut steps = authority(Some(100));
        if mismatch {
            steps[0].body = json!({"id":8,"login":"verified-owner"});
        } else {
            steps[0].status = 401;
        }
        steps.truncate(1);
        let (http, _, script) = bounded(steps);
        assert_eq!(
            check(&http, &token(), &unknown()).await.unwrap_err(),
            Failure {
                reason: FailReason::NotConnected,
                unauthorized: !mismatch,
            }
        );
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn prepared_and_failure_debug_never_expose_user_or_remote_content() {
    let mut steps = authority(Some(100));
    steps.push(reply(
        Expected::Issues { since: 99, page: 1 },
        Some(120),
        json!([]),
    ));
    let (http, control, _) = bounded(steps);
    let mut prepared = preflight(http, token(), &action(), &control).await.unwrap();
    prepared.user.login = GitHubLogin::new(SENTINEL).unwrap();
    prepared.repos.names = Bounded::new(vec![
        RepoName::new(&format!("{SENTINEL}/{SENTINEL}")).unwrap(),
    ])
    .unwrap();
    let debug = format!("{:?}", Ok::<_, Failure>(prepared));
    let bytes = format!("{:?}", SENTINEL.as_bytes());
    for marker in [
        SENTINEL,
        bytes.as_str(),
        bytes.trim_start_matches('[').trim_end_matches(']'),
    ] {
        assert!(!debug.contains(marker));
    }
    assert_eq!(
        format!("{INCONCLUSIVE:?}"),
        "Failure { reason: PreflightFailed, unauthorized: false }"
    );
}
