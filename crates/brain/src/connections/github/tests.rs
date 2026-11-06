use super::{ClientId, DeviceCodeValue, GitHubEndpoint, GitHubToken, Page};
use crate::connections::http_client::{Endpoint, HttpError, Request, Response, ResponseMeta};
use crate::test_support::SENTINEL;
use kumochi_protocol::Secret;
use kumochi_protocol::message::{GitHubLogin, RepoName};
use zeroize::Zeroizing;

pub(super) fn token() -> Secret<GitHubToken> {
    Secret::new(GitHubToken::new(Zeroizing::new(format!("ghu_{SENTINEL}"))).unwrap())
}

pub(crate) fn endpoints() -> Vec<GitHubEndpoint> {
    vec![
        GitHubEndpoint::DeviceCode {
            client_id: ClientId::new(SENTINEL).unwrap(),
        },
        GitHubEndpoint::AccessToken {
            client_id: ClientId::new(SENTINEL).unwrap(),
            code: Secret::new(DeviceCodeValue::new(Zeroizing::new(SENTINEL.to_owned())).unwrap()),
        },
        GitHubEndpoint::User { token: token() },
        GitHubEndpoint::Installations {
            token: token(),
            page: Page::new(2).unwrap(),
        },
        GitHubEndpoint::Repositories {
            token: token(),
            installation: 42,
            page: Page::new(1).unwrap(),
        },
        GitHubEndpoint::CreateIssue {
            token: token(),
            repo: RepoName::new("owner/repo").unwrap(),
            body: Secret::new(Zeroizing::new(
                format!(r#"{{"title":"{SENTINEL}","body":""}}"#).into_bytes(),
            )),
        },
        GitHubEndpoint::Issues {
            token: token(),
            repo: RepoName::new("owner/repo").unwrap(),
            creator: GitHubLogin::new("owner").unwrap(),
            since_s: 1_709_251_199,
            page: Page::new(2).unwrap(),
        },
    ]
}

#[test]
fn inputs_enforce_each_local_bound_without_normalizing() {
    assert_eq!(
        ClientId::new(&"x".repeat(255)).unwrap().as_str(),
        "x".repeat(255)
    );
    assert_eq!(
        DeviceCodeValue::new(Zeroizing::new(" ".repeat(255)))
            .unwrap()
            .as_str(),
        " ".repeat(255)
    );
    assert_eq!(
        GitHubToken::new(Zeroizing::new(format!("ghu_{}", "x".repeat(251))))
            .unwrap()
            .as_str(),
        format!("ghu_{}", "x".repeat(251))
    );
    for value in [
        "",
        "x\n",
        "x\r",
        "x\t",
        "x\0",
        "x\u{7f}",
        "é",
        &"x".repeat(256),
    ] {
        assert_eq!(ClientId::new(value).unwrap_err(), HttpError::Request);
        assert_eq!(
            DeviceCodeValue::new(Zeroizing::new(value.to_owned())).unwrap_err(),
            HttpError::Request
        );
    }
    for value in [" x", "x ", "x\u{7f}"] {
        assert_eq!(ClientId::new(value).unwrap_err(), HttpError::Request);
    }
    for value in [
        "",
        "ghu_",
        "gho_fake",
        "ghu_ ",
        "ghu_x\n",
        "ghu_x\u{7f}",
        "ghu_é",
        &format!("ghu_{}", "x".repeat(252)),
    ] {
        assert_eq!(
            GitHubToken::new(Zeroizing::new(value.to_owned())).unwrap_err(),
            HttpError::Request
        );
    }
    assert_eq!(Page::new(1), Ok(Page(1)));
    assert_eq!(Page::new(2), Ok(Page(2)));
    assert_eq!(Page::new(0), Err(HttpError::Request));
    assert_eq!(Page::new(3), Err(HttpError::Request));
    assert_eq!(Page::new(u8::MAX), Err(HttpError::Request));
}

#[test]
fn unrepresentable_since_and_zero_installation_are_refused() {
    let mut issues = endpoints().pop().unwrap();
    let GitHubEndpoint::Issues { since_s, .. } = &mut issues else {
        panic!("issue fixture")
    };
    *since_s = u64::MAX;
    assert_eq!(issues.url().unwrap_err(), HttpError::Request);
    let GitHubEndpoint::Issues { since_s, .. } = &mut issues else {
        panic!("issue fixture")
    };
    *since_s = 0;
    assert_eq!(
        issues
            .url()
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "since")
            .unwrap()
            .1,
        "1970-01-01T00:00:00Z"
    );
    let GitHubEndpoint::Issues { since_s, .. } = &mut issues else {
        panic!("issue fixture")
    };
    *since_s = 253_402_300_799;
    assert_eq!(
        issues
            .url()
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "since")
            .unwrap()
            .1,
        "9999-12-31T23:59:59Z"
    );
    let GitHubEndpoint::Issues { since_s, .. } = &mut issues else {
        panic!("issue fixture")
    };
    *since_s = 253_402_300_800;
    assert_eq!(issues.url().unwrap_err(), HttpError::Request);
    assert_eq!(
        GitHubEndpoint::Repositories {
            token: token(),
            installation: 0,
            page: Page(1)
        }
        .url()
        .unwrap_err(),
        HttpError::Request
    );
}

