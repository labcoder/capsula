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

use super::{Client, Endpoint, HttpError, MAX_BODY_BYTES, Request, from_roots};
use crate::test_support::{Captured, SENTINEL};

struct Server {
    address: SocketAddr,
    root: Vec<u8>,
    config: Arc<rustls::ServerConfig>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl Server {
    fn start(reply: Vec<u8>) -> Self {
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
        Self::with_config(reply, root, Arc::new(config))
    }

    fn with_config(reply: Vec<u8>, root: Vec<u8>, config: Arc<rustls::ServerConfig>) -> Self {
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
            tls.write_all(&reply)?;
            tls.flush()
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
        Client(from_roots(vec![self.root.clone()]).expect("supplied test CA"))
    }

    fn finish(mut self) -> io::Result<()> {
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
            matches!(from_roots(roots), Err(HttpError::TrustStoreUnavailable)),
            "no fallback trust roots"
        );
    }
}

#[tokio::test]
async fn unknown_issuer_is_rejected_and_the_same_ca_is_a_positive_control() {
    let untrusted = Server::start(reply("200 OK", b"{}", ""));
    let unrelated = Server::start(reply("200 OK", b"{}", ""));
    let client = from_roots(vec![unrelated.root.clone()]).expect("unrelated valid CA");
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
    let trusted = Server::with_config(reply("200 OK", b"{}", ""), root, config);
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
            .send_to(&server.url(), request(endpoint))
            .await;
        assert_eq!(response.unwrap_err(), HttpError::BodyTooLarge);
        server
            .finish()
            .expect("fixture finished sending its bounded overflow");
    }
}

#[tokio::test]
async fn forms_and_responses_do_not_enter_logs_or_debug() {
    let captured = Captured::default();
    let _logging = captured.install();
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
    let response = server
        .client()
        .send_to(&format!("{}?query={SENTINEL}", server.url()), request)
        .await
        .expect("bounded fixture");
    assert_eq!(response.body.expose().as_slice(), SENTINEL.as_bytes());
    assert_eq!(response.request_id.as_deref(), Some("req_fixture"));
    assert!(
        !format!("{response:?}").contains(SENTINEL),
        "response body redacted"
    );
    assert!(
        captured.text().contains("capture-control"),
        "capture active"
    );
    assert!(
        !captured.text().contains(SENTINEL),
        "query, form and body never logged"
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
fn proxy_probe_child() {
    let Some(url) = std::env::var_os("ACCOUNT_HTTP_TEST_CHILD") else {
        return;
    };
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
    assert!(
        result.is_err(),
        "the generated fixture CA isn't in the real ROOT store"
    );
    println!("transport-rejected");
}

#[test]
fn production_client_ignores_proxy_environment_in_an_isolated_child() {
    let target = Server::start(reply("200 OK", b"{}", ""));
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy port 0");
    proxy.set_nonblocking(true).expect("nonblocking probe");
    let proxy_address = proxy.local_addr().expect("proxy address");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "connections::http_client::tests::proxy_probe_child",
            "--nocapture",
        ])
        .env("HTTPS_PROXY", format!("http://{proxy_address}"))
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
        matches!(proxy.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
        "the proxy saw no connection"
    );
    assert!(
        target.finish().is_err(),
        "direct TLS connection rejected the private fixture CA"
    );
}
