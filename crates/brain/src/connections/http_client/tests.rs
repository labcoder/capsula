use std::collections::BTreeMap;
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kumochi_protocol::Secret;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use reqwest::header::{HeaderMap, HeaderValue};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use zeroize::Zeroizing;

use super::{
    Client, Endpoint, HttpError, IssuerUrl, MAX_BODY_BYTES, Request, ResponseMeta,
    connecting_within, from_roots, read_response,
};
use crate::platform::{TrustRoots, TrustRootsError};
use crate::test_support::{Captured, SENTINEL};

struct Server {
    address: SocketAddr,
    root: Vec<u8>,
    config: Arc<rustls::ServerConfig>,
    task: Option<JoinHandle<io::Result<WireRequest>>>,
    flushed: Option<tokio::sync::oneshot::Receiver<()>>,
    release: std::sync::mpsc::Sender<()>,
}

struct WireRequest {
    headers: BTreeMap<String, String>,
    line: Zeroizing<String>,
    content_type: Option<String>,
    authorization: Option<String>,
    api_key: Option<String>,
    anthropic_version: Option<String>,
    body: Secret<Zeroizing<Vec<u8>>>,
}

impl Server {
    fn start(reply: Vec<u8>) -> Self {
        Self::start_delayed(reply, Duration::ZERO)
    }

    fn start_delayed(reply: Vec<u8>, delay: Duration) -> Self {
        static NEXT_CA: AtomicUsize = AtomicUsize::new(0);
        let key = KeyPair::generate().expect("in-memory CA key");
        let mut ca = CertificateParams::new(Vec::new()).expect("CA parameters");
        ca.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!(
                "account-http-test-ca-{}",
                NEXT_CA.fetch_add(1, Ordering::Relaxed)
            ),
        );
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let root = ca
            .self_signed(&key)
            .expect("public CA certificate")
            .der()
            .to_vec();
        let leaf_key = KeyPair::generate().expect("in-memory leaf key");
        let leaf = CertificateParams::new(vec!["127.0.0.1".into()])
            .expect("IP SAN")
            .signed_by(&leaf_key, &Issuer::from_params(&ca, &key))
            .expect("CA-signed leaf");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring protocols")
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .expect("test TLS key");
        Self::with_config(reply, root, Arc::new(config), delay)
    }

    fn with_config(
        reply: Vec<u8>,
        root: Vec<u8>,
        config: Arc<rustls::ServerConfig>,
        delay: Duration,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback port 0");
        let address = listener.local_addr().expect("bound address");
        let worker_config = Arc::clone(&config);
        let (flushed, received) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let task = thread::spawn(move || {
            let (socket, _) = listener.accept()?;
            socket.set_read_timeout(Some(Duration::from_secs(5)))?;
            socket.set_write_timeout(Some(Duration::from_secs(5)))?;
            let connection =
                rustls::ServerConnection::new(worker_config).map_err(io::Error::other)?;
            let mut tls = rustls::StreamOwned::new(connection, socket);
            let mut header = Zeroizing::new(Vec::with_capacity(8192));
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                tls.read_exact(&mut byte)?;
                header.push(byte[0]);
                if header.len() > 8192 {
                    return Err(io::Error::other("fixture header bound"));
                }
            }
            let header = std::str::from_utf8(&header).map_err(io::Error::other)?;
            let length = header
                .split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(Ok(0), |(_, value)| {
                    value.trim().parse::<usize>().map_err(io::Error::other)
                })?;
            if length > 4096 {
                return Err(io::Error::other("fixture form bound"));
            }
            let mut body = Zeroizing::new(vec![0; length]);
            tls.read_exact(&mut body)?;
            let wire = WireRequest {
                headers: header
                    .split("\r\n")
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect(),
                line: Zeroizing::new(
                    header
                        .lines()
                        .next()
                        .ok_or_else(|| io::Error::other("missing request line"))?
                        .to_owned(),
                ),
                content_type: header
                    .split("\r\n")
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.trim().to_owned()),
                authorization: header
                    .split("\r\n")
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_owned()),
                api_key: header
                    .split("\r\n")
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
                    .map(|(_, value)| value.trim().to_owned()),
                anthropic_version: header
                    .split("\r\n")
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
                    .map(|(_, value)| value.trim().to_owned()),
                body: Secret::new(body),
            };
            tls.write_all(&reply)?;
            if delay.is_zero() {
                tls.conn.send_close_notify();
            }
            tls.flush()?;
            flushed.send(()).unwrap_or(());
            if !delay.is_zero() {
                let _release = held.recv_timeout(delay);
            }
            Ok(wire)
        });
        Self {
            address,
            root,
            config,
            task: Some(task),
            flushed: Some(received),
            release,
        }
    }

    fn url(&self) -> String {
        format!("https://{}/fixture", self.address)
    }

    fn client(&self) -> Client {
        Client(
            from_roots(Ok(TrustRoots {
                certificates: vec![self.root.clone()],
                skipped: 0,
                unreadable: 0,
            }))
            .expect("supplied test CA"),
        )
    }

    fn finish(mut self) -> io::Result<WireRequest> {
        self.release.send(()).unwrap_or(());
        self.task
            .take()
            .expect("fixture task")
            .join()
            .map_err(|_| io::Error::other("TLS fixture panicked"))?
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.release.send(()).unwrap_or(());
            if !task.is_finished() {
                drop(TcpStream::connect_timeout(
                    &self.address,
                    Duration::from_secs(1),
                ));
            }
            if task.join().is_err() {
                tracing::warn!("TLS fixture thread panicked");
            }
        }
    }
}

