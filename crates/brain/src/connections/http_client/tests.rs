use std::collections::BTreeMap;
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kumochi_protocol::Secret;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use zeroize::Zeroizing;

use super::{
    Client, Endpoint, HttpError, IssuerUrl, MAX_BODY_BYTES, Request, body_buffer, from_roots,
    read_response,
};
use crate::platform::{TrustRoots, TrustRootsError};
use crate::test_support::{Captured, SENTINEL};

struct Server {
    address: SocketAddr,
    root: Vec<u8>,
    config: Arc<rustls::ServerConfig>,
    task: Option<JoinHandle<io::Result<WireRequest>>>,
}

struct WireRequest {
    line: Zeroizing<String>,
    content_type: Option<String>,
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
                body: Secret::new(body),
            };
            tls.write_all(&reply)?;
            if delay.is_zero() {
                tls.conn.send_close_notify();
            }
            tls.flush()?;
            thread::sleep(delay);
            Ok(wire)
        });
        Self {
            address,
            root,
            config,
            task: Some(task),
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

#[test]
fn allocation_hints_are_bounded_and_optional() {
    assert_eq!(body_buffer(None).capacity(), 0);
    for (length, minimum) in [(333, 333), (u64::MAX, MAX_BODY_BYTES)] {
        let bytes = body_buffer(Some(length));
        assert!(bytes.is_empty());
        assert!(bytes.capacity() >= minimum);
        assert!(bytes.capacity() <= MAX_BODY_BYTES);
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
        read_response(response).await.unwrap_err(),
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
    for endpoint in [
        Endpoint::Discovery,
        Endpoint::Jwks(IssuerUrl::parse("https://auth.openai.com/keys").expect("same origin")),
    ] {
        let server = Server::start(reply("200 OK", b"{}", ""));
        server
            .client()
            .send_to(&server.url(), request(endpoint))
            .await
            .expect("public GET");
        let wire = server.finish().expect("captured GET");
        assert_eq!(wire.line.as_str(), "GET /fixture HTTP/1.1");
        assert_eq!(wire.content_type, None);
        assert!(wire.body.expose().is_empty());
    }
}

#[tokio::test]
async fn public_endpoints_refuse_fields_before_connecting() {
    for endpoint in [
        Endpoint::Discovery,
        Endpoint::Jwks(IssuerUrl::parse("https://auth.openai.com/keys").expect("same origin")),
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
    assert_eq!(error, HttpError::Transport);
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
fn production_client_ignores_proxy_environment_in_an_isolated_child() {
    let target = Server::start(reply("200 OK", b"{}", ""));
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy port 0");
    let proxy_address = proxy.local_addr().expect("proxy address");
    drop(proxy);
    let output = Command::new(std::env::current_exe().expect("test executable"))
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
        .output()
        .expect("child test process");
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