#[test]
fn origins_are_rechecked_independently_of_identifier_grammars() {
    for host in ["github.com", "api.github.com"] {
        let good = format!("https://{host}/a?b=c");
        assert_eq!(
            super::check_origin(url::Url::parse(&good).unwrap(), host, "/a")
                .unwrap()
                .as_str(),
            good
        );
        for bad in [
            format!("http://{host}/"),
            format!("http://{host}:443/"),
            format!("https://{host}:444/"),
            format!("https://{host}.example/"),
            format!("https://user@{host}/"),
            format!("https://user:pass@{host}/"),
            format!("https://:pass@{host}/"),
            format!("https://{host}/#fragment"),
            "https://127.0.0.1/".to_owned(),
        ] {
            assert_eq!(
                super::check_origin(url::Url::parse(&bad).unwrap(), host, "/").unwrap_err(),
                HttpError::Endpoint
            );
        }
    }
    assert_eq!(
        super::check_origin(
            url::Url::parse("https://api.github.com/").unwrap(),
            "github.com",
            "/"
        )
        .unwrap_err(),
        HttpError::Endpoint
    );
    assert_eq!(
        super::check_origin(
            url::Url::parse("https://github.com/").unwrap(),
            "api.github.com",
            "/"
        )
        .unwrap_err(),
        HttpError::Endpoint
    );
    for (url, path) in [
        (
            "https://api.github.com/repos/a/b/issues",
            "/repos/a/c/issues",
        ),
        (
            "https://api.github.com/repos/a/../b/issues",
            "/repos/a/../b/issues",
        ),
    ] {
        assert_eq!(
            super::check_origin(url::Url::parse(url).unwrap(), "api.github.com", path).unwrap_err(),
            HttpError::Endpoint
        );
    }
}

#[test]
fn outermost_carriers_redact_credentials_content_form_names_and_request_id() {
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    for endpoint in endpoints() {
        let request = Request {
            endpoint: Endpoint::GitHub(endpoint),
            fields: vec![(SENTINEL, Secret::new(Zeroizing::new(SENTINEL.to_owned())))],
        };
        let text = format!("{request:?}");
        assert!(!text.contains(SENTINEL));
        assert!(!text.contains(&byte_list));
        assert!(!text.contains(byte_list.trim_start_matches('[').trim_end_matches(']')));
    }
    for endpoint in [
        Endpoint::Token,
        Endpoint::Discovery,
        Endpoint::Models(Secret::new(Zeroizing::new(SENTINEL.to_owned()))),
        Endpoint::Jwks(
            crate::connections::http_client::IssuerUrl::parse(&format!(
                "https://auth.openai.com/{SENTINEL}"
            ))
            .unwrap(),
        ),
        Endpoint::Revocation(
            crate::connections::http_client::IssuerUrl::parse(&format!(
                "https://auth.openai.com/{SENTINEL}"
            ))
            .unwrap(),
        ),
    ] {
        let request = Request {
            endpoint,
            fields: vec![(SENTINEL, Secret::new(Zeroizing::new(SENTINEL.to_owned())))],
        };
        let text = format!("{request:?}");
        assert!(!text.contains(SENTINEL));
        assert!(!text.contains(&byte_list));
    }
    let response = Response {
        status: 201,
        request_id: Some(SENTINEL.to_owned()),
        meta: ResponseMeta::default(),
        body: Secret::new(Zeroizing::new(SENTINEL.as_bytes().to_vec())),
    };
    let text = format!("{response:?}");
    assert!(!text.contains(SENTINEL));
    assert!(!text.contains(&byte_list));
    assert!(!text.contains(byte_list.trim_start_matches('[').trim_end_matches(']')));
    for text in [
        format!("{:?}", token().expose()),
        format!(
            "{:?}",
            DeviceCodeValue::new(Zeroizing::new(SENTINEL.to_owned())).unwrap()
        ),
    ] {
        assert!(!text.contains(SENTINEL));
        assert!(!text.contains(&byte_list));
    }
}