fn reply(status: &str, body: &[u8], headers: &str) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn request(endpoint: Endpoint) -> Request {
    Request {
        endpoint,
        fields: Vec::new(),
    }
}

#[tokio::test]
async fn github_wire_policy_uses_typed_payloads_and_never_logs_them() {
    use crate::connections::github::tests::endpoints;
    let captured = Captured::default();
    let _logging = captured.install();
    tracing::info!("github-wire-control");
    for (index, endpoint) in endpoints().into_iter().enumerate() {
        let server = Server::start(reply(
            "200 OK",
            b"{}",
            "Date: Thu, 29 Feb 2024 23:59:59 GMT\r\n",
        ));
        let original = endpoint.url().unwrap();
        let mut target = original.clone();
        target.set_host(Some("127.0.0.1")).unwrap();
        target.set_port(Some(server.address.port())).unwrap();
        let response = server
            .client()
            .send_to(target.as_str(), request(Endpoint::GitHub(endpoint)))
            .await
            .unwrap();
        assert_eq!(response.meta.date_s, Some(1_709_251_199));
        assert_eq!(response.body.expose().as_slice(), b"{}");
        let wire = server.finish().unwrap();
        let path = &original[url::Position::BeforePath..];
        let method = if matches!(index, 0 | 1 | 5 | 7) {
            "POST"
        } else {
            "GET"
        };
        assert_eq!(wire.line.as_str(), format!("{method} {path} HTTP/1.1"));
        assert_eq!(
            wire.headers["user-agent"],
            kumochi_protocol::constants::HTTP_USER_AGENT
        );
        assert_eq!(wire.api_key, None);
        assert_eq!(wire.anthropic_version, None);
        if matches!(index, 0 | 1 | 7) {
            assert_eq!(wire.authorization, None);
            assert_eq!(wire.headers["accept"], "application/json");
            assert_eq!(wire.headers.get("x-github-api-version"), None);
            assert_eq!(
                wire.content_type.as_deref(),
                Some("application/x-www-form-urlencoded")
            );
            let fields: BTreeMap<_, _> = url::form_urlencoded::parse(wire.body.expose())
                .into_owned()
                .collect();
            assert_eq!(fields["client_id"], SENTINEL);
            if index == 1 {
                assert_eq!(fields["device_code"], SENTINEL);
                assert_eq!(
                    fields["grant_type"],
                    "urn:ietf:params:oauth:grant-type:device_code"
                );
                assert_eq!(fields.len(), 3);
            } else if index == 7 {
                assert_eq!(fields["grant_type"], "refresh_token");
                assert_eq!(fields["refresh_token"], format!("ghr_{SENTINEL}"));
                assert_eq!(fields.len(), 3);
                assert!(!fields.contains_key("client_secret"));
            } else {
                assert_eq!(fields.len(), 1);
            }
        } else {
            assert_eq!(
                wire.authorization.as_deref(),
                Some(format!("Bearer ghu_{SENTINEL}").as_str())
            );
            assert_eq!(wire.headers["accept"], "application/vnd.github+json");
            assert_eq!(wire.headers["x-github-api-version"], "2026-03-10");
            if index == 5 {
                assert_eq!(wire.content_type.as_deref(), Some("application/json"));
                assert_eq!(
                    wire.body.expose().as_slice(),
                    format!(r#"{{"title":"{SENTINEL}","body":""}}"#).as_bytes()
                );
            } else {
                assert_eq!(wire.content_type, None);
                assert_eq!(wire.body.expose().as_slice(), b"");
            }
        }
    }
    let logs = captured.text();
    assert!(logs.contains("github-wire-control"));
    assert!(!logs.contains(SENTINEL));
    assert!(!logs.contains(&format!("{:?}", SENTINEL.as_bytes())));
    assert!(
        !logs.contains(
            format!("{:?}", SENTINEL.as_bytes())
                .trim_start_matches('[')
                .trim_end_matches(']')
        )
    );
}

#[tokio::test]
async fn github_legacy_fields_are_refused_before_connecting() {
    use crate::connections::github::tests::endpoints;
    let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = unused.local_addr().unwrap();
    drop(unused);
    let server = Server::start(reply("200 OK", b"", ""));
    let client = server.client();
    for endpoint in endpoints() {
        let result = client
            .send_to(
                &format!("https://{address}/"),
                Request {
                    endpoint: Endpoint::GitHub(endpoint),
                    fields: vec![(
                        "client_id",
                        Secret::new(Zeroizing::new("legacy".to_owned())),
                    )],
                },
            )
            .await;
        assert_eq!(result.unwrap_err(), HttpError::Request);
    }
}

fn single_header_meta(name: &'static str, value: &str) -> ResponseMeta {
    let mut headers = HeaderMap::new();
    headers.insert(name, HeaderValue::from_str(value).unwrap());
    ResponseMeta::parse(&headers)
}

#[test]
fn github_metadata_parses_fixed_gmt_and_retry_times_without_retaining_headers() {
    let parse = single_header_meta;
    assert_eq!(
        ResponseMeta::parse(&HeaderMap::new()),
        ResponseMeta::default()
    );
    for (date, seconds) in [
        ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
        ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
        ("Thu, 29 Feb 2024 23:59:59 GMT", 1_709_251_199),
    ] {
        assert_eq!(parse("date", date).date_s, Some(seconds));
    }
    let date = "Sun, 06 Nov 1994 08:49:37 GMT";
    let exact = format!("{}{}", " ".repeat(64 - date.len()), date);
    assert_eq!(exact.len(), 64);
    assert_eq!(parse("date", &exact).date_s, Some(784_111_777));
    assert_eq!(parse("date", &format!(" {exact}")).date_s, None);
    for bad in [
        "",
        "garbage",
        "Thu, 01 Jan 1970",
        "Thu, 01 Jan 1970 00:00:00 +0000",
        "Wed, 31 Dec 1969 23:59:59 GMT",
        "Thu, 30 Feb 2024 00:00:00 GMT",
        "Mon, 06 Nov 1994 08:49:37 GMT",
    ] {
        assert_eq!(parse("date", bad).date_s, None);
    }
    assert_eq!(
        parse("retry-after", &format!("{}1", "0".repeat(63))).retry_after_s,
        Some(1)
    );
    assert_eq!(
        parse("retry-after", &format!("{}1", "0".repeat(64))).retry_after_s,
        None
    );
    for bad in ["", "-1", "1.5", "18446744073709551616", date] {
        assert_eq!(parse("retry-after", bad).retry_after_s, None);
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "date",
        HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
    );
    headers.insert(
        "retry-after",
        HeaderValue::from_static("Sun, 06 Nov 1994 08:50:37 GMT"),
    );
    assert_eq!(ResponseMeta::parse(&headers).retry_after_s, Some(60));
    headers.insert(
        "retry-after",
        HeaderValue::from_static("Sun, 06 Nov 1994 08:49:36 GMT"),
    );
    assert_eq!(ResponseMeta::parse(&headers).retry_after_s, Some(0));
    headers.insert("date", HeaderValue::from_static("invalid"));
    headers.insert(
        "retry-after",
        HeaderValue::from_static("Sun, 06 Nov 1994 08:50:37 GMT"),
    );
    assert_eq!(
        ResponseMeta::parse(&headers),
        ResponseMeta {
            retry_after_present: true,
            ..ResponseMeta::default()
        }
    );
    headers.clear();
    headers.append("date", HeaderValue::from_str(date).unwrap());
    headers.append("date", HeaderValue::from_str(date).unwrap());
    headers.insert("retry-after", HeaderValue::from_static("60"));
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    headers.insert("x-ratelimit-reset", HeaderValue::from_static("784111837"));
    assert_eq!(
        ResponseMeta::parse(&headers),
        ResponseMeta {
            date_s: None,
            retry_after_s: Some(60),
            retry_after_present: true,
            rate_remaining: Some(0),
            rate_reset_s: Some(784_111_837)
        }
    );
}

#[test]
fn github_metadata_duplicates_and_non_ascii_headers_have_no_parsed_values() {
    for name in [
        "date",
        "retry-after",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
    ] {
        let mut headers = HeaderMap::new();
        let value = if name == "date" {
            "Sun, 06 Nov 1994 08:49:37 GMT"
        } else {
            "0"
        };
        headers.append(name, HeaderValue::from_str(value).unwrap());
        headers.append(name, HeaderValue::from_str(value).unwrap());
        let expected = ResponseMeta {
            retry_after_present: name == "retry-after",
            ..ResponseMeta::default()
        };
        assert_eq!(ResponseMeta::parse(&headers), expected);
        headers.clear();
        headers.insert(name, HeaderValue::from_bytes(&[0xff]).unwrap());
        assert_eq!(ResponseMeta::parse(&headers), expected);
    }
}

#[test]
fn github_metadata_numeric_bounds_reject_whitespace_and_overflow() {
    for name in ["x-ratelimit-remaining", "x-ratelimit-reset"] {
        for (value, expected) in [
            ("0", Some(0)),
            ("18446744073709551615", Some(u64::MAX)),
            ("00000000000000000001", Some(1)),
            ("000000000000000000001", None),
            ("18446744073709551616", None),
            ("", None),
            ("-1", None),
            ("+1", None),
            ("1.0", None),
            (" 1", None),
            ("1 ", None),
        ] {
            let meta = single_header_meta(name, value);
            assert_eq!(
                if name == "x-ratelimit-remaining" {
                    meta.rate_remaining
                } else {
                    meta.rate_reset_s
                },
                expected
            );
        }
    }
}

#[test]
fn retry_after_presence_survives_invalid_values_duplicates_and_bad_date() {
    assert_eq!(
        ResponseMeta::parse(&HeaderMap::new()),
        ResponseMeta::default()
    );
    let present = ResponseMeta {
        retry_after_present: true,
        ..ResponseMeta::default()
    };
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("60"));
    assert_eq!(
        ResponseMeta::parse(&headers),
        ResponseMeta {
            retry_after_s: Some(60),
            ..present
        }
    );
    headers.insert("retry-after", HeaderValue::from_static("invalid"));
    assert_eq!(ResponseMeta::parse(&headers), present);
    headers.insert("retry-after", HeaderValue::from_static("60"));
    headers.append("retry-after", HeaderValue::from_static("60"));
    assert_eq!(ResponseMeta::parse(&headers), present);
    headers.clear();
    headers.insert(
        "retry-after",
        HeaderValue::from_static("Sun, 06 Nov 1994 08:50:37 GMT"),
    );
    assert_eq!(ResponseMeta::parse(&headers), present);
    headers.insert(
        "date",
        HeaderValue::from_static("Mon, 06 Nov 1994 08:49:37 GMT"),
    );
    assert_eq!(ResponseMeta::parse(&headers), present);
    let printed = format!("{:?}", ResponseMeta::parse(&headers));
    assert!(printed.contains("retry_after_present: true"));
    assert!(!printed.contains("Mon, 06 Nov 1994"));
}

#[tokio::test]
async fn github_collection_bounds_cover_length_and_chunked_bodies() {
    use crate::connections::github::tests::endpoints;
    for index in 0..8 {
        let server = Server::start(reply("200 OK", b"", ""));
        let response = server
            .client()
            .send_to(
                &server.url(),
                request(Endpoint::GitHub(endpoints().remove(index))),
            )
            .await
            .unwrap();
        assert_eq!(response.body.expose().as_slice(), b"");
        server.finish().unwrap();
        let status = if index == 5 { "201 Created" } else { "200 OK" };
        let limit = match index {
            0 | 1 | 7 => 4096,
            2 => 65_536,
            _ => 1_048_576,
        };
        for chunked in [false, true] {
            for length in [limit, limit + 1] {
                let body = vec![b'x'; length];
                let reply = if chunked {
                    let mut reply = format!("HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{length:x}\r\n").into_bytes();
                    reply.extend_from_slice(&body);
                    reply.extend_from_slice(b"\r\n0\r\n\r\n");
                    reply
                } else {
                    reply(status, &body, "")
                };
                let server = Server::start(reply);
                let endpoint = endpoints().remove(index);
                let response = server
                    .client()
                    .send_to(&server.url(), request(Endpoint::GitHub(endpoint)))
                    .await;
                if length == limit {
                    assert_eq!(response.unwrap().body.expose().as_slice(), body);
                } else {
                    assert_eq!(response.unwrap_err(), HttpError::BodyTooLarge);
                }
                server.finish().unwrap();
            }
        }
    }
}

#[tokio::test]
async fn github_create_retains_every_rejection_without_reading_diagnostics() {
    use crate::connections::github::tests::endpoints;
    for status in [400, 401, 403, 404, 410, 422, 429, 499] {
        let server = Server::start(format!("HTTP/1.1 {status} Refused\r\nContent-Length: 1048577\r\nRetry-After: 123\r\nConnection: close\r\n\r\n").into_bytes());
        let response = server
            .client()
            .send_to(
                &server.url(),
                request(Endpoint::GitHub(endpoints().remove(5))),
            )
            .await
            .unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.meta.retry_after_s, Some(123));
        assert_eq!(response.body.expose().as_slice(), b"");
        server.finish().unwrap();
    }
}

#[tokio::test]
async fn github_create_reads_bodies_outside_the_4xx_range() {
    use crate::connections::github::tests::endpoints;
    for (status, expected) in [(399, HttpError::Redirect), (500, HttpError::Transport)] {
        let server = Server::start(
            format!(
                "HTTP/1.1 {status} Edge\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
        );
        assert_eq!(
            server
                .client()
                .send_to(
                    &server.url(),
                    request(Endpoint::GitHub(endpoints().remove(5))),
                )
                .await
                .unwrap_err(),
            expected
        );
        server.finish().unwrap();
    }
}

#[tokio::test]
async fn github_refuses_redirects_without_followup_and_incomplete_bodies_are_transport() {
    use crate::connections::github::tests::endpoints;
    let followup = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    followup.set_nonblocking(true).unwrap();
    let address = followup.local_addr().unwrap();
    for endpoint in endpoints() {
        let server = Server::start(reply(
            "307 Temporary Redirect",
            b"",
            &format!("Location: https://{address}/forbidden\r\n"),
        ));
        assert_eq!(
            server
                .client()
                .send_to(&server.url(), request(Endpoint::GitHub(endpoint)))
                .await
                .unwrap_err(),
            HttpError::Redirect
        );
        assert_eq!(
            followup.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        server.finish().unwrap();
    }
    for endpoint in endpoints() {
        let server = Server::start(
            b"HTTP/1.1 201 Created\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx".to_vec(),
        );
        assert_eq!(
            server
                .client()
                .send_to(&server.url(), request(Endpoint::GitHub(endpoint)))
                .await
                .unwrap_err(),
            HttpError::Transport
        );
        server.finish().unwrap();
    }
}

#[tokio::test]
async fn github_deadlines_cover_held_headers_and_body_eof() {
    use crate::connections::github::tests::endpoints;
    for index in 0..8 {
        for reply in [
            Vec::new(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n".to_vec(),
        ] {
            let mut server = Server::start_delayed(reply, Duration::from_secs(40));
            let client = server.client();
            let url = server.url();
            let endpoint = endpoints().remove(index);
            let mut task = tokio::spawn(async move {
                client
                    .send_to(&url, request(Endpoint::GitHub(endpoint)))
                    .await
            });
            server.flushed.take().unwrap().await.unwrap();
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(if matches!(index, 5 | 6) {
                30
            } else {
                15
            }))
            .await;
            let result = tokio::select! {
                result = &mut task => result.unwrap(),
                () = tokio::time::sleep(Duration::from_millis(1)) => panic!("whole GitHub deadline didn't end the request"),
            };
            assert_eq!(result.unwrap_err(), HttpError::Timeout);
            tokio::time::resume();
            server.finish().unwrap();
        }
    }
}

fn unknown_issuer(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = current {
        if matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer
            ))
        ) {
            return true;
        }
        current = match error
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
        {
            Some(inner) => Some(inner),
            None => error.source(),
        };
    }
    false
}

#[test]
fn empty_and_unparsable_root_sets_fail_closed() {
    for roots in [Vec::new(), vec![vec![0, 1, 2]]] {
        assert!(
            matches!(
                from_roots(Ok(TrustRoots {
                    certificates: roots,
                    skipped: 0,
                    unreadable: 0
                })),
                Err(HttpError::TrustStoreUnavailable)
            ),
            "no fallback trust roots"
        );
    }
}

#[tokio::test]
async fn unknown_issuer_is_rejected_and_the_same_ca_is_a_positive_control() {
    let untrusted = Server::start(reply("200 OK", b"{}", ""));
    let unrelated = Server::start(reply("200 OK", b"{}", ""));
    let client = from_roots(Ok(TrustRoots {
        certificates: vec![unrelated.root.clone()],
        skipped: 0,
        unreadable: 0,
    }))
    .expect("unrelated valid CA");
    let root = untrusted.root.clone();
    let config = Arc::clone(&untrusted.config);
    let Err(error) = client.get(untrusted.url()).send().await else {
        panic!("untrusted issuer was accepted");
    };
    assert!(
        unknown_issuer(&error),
        "the exact TLS rejection is UnknownIssuer"
    );
    assert!(
        untrusted.finish().is_err(),
        "server observes the rejected handshake"
    );
    drop(unrelated);
    let trusted = Server::with_config(reply("200 OK", b"{}", ""), root, config, Duration::ZERO);
    let response = trusted
        .client()
        .send_to(&trusted.url(), request(Endpoint::Discovery))
        .await
        .expect("trusted TLS request");
    assert_eq!(response.status, 200);
    assert_eq!(response.body.expose().as_slice(), b"{}");
    trusted.finish().expect("fixture completed");
}

#[tokio::test]
async fn http_is_refused_before_connect_and_https_still_works() {
    let server = Server::start(reply("200 OK", b"ok", ""));
    let client = server.client();
    let Err(error) = client
        .0
        .get(format!("http://{}/fixture", server.address))
        .send()
        .await
    else {
        panic!("HTTP URL was accepted");
    };
    assert!(
        error.is_builder(),
        "HTTPS-only rejects before network dispatch"
    );
    let response = client
        .send_to(&server.url(), request(Endpoint::Discovery))
        .await
        .expect("TLS listener was untouched by HTTP refusal");
    assert_eq!(response.body.expose().as_slice(), b"ok");
    server.finish().expect("fixture completed");
}

#[tokio::test]
async fn redirect_is_refused_instead_of_followed() {
    let server = Server::start(reply("302 Found", b"", "Location: /forbidden\r\n"));
    let response = server
        .client()
        .send_to(&server.url(), request(Endpoint::Discovery))
        .await;
    assert_eq!(response.unwrap_err(), HttpError::Redirect);
    server.finish().expect("fixture completed");
}

#[tokio::test]
async fn auth_discovery_and_error_bodies_all_have_the_same_byte_cap() {
    for (endpoint, status) in [
        (Endpoint::Token, "200 OK"),
        (Endpoint::Discovery, "200 OK"),
        (Endpoint::Token, "400 Bad Request"),
    ] {
        let server = Server::start(reply(status, &vec![b'x'; MAX_BODY_BYTES + 1], ""));
        let response = server
            .client()
            .send_to(&server.url(), super::tests::request(endpoint))
            .await;
        assert_eq!(response.unwrap_err(), HttpError::BodyTooLarge);
        server
            .finish()
            .expect("fixture finished sending its bounded overflow");
    }
}

#[tokio::test]
async fn trust_root_diagnostics_log_counts_and_loader_codes_only() {
    let captured = Captured::default();
    let _logging = captured.install();
    let server = Server::start(reply("200 OK", b"{}", ""));
    let roots = TrustRoots {
        certificates: vec![server.root.clone(), SENTINEL.as_bytes().to_vec()],
        skipped: 7,
        unreadable: 2,
    };
    let client = Client(from_roots(Ok(roots)).expect("one parseable CA"));
    client
        .send_to(&server.url(), request(Endpoint::Discovery))
        .await
        .expect("trusted fixture");
    server.finish().expect("complete request");
    assert!(matches!(
        from_roots(Err(TrustRootsError::Open(5))),
        Err(HttpError::TrustStoreUnavailable)
    ));
    let text = captured.text();
    assert_eq!(text.matches("account trust roots").count(), 2);
    for expected in [
        "kept=1",
        "skipped=7",
        "unreadable=2",
        "unparsable=1",
        "loader_error=Open(5)",
    ] {
        assert!(
            text.contains(expected),
            "missing safe root diagnostic {expected}"
        );
    }
    assert!(!text.contains(SENTINEL));
}

#[tokio::test]
async fn exact_body_limit_is_accepted_and_missing_length_grows() {
    let expected = vec![b'x'; MAX_BODY_BYTES];
    let server = Server::start(reply("200 OK", &expected, ""));
    let response = server
        .client()
        .send_to(&server.url(), request(Endpoint::Discovery))
        .await
        .expect("exact cap accepted");
    assert_eq!(response.body.expose().as_slice(), expected);
    server.finish().expect("complete request");
    let server = Server::start(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nsmall".to_vec());
    let response = server
        .client()
        .send_to(&server.url(), request(Endpoint::Discovery))
        .await
        .expect("body without a length");
    assert_eq!(response.body.expose().as_slice(), b"small");
    server.finish().expect("complete request");
}

/// A TLS handshake that never finishes fails at the connect deadline. reqwest reports that as a
/// timeout too, but nothing was sent, so it's `Connect`.
#[tokio::test]
async fn a_handshake_past_its_connect_deadline_is_a_connect_failure() {
    let ca = Server::start(Vec::new());
    let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback port 0");
    let url = format!("https://{}/fixture", silent.local_addr().expect("address"));
    let client = Client(
        connecting_within(
            Ok(TrustRoots {
                certificates: vec![ca.root.clone()],
                skipped: 0,
                unreadable: 0,
            }),
            Duration::from_millis(200),
        )
        .expect("supplied test CA"),
    );
    let error = client.0.get(&url).send().await.expect_err("no handshake");
    assert!(error.is_connect() && error.is_timeout(), "{error:?}");
    assert_eq!(
        client
            .send_to(&url, request(Endpoint::Discovery))
            .await
            .unwrap_err(),
        HttpError::Connect
    );
    drop(silent);
}

#[tokio::test]
async fn a_truncated_body_is_transport_and_a_stalled_body_is_timeout() {
    let server = Server::start(
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx".to_vec(),
    );
    assert_eq!(
        server
            .client()
            .send_to(&server.url(), request(Endpoint::Discovery))
            .await
            .unwrap_err(),
        HttpError::Transport
    );
    server
        .finish()
        .expect("request read before truncated reply");
    let server = Server::start_delayed(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n".to_vec(),
        Duration::from_secs(2),
    );
    let builder = server
        .client()
        .0
        .get(server.url())
        .timeout(Duration::from_secs(1));
    let response = builder
        .send()
        .await
        .expect("headers arrive before the body stalls");
    assert_eq!(
        read_response(response, None).await.unwrap_err(),
        HttpError::Timeout
    );
    server.finish().expect("request read before body stalled");
}

#[tokio::test]
async fn token_posts_are_form_encoded_and_public_gets_have_no_body() {
    let pairs = [
        ("grant_type", "authorization_code"),
        ("client_id", "issued-client"),
        ("code", "one +&=/é"),
        ("code_verifier", "v_v-v.v~v"),
        ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
        ("resource", "https://api.openai.com/v1"),
    ];
    let server = Server::start(reply("200 OK", b"{}", ""));
    let token_request = Request {
        endpoint: Endpoint::Token,
        fields: pairs
            .iter()
            .map(|(key, value)| (*key, Secret::new(Zeroizing::new((*value).to_owned()))))
            .collect(),
    };
    server
        .client()
        .send_to(&server.url(), token_request)
        .await
        .expect("POST");
    let wire = server.finish().expect("captured POST");
    assert_eq!(wire.line.as_str(), "POST /fixture HTTP/1.1");
    assert_eq!(wire.authorization, None);
    assert_eq!(
        wire.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    let decoded: Vec<_> = url::form_urlencoded::parse(wire.body.expose())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(decoded.len(), pairs.len());
    assert_eq!(
        decoded.into_iter().collect::<BTreeMap<_, _>>(),
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    );
    let bearer = Secret::new(Zeroizing::new("bearer-token".to_owned()));
    for (endpoint, authorization) in [
        (Endpoint::Discovery, None),
        (
            Endpoint::Jwks(IssuerUrl::parse("https://auth.openai.com/keys").expect("same origin")),
            None,
        ),
        (Endpoint::Models(bearer), Some("Bearer bearer-token")),
    ] {
        let server = Server::start(reply("200 OK", b"{}", ""));
        server
            .client()
            .send_to(&server.url(), request(endpoint))
            .await
            .expect("GET");
        let wire = server.finish().expect("captured GET");
        assert_eq!(wire.line.as_str(), "GET /fixture HTTP/1.1");
        assert_eq!(wire.content_type, None);
        assert_eq!(wire.authorization.as_deref(), authorization);
        assert!(wire.body.expose().is_empty());
    }
}

#[tokio::test]
async fn public_endpoints_refuse_fields_before_connecting() {
    for endpoint in [
        Endpoint::Discovery,
        Endpoint::Jwks(IssuerUrl::parse("https://auth.openai.com/keys").expect("same origin")),
        Endpoint::Models(Secret::new(Zeroizing::new("bearer-token".to_owned()))),
    ] {
        let server = Server::start(reply("200 OK", b"{}", ""));
        let client = server.client();
        let with_fields = Request {
            endpoint,
            fields: vec![("code", Secret::new(Zeroizing::new(SENTINEL.to_owned())))],
        };
        assert_eq!(
            client
                .send_to(&server.url(), with_fields)
                .await
                .unwrap_err(),
            HttpError::Request
        );
        client
            .send_to(&server.url(), request(Endpoint::Discovery))
            .await
            .expect("listener untouched");
        server.finish().expect("only the public request arrived");
    }
}

#[tokio::test]
async fn forms_and_responses_do_not_enter_logs_or_debug() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone()),
        )
        .with(crate::logging::filter(tracing::Level::TRACE));
    let _logging = tracing::subscriber::set_default(subscriber);
    tracing::info!("capture-control");
    let server = Server::start(reply(
        "200 OK",
        SENTINEL.as_bytes(),
        "x-request-id: req_fixture\r\n",
    ));
    let request = Request {
        endpoint: Endpoint::Token,
        fields: vec![("code", Secret::new(Zeroizing::new(SENTINEL.to_owned())))],
    };
    assert!(
        !format!("{request:?}").contains(SENTINEL),
        "form values redacted"
    );
    let client = server.client();
    let response = client
        .send_to(&format!("{}?query={SENTINEL}", server.url()), request)
        .await
        .expect("bounded fixture");
    assert_eq!(response.body.expose().as_slice(), SENTINEL.as_bytes());
    assert!(
        response.body.expose().capacity() < MAX_BODY_BYTES,
        "a small response doesn't reserve the whole body cap"
    );
    assert_eq!(response.request_id.as_deref(), Some("req_fixture"));
    assert!(
        !format!("{response:?}").contains(SENTINEL),
        "response body redacted"
    );
    let unavailable = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("unused loopback port");
    let address = unavailable.local_addr().expect("unused address");
    drop(unavailable);
    let error = client
        .send_to(
            &format!("https://{address}/failed?query={SENTINEL}"),
            Request {
                endpoint: Endpoint::Discovery,
                fields: Vec::new(),
            },
        )
        .await
        .expect_err("the loopback listener was closed");
    assert_eq!(error, HttpError::Connect);
    assert!(
        captured.text().contains("capture-control"),
        "capture active"
    );
    assert!(
        !captured.text().contains(SENTINEL),
        "query, form and body never logged"
    );
    assert!(
        !captured.text().contains("127.0.0.1") && !captured.text().contains(&address.to_string()),
        "connection hosts and addresses never logged"
    );
    server.finish().expect("fixture completed");
}

#[tokio::test]
async fn request_ids_are_bounded_graphic_ascii() {
    for (value, expected) in [
        ("r".repeat(256), Some("r".repeat(256))),
        ("r".repeat(257), None),
        ("contains space".to_owned(), None),
        ("non-ascii-é".to_owned(), None),
    ] {
        let server = Server::start(reply(
            "200 OK",
            b"{}",
            &format!("x-request-id: {value}\r\n"),
        ));
        let response = server
            .client()
            .send_to(&server.url(), request(Endpoint::Discovery))
            .await
            .expect("bounded fixture");
        assert_eq!(response.request_id, expected);
        server.finish().expect("fixture completed");
    }
}

#[test]
#[ignore = "entry point for the isolated proxy child process"]
fn proxy_probe_child() {
    let url =
        std::env::var_os("ACCOUNT_HTTP_TEST_CHILD").expect("parent supplies the loopback URL");
    let url = url::Url::parse(url.to_str().expect("numeric test URL")).expect("fixture URL");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.scheme(), "https");
    assert!(
        url.port().is_some_and(|port| port != 0),
        "port from the parent's listener"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let result = runtime.block_on(async {
        let client = super::build().expect("real public ROOT store");
        client
            .0
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
    });
    let error = result.expect_err("the generated fixture CA isn't in the real ROOT store");
    assert!(
        unknown_issuer(&error),
        "the request reached the TLS target directly, not the dead proxy"
    );
    println!("transport-rejected");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "created under spawn_lock; its output is read after the guard is released"
)]
fn production_client_ignores_proxy_environment_in_an_isolated_child() {
    let target = Server::start(reply("200 OK", b"{}", ""));
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy port 0");
    let proxy_address = proxy.local_addr().expect("proxy address");
    drop(proxy);
    let spawning = crate::platform::spawn_lock();
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "connections::http_client::tests::proxy_probe_child",
            "--nocapture",
            "--ignored",
        ])
        .env("HTTPS_PROXY", format!("http://{proxy_address}"))
        .env("HTTP_PROXY", format!("http://{proxy_address}"))
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env("ACCOUNT_HTTP_TEST_CHILD", target.url())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("child test process");
    drop(spawning);
    let output = child.wait_with_output().expect("child test output");
    assert!(
        output.status.success(),
        "child reports a closed transport result"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("transport-rejected"),
        "child reached the production constructor"
    );
    assert!(
        target.finish().is_err(),
        "direct TLS connection rejected the private fixture CA"
    );
}

#[test]
fn destinations_queries_methods_and_headers_are_closed() {
    use crate::connections::github::tests::endpoints;
    let expected = [
        ("POST", "https://github.com/login/device/code"),
        ("POST", "https://github.com/login/oauth/access_token"),
        ("GET", "https://api.github.com/user"),
        (
            "GET",
            "https://api.github.com/user/installations?per_page=100&page=2",
        ),
        (
            "GET",
            "https://api.github.com/user/installations/42/repositories?per_page=100&page=1",
        ),
        ("POST", "https://api.github.com/repos/owner/repo/issues"),
        (
            "GET",
            "https://api.github.com/repos/owner/repo/issues?per_page=100&page=2&creator=owner&state=all&since=2024-02-29T23%3A59%3A59Z&sort=created&direction=desc",
        ),
        ("POST", "https://github.com/login/oauth/access_token"),
    ];
    let server = Server::start(reply("200 OK", b"", ""));
    let client = server.client();
    for (index, (endpoint, (method, url))) in endpoints().into_iter().zip(expected).enumerate() {
        assert_eq!(endpoint.url().unwrap().as_str(), url);
        let request = client
            .github_request(&endpoint, url)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method().as_str(), method);
        assert_eq!(request.url().as_str(), url);
        assert_eq!(
            request.headers()["user-agent"],
            kumochi_protocol::constants::HTTP_USER_AGENT
        );
        assert_eq!(request.headers().get("x-api-key"), None);
        if matches!(index, 0 | 1 | 7) {
            assert_eq!(request.headers()["accept"], "application/json");
            assert_eq!(request.headers().get("authorization"), None);
            assert_eq!(request.headers().get("x-github-api-version"), None);
            assert_eq!(
                request.headers()["content-type"],
                "application/x-www-form-urlencoded"
            );
            assert_eq!(request.timeout(), Some(&Duration::from_secs(15)));
            let body = request.body().unwrap().as_bytes().unwrap();
            let fields: Vec<_> = url::form_urlencoded::parse(body).into_owned().collect();
            let mut expected = vec![("client_id".to_owned(), SENTINEL.to_owned())];
            if index == 1 {
                expected.extend([
                    ("device_code".to_owned(), SENTINEL.to_owned()),
                    (
                        "grant_type".to_owned(),
                        "urn:ietf:params:oauth:grant-type:device_code".to_owned(),
                    ),
                ]);
            } else if index == 7 {
                expected.extend([
                    ("grant_type".to_owned(), "refresh_token".to_owned()),
                    ("refresh_token".to_owned(), format!("ghr_{SENTINEL}")),
                ]);
            }
            assert_eq!(fields, expected);
        } else {
            assert_eq!(request.headers()["accept"], "application/vnd.github+json");
            assert_eq!(request.headers()["x-github-api-version"], "2026-03-10");
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer ghu_{SENTINEL}")
            );
            assert!(request.headers()["authorization"].is_sensitive());
            assert_eq!(
                request.timeout(),
                Some(&Duration::from_secs(if index >= 5 { 30 } else { 15 }))
            );
            if index == 5 {
                assert_eq!(request.headers()["content-type"], "application/json");
                assert_eq!(
                    request.body().unwrap().as_bytes().unwrap(),
                    format!(r#"{{"title":"{SENTINEL}","body":""}}"#).as_bytes()
                );
            } else {
                assert!(request.body().is_none(), "reads have no payload");
                assert_eq!(request.headers().get("content-type"), None);
            }
        }
    }
}
