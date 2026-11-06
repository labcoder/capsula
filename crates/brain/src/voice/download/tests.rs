use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::mpsc as async_mpsc;

use super::*;
use crate::test_support::{Captured, SENTINEL, Scratch};

const MODEL: ModelFile = ModelFile {
    part: VoicePart::SpeechInput,
    name: "fixture.bin",
    url: "https://huggingface.co/fixture",
    bytes: 6,
    sha256: "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721",
};

struct Reply {
    wire: Vec<u8>,
    hold: bool,
}

impl Reply {
    fn raw(wire: &str) -> Self {
        Self {
            wire: wire.as_bytes().to_vec(),
            hold: false,
        }
    }

    fn full() -> Self {
        Self::raw("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef")
    }

    fn redirect(location: &str) -> Self {
        Self::raw(&format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    }

    fn stalled() -> Self {
        Self {
            wire: b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".to_vec(),
            hold: true,
        }
    }
}

struct Server {
    base: Url,
    root: Vec<u8>,
    requests: async_mpsc::UnboundedReceiver<String>,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    release: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Server {
    fn start(replies: Vec<Reply>) -> Self {
        let key = KeyPair::generate().expect("valid voice fixture");
        let cert = CertificateParams::new(vec!["127.0.0.1".into()])
            .expect("valid voice fixture")
            .self_signed(&key)
            .expect("valid voice fixture");
        let root = cert.der().to_vec();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("valid voice fixture")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("valid voice fixture");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("valid voice fixture");
        let base = Url::parse(&format!(
            "https://{}/",
            listener.local_addr().expect("valid voice fixture")
        ))
        .expect("valid voice fixture");
        listener.set_nonblocking(true).expect("valid voice fixture");
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (requests_tx, requests) = async_mpsc::unbounded_channel();
        let (release, release_rx) = mpsc::channel();
        let stopping = Arc::clone(&stop);
        let accepts = Arc::clone(&accepted);
        let config = Arc::new(config);
        let worker = thread::spawn(move || {
            for reply in replies {
                let socket = loop {
                    if stopping.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => return Err(error),
                    }
                };
                accepts.fetch_add(1, Ordering::SeqCst);
                socket.set_nonblocking(false)?;
                socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                let connection =
                    rustls::ServerConnection::new(Arc::clone(&config)).map_err(io::Error::other)?;
                let mut tls = rustls::StreamOwned::new(connection, socket);
                let mut header = Vec::new();
                let mut byte = [0];
                while !header.ends_with(b"\r\n\r\n") {
                    tls.read_exact(&mut byte)?;
                    header.push(byte[0]);
                    assert!(header.len() <= 8192, "fixture request header bound");
                }
                requests_tx
                    .send(String::from_utf8(header).expect("valid voice fixture"))
                    .map_err(io::Error::other)?;
                tls.write_all(&reply.wire)?;
                tls.flush()?;
                if reply.hold {
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(io::Error::other)?;
                }
                tls.conn.send_close_notify();
                if let Err(error) = tls.flush()
                    && error.kind() != io::ErrorKind::ConnectionAborted
                    && error.kind() != io::ErrorKind::BrokenPipe
                    && error.kind() != io::ErrorKind::ConnectionReset
                {
                    return Err(error);
                }
            }
            Ok(())
        });
        Self {
            base,
            root,
            requests,
            accepted,
            stop,
            release,
            worker: Some(worker),
        }
    }

    fn downloader(&self) -> (Scratch, Downloader) {
        let scratch = Scratch::new("voice");
        let models = scratch.db().with_file_name("models");
        let downloader = Downloader::for_test(
            models,
            self.base.clone(),
            self.root.clone(),
            vec!["127.0.0.1".into()],
        );
        (scratch, downloader)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        match self.release.send(()) {
            Ok(()) | Err(_) => {}
        }
        self.worker
            .take()
            .expect("valid voice fixture")
            .join()
            .expect("valid voice fixture")
            .expect("valid voice fixture");
    }
}

impl Downloader {
    fn for_test(models: PathBuf, base: Url, root: Vec<u8>, allowlist: Vec<String>) -> Self {
        let rules = Rules::Fixture {
            port: base.port_or_known_default().expect("valid voice fixture"),
            hosts: allowlist,
        };
        let mut downloader =
            Self::from_roots(models, vec![root], rules).expect("valid voice fixture");
        downloader.base = Some(base);
        downloader.space = Some(u64::MAX);
        downloader
    }
}

fn final_path(downloader: &Downloader) -> PathBuf {
    downloader.models.join(MODEL.part.dir()).join(MODEL.name)
}

fn part_path(downloader: &Downloader) -> PathBuf {
    final_path(downloader).with_file_name("fixture.bin.part")
}

fn seed(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("valid voice fixture")).expect("valid voice fixture");
    fs::write(path, bytes).expect("valid voice fixture");
}

async fn fetch(downloader: &Downloader) -> (Result<(), DownloadError>, Progress) {
    let (_cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let result = downloader
        .fetch_files(&[&MODEL], &progress, &receiver)
        .await;
    let value = *progress.borrow();
    (result, value)
}

#[tokio::test]
async fn sizes_exact_over_short_empty_and_missing_length() {
    for (wire, expected, kept) in [
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef",
            Ok(()),
            None,
        ),
        (
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcdef",
            Ok(()),
            None,
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nabcdefg",
            Err(DownloadError::Size),
            None,
        ),
        (
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcdefg",
            Err(DownloadError::Size),
            None,
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde",
            Err(DownloadError::Size),
            None,
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcde",
            Err(DownloadError::Transport),
            Some(b"abcde".as_slice()),
        ),
        (
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
            Err(DownloadError::Transport),
            Some(b"".as_slice()),
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nxxxxxx",
            Err(DownloadError::Hash),
            None,
        ),
    ] {
        let server = Server::start(vec![Reply::raw(wire)]);
        let (_scratch, downloader) = server.downloader();
        let (result, progress) = fetch(&downloader).await;
        assert_eq!(result, expected, "{wire:?}");
        if expected == Ok(()) {
            assert_eq!(
                fs::read(final_path(&downloader)).expect("valid voice fixture"),
                b"abcdef",
                "verified body"
            );
            assert_eq!(
                progress,
                Progress {
                    phase: FetchPhase::Downloading,
                    done: 6,
                    total: 6
                },
                "terminal progress"
            );
        } else {
            assert!(
                !final_path(&downloader).exists(),
                "no unverified final file"
            );
        }
        assert_eq!(
            fs::read(part_path(&downloader)).ok().as_deref(),
            kept,
            "retention for {wire:?}"
        );
    }
}

#[tokio::test]
async fn truncated_response_resumes_with_exact_headers() {
    let mut server = Server::start(vec![
        Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc"),
        Reply::raw(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef",
        ),
    ]);
    let (_scratch, downloader) = server.downloader();
    assert_eq!(
        fetch(&downloader).await.0,
        Err(DownloadError::Transport),
        "truncated first response"
    );
    assert_eq!(
        fs::read(part_path(&downloader)).expect("valid voice fixture"),
        b"abc",
        "prefix saved"
    );
    assert_eq!(
        fetch(&downloader).await,
        (
            Ok(()),
            Progress {
                phase: FetchPhase::Downloading,
                done: 6,
                total: 6
            }
        ),
        "resumed"
    );
    assert_eq!(
        fs::read(final_path(&downloader)).expect("valid voice fixture"),
        b"abcdef",
        "whole verified file"
    );
    let first = server.requests.recv().await.expect("valid voice fixture");
    let second = server.requests.recv().await.expect("valid voice fixture");
    let headers = |request: &str| -> BTreeMap<String, String> {
        request
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_lowercase(), value.trim().to_owned()))
            .collect()
    };
    let mut expected = BTreeMap::from([
        ("host".into(), server.base.authority().to_owned()),
        ("accept".into(), "*/*".into()),
        ("accept-encoding".into(), "identity".into()),
        (
            "user-agent".into(),
            format!("{}/{}", constants::DISPLAY_NAME, env!("CARGO_PKG_VERSION")),
        ),
    ]);
    assert_eq!(headers(&first), expected, "no auth, cookies or identifiers");
    expected.insert("range".into(), "bytes=3-".into());
    assert_eq!(headers(&second), expected, "only Range added on resume");
}

#[tokio::test]
async fn restart_statuses_and_the_single_restart_bound() {
    for status in [
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-5/6\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ] {
        for twice in [false, true] {
            let mut server = Server::start(vec![
                Reply::raw(status),
                if twice {
                    Reply::raw(status)
                } else {
                    Reply::full()
                },
            ]);
            let (_scratch, downloader) = server.downloader();
            seed(&part_path(&downloader), b"abc");
            assert_eq!(
                fetch(&downloader).await.0,
                if twice {
                    Err(DownloadError::Refused)
                } else {
                    Ok(())
                },
                "restart bound"
            );
            assert!(
                server
                    .requests
                    .recv()
                    .await
                    .expect("valid voice fixture")
                    .contains("range: bytes=3-\r\n"),
                "first resumes"
            );
            assert!(
                !server
                    .requests
                    .recv()
                    .await
                    .expect("valid voice fixture")
                    .contains("range:"),
                "restart is fresh"
            );
            if !twice {
                assert_eq!(
                    fs::read(final_path(&downloader)).expect("valid voice fixture"),
                    b"abcdef",
                    "correct restart"
                );
            }
        }
    }
    let server = Server::start(vec![Reply::full()]);
    let (_scratch, downloader) = server.downloader();
    seed(&part_path(&downloader), b"bad");
    assert_eq!(
        fetch(&downloader).await.0,
        Ok(()),
        "200 replaces ranged prefix"
    );
    assert_eq!(
        fs::read(final_path(&downloader)).expect("valid voice fixture"),
        b"abcdef",
        "200 did not append"
    );
}

#[tokio::test]
async fn existing_files_crash_partials_and_exclusive_lock() {
    let server = Server::start(vec![Reply::full()]);
    let (_scratch, downloader) = server.downloader();
    seed(&final_path(&downloader), b"abcdef");
    assert_eq!(
        fetch(&downloader).await,
        (
            Ok(()),
            Progress {
                phase: FetchPhase::Verifying,
                done: 6,
                total: 6
            }
        ),
        "verified final skipped"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "no request for verified file"
    );
    fs::remove_file(final_path(&downloader)).expect("valid voice fixture");
    seed(&part_path(&downloader), b"abcdef");
    assert_eq!(
        fetch(&downloader).await.0,
        Ok(()),
        "complete crash partial promoted without request"
    );
    assert_eq!(
        fs::read(final_path(&downloader)).expect("valid voice fixture"),
        b"abcdef",
        "verified crash partial"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "no redundant request"
    );
    fs::remove_file(final_path(&downloader)).expect("valid voice fixture");
    seed(&part_path(&downloader), b"abc");
    let lock = locked_file(&part_path(&downloader)).expect("valid voice fixture");
    assert_eq!(
        fetch(&downloader).await.0,
        Err(DownloadError::Io),
        "exclusive lock refuses a second writer"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "locked partial sends nothing"
    );
    drop(lock);
    assert_eq!(
        fs::read(part_path(&downloader)).expect("valid voice fixture"),
        b"abc",
        "locked partial unchanged"
    );
    seed(&part_path(&downloader), b"abcdefg");
    assert_eq!(
        fetch(&downloader).await.0,
        Ok(()),
        "oversize crash partial replaced"
    );
    assert_eq!(
        fs::read(final_path(&downloader)).expect("valid voice fixture"),
        b"abcdef",
        "fresh verified file"
    );
}

#[tokio::test]
async fn corrupt_final_is_removed_before_request_and_hash_failures_delete_partials() {
    let mut server = Server::start(vec![Reply::stalled()]);
    let (_scratch, downloader) = server.downloader();
    seed(&final_path(&downloader), b"xxxxxx");
    let (cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let future = downloader.fetch_files(&[&MODEL], &progress, &receiver);
    tokio::pin!(future);
    tokio::select! { result = &mut future => panic!("unexpected finish: {result:?}"), request = server.requests.recv() => { request.expect("valid voice fixture"); } }
    assert!(
        !final_path(&downloader).exists(),
        "bad final removed before response body"
    );
    cancel.send_replace(true);
    assert_eq!(
        future.await,
        Err(DownloadError::Cancelled),
        "cancel stalled replacement"
    );

    let server = Server::start(vec![]);
    let (_scratch, downloader) = server.downloader();
    seed(&part_path(&downloader), b"xxxxxx");
    assert_eq!(
        fetch(&downloader).await.0,
        Err(DownloadError::Hash),
        "complete bad partial"
    );
    assert!(!part_path(&downloader).exists(), "hash failure removed");
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "bad full prefix verified locally"
    );
}

#[tokio::test]
async fn free_space_exact_need_one_under_and_resume_credit() {
    for (prefix, available, expected, needed) in [
        (b"".as_slice(), 7, Ok(()), 7),
        (
            b"".as_slice(),
            6,
            Err(DownloadError::NoSpace { needed: 7 }),
            7,
        ),
        (b"abc".as_slice(), 4, Ok(()), 4),
        (
            b"abc".as_slice(),
            3,
            Err(DownloadError::NoSpace { needed: 4 }),
            4,
        ),
        (
            b"abcdefg".as_slice(),
            6,
            Err(DownloadError::NoSpace { needed: 7 }),
            7,
        ),
    ] {
        let server = Server::start(vec![if prefix == b"abc" {
            Reply::raw(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef",
            )
        } else {
            Reply::full()
        }]);
        let (_scratch, mut downloader) = server.downloader();
        downloader.space = Some(available);
        if !prefix.is_empty() {
            seed(&part_path(&downloader), prefix);
        }
        assert_eq!(
            fetch(&downloader).await.0,
            expected,
            "need {needed}, available {available}"
        );
        if expected != Ok(()) {
            assert_eq!(
                server.accepted.load(Ordering::SeqCst),
                0,
                "no request without space"
            );
            assert_eq!(
                fs::read(part_path(&downloader)).ok(),
                if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_vec())
                },
                "nothing written without space"
            );
        }
    }
}

#[tokio::test]
async fn part_space_and_progress_are_aggregated_before_the_first_request() {
    let server = Server::start(vec![Reply::full(), Reply::full()]);
    let (_scratch, mut downloader) = server.downloader();
    let second = ModelFile {
        name: "second.bin",
        ..MODEL
    };
    let (_cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    downloader.space = Some(13);
    assert_eq!(
        downloader
            .fetch_files(&[&MODEL, &second], &progress, &receiver)
            .await,
        Err(DownloadError::NoSpace { needed: 14 }),
        "whole part plus ten percent rounded up"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "all space checked before first request"
    );
    downloader.space = Some(14);
    assert_eq!(
        downloader
            .fetch_files(&[&MODEL, &second], &progress, &receiver)
            .await,
        Ok(()),
        "exact aggregate need"
    );
    assert_eq!(
        *progress.borrow(),
        Progress {
            phase: FetchPhase::Downloading,
            done: 12,
            total: 12
        },
        "aggregate progress"
    );
    assert_eq!(
        fs::read(final_path(&downloader).with_file_name("second.bin")).expect("second model"),
        b"abcdef",
        "second file verified"
    );
    downloader.space = Some(0);
    assert_eq!(
        downloader
            .fetch_files(&[&MODEL, &second], &progress, &receiver)
            .await,
        Ok(()),
        "verified files need no download space"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        2,
        "verified part sends nothing more"
    );
}

#[tokio::test]
async fn transient_statuses_keep_partial_without_retry() {
    for (status, expected) in [
        (429, DownloadError::Transport),
        (500, DownloadError::Transport),
        (503, DownloadError::Transport),
        (404, DownloadError::Refused),
        (204, DownloadError::Refused),
    ] {
        let server = Server::start(vec![Reply::raw(&format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))]);
        let (_scratch, downloader) = server.downloader();
        seed(&part_path(&downloader), b"abc");
        assert_eq!(fetch(&downloader).await.0, Err(expected), "status {status}");
        assert_eq!(
            fs::read(part_path(&downloader)).expect("valid voice fixture"),
            b"abc",
            "prefix retained"
        );
        assert_eq!(server.accepted.load(Ordering::SeqCst), 1, "no retry");
    }
}

#[tokio::test]
async fn redirects_five_pass_six_fail_and_never_send_referer() {
    for hops in [5, 6] {
        let captured = Captured::default();
        let _capture = captured.install();
        let mut replies = (0..hops)
            .map(|hop| Reply::redirect(&format!("/hop{hop}?signed={SENTINEL}")))
            .collect::<Vec<_>>();
        replies.push(Reply::full());
        let mut server = Server::start(replies);
        let (_scratch, downloader) = server.downloader();
        assert_eq!(
            fetch(&downloader).await.0,
            if hops == 5 {
                Ok(())
            } else {
                Err(DownloadError::Refused)
            },
            "{hops} hops"
        );
        assert_eq!(
            server.accepted.load(Ordering::SeqCst),
            6,
            "exactly six requests at the redirect bound"
        );
        for _ in 0..6 {
            let request = server
                .requests
                .recv()
                .await
                .expect("valid voice fixture")
                .to_ascii_lowercase();
            assert!(
                !request.contains("referer:"),
                "signed CDN URL never forwarded"
            );
            assert!(
                request.contains("accept-encoding: identity\r\n"),
                "no compression on redirects"
            );
        }
        assert!(
            !captured.text().contains(SENTINEL),
            "signed query never enters diagnostics"
        );
    }
}

#[tokio::test]
async fn redirects_and_first_url_refuse_untrusted_destinations() {
    for location in [
        "https://off-list.invalid/model",
        "http://127.0.0.1/model",
        "https://user:password@127.0.0.1/model",
    ] {
        let server = Server::start(vec![Reply::redirect(location)]);
        let (_scratch, downloader) = server.downloader();
        assert_eq!(
            fetch(&downloader).await.0,
            Err(DownloadError::Refused),
            "{location}"
        );
        assert_eq!(
            server.accepted.load(Ordering::SeqCst),
            1,
            "refused before destination connection"
        );
    }
    let server = Server::start(vec![Reply::full()]);
    let (_scratch, mut downloader) = server.downloader();
    downloader.rules = Rules::Fixture {
        port: server.base.port().expect("valid voice fixture"),
        hosts: vec![],
    };
    assert_eq!(
        fetch(&downloader).await.0,
        Err(DownloadError::Refused),
        "first URL checked too"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "no initial connection"
    );
}

#[test]
fn production_host_policy_and_parser_boundaries() {
    let rules = Rules::default();
    for (url, expected) in [
        ("https://huggingface.co/a", true),
        ("https://cdn.hf.co/a", true),
        ("https://a.b.hf.co:443/a", true),
        ("https://evilhf.co/a", false),
        ("https://hf.co/a", false),
        ("https://huggingface.co.evil.invalid/a", false),
        ("https://127.0.0.1/a", false),
        ("https://[::1]/a", false),
        ("http://huggingface.co/a", false),
        ("https://huggingface.co:444/a", false),
        ("https://user@huggingface.co/a", false),
        ("https://:pass@huggingface.co/a", false),
    ] {
        assert_eq!(
            rules.accepts(&Url::parse(url).expect("valid voice fixture")),
            expected,
            "{url}"
        );
    }
    assert_eq!(Url::parse("").ok(), None, "empty URL");
    assert_eq!(Url::parse("https://").ok(), None, "truncated URL");
    for (value, expected) in [
        ("bytes 3-5/6", true),
        ("bytes 4-5/6", false),
        ("bytes 2-5/6", false),
        ("bytes 3-6/6", false),
        ("bytes 3-5/7", false),
        ("", false),
        ("bytes 3-5/", false),
        ("items 3-5/6", false),
        ("bytes x-5/6", false),
    ] {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CONTENT_RANGE, value.parse().expect("valid voice fixture"));
        assert_eq!(valid_range(&headers, 3, 6), expected, "{value:?}");
        headers.append(CONTENT_RANGE, value.parse().expect("valid voice fixture"));
        assert!(!valid_range(&headers, 3, 6), "duplicate range refused");
    }
    let mut headers = reqwest::header::HeaderMap::new();
    assert!(!valid_range(&headers, 3, 6), "missing range");
    assert!(valid_length(&headers, 6), "missing length allowed");
    for (value, expected) in [
        ("6", true),
        ("7", false),
        ("5", false),
        ("", false),
        ("+6", false),
        ("six", false),
        ("18446744073709551616", false),
    ] {
        headers.insert(CONTENT_LENGTH, value.parse().expect("valid voice fixture"));
        assert_eq!(valid_length(&headers, 6), expected, "{value:?}");
        headers.append(CONTENT_LENGTH, value.parse().expect("valid voice fixture"));
        assert!(!valid_length(&headers, 6), "duplicate length refused");
        headers.clear();
    }
    headers.insert(
        CONTENT_LENGTH,
        "18446744073709551615".parse().expect("numeric fixture"),
    );
    assert!(
        valid_length(&headers, u64::MAX),
        "largest representable length"
    );
    headers.insert(
        CONTENT_LENGTH,
        "18446744073709551616".parse().expect("numeric fixture"),
    );
    assert!(
        !valid_length(&headers, u64::MAX),
        "one over the numeric bound"
    );
    for (expected, matches) in [
        (MODEL.sha256.to_owned(), true),
        (String::new(), false),
        (MODEL.sha256[..63].to_owned(), false),
        (format!("{}0", MODEL.sha256), false),
        (MODEL.sha256.to_uppercase(), false),
        ("g".repeat(64), false),
    ] {
        let mut hash = Sha256::new();
        hash.update(b"abcdef");
        assert_eq!(
            hash_matches(hash, &expected),
            matches,
            "exact lowercase SHA-256 fixture"
        );
    }
}

async fn wait_for_prefix(path: &Path) {
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        if fs::metadata(path).ok().map(|metadata| metadata.len()) == Some(3) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("fixture prefix was not written");
}

#[tokio::test]
async fn cancel_mid_file_keeps_prefix_and_next_fetch_resumes() {
    let server = Server::start(vec![
        Reply::stalled(),
        Reply::raw(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef",
        ),
    ]);
    let (_scratch, downloader) = server.downloader();
    let (cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let future = downloader.fetch_files(&[&MODEL], &progress, &receiver);
    tokio::pin!(future);
    let partial = part_path(&downloader);
    tokio::select! { result = &mut future => panic!("unexpected finish: {result:?}"), () = wait_for_prefix(&partial) => {} }
    cancel.send_replace(true);
    assert_eq!(
        future.await,
        Err(DownloadError::Cancelled),
        "mid-body cancellation"
    );
    assert_eq!(
        fs::read(part_path(&downloader)).expect("valid voice fixture"),
        b"abc",
        "cancel keeps prefix"
    );
    server.release.send(()).expect("valid voice fixture");
    assert_eq!(
        fetch(&downloader).await.0,
        Ok(()),
        "next explicit fetch resumes"
    );
    assert_eq!(
        fs::read(final_path(&downloader)).expect("valid voice fixture"),
        b"abcdef",
        "resumed file verified"
    );
}

#[tokio::test]
async fn cancellation_during_prefix_rehash_and_closed_cancel_channel() {
    let server = Server::start(vec![]);
    let (_scratch, downloader) = server.downloader();
    seed(&part_path(&downloader), &vec![b'x'; 128 * 1024]);
    let mut file = File::open(part_path(&downloader)).expect("valid voice fixture");
    let (cancel, receiver) = watch::channel(false);
    let hash = hash_file(&mut file, &receiver);
    tokio::pin!(hash);
    tokio::select! { biased; result = &mut hash => panic!("rehash should yield: {result:?}"), () = tokio::task::yield_now() => {} }
    cancel.send_replace(true);
    assert_eq!(
        hash.await.err(),
        Some(DownloadError::Cancelled),
        "cancel during rehash"
    );
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "rehash never sends"
    );
    let (cancel, receiver) = watch::channel(false);
    drop(cancel);
    assert_eq!(
        bounded(async { 17 }, &receiver, Instant::now() + IDLE).await,
        Ok(17),
        "closed channel isn't cancellation"
    );
}

#[tokio::test(start_paused = true)]
async fn stalled_body_times_out_at_exactly_thirty_seconds() {
    let spinner = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let server = Server::start(vec![Reply::stalled()]);
    let (_scratch, downloader) = server.downloader();
    let (_cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let future = downloader.fetch_files(&[&MODEL], &progress, &receiver);
    tokio::pin!(future);
    let partial = part_path(&downloader);
    tokio::select! { result = &mut future => panic!("unexpected finish: {result:?}"), () = wait_for_prefix(&partial) => {} }
    let started = Instant::now();
    time::advance(Duration::from_millis(29_999)).await;
    tokio::select! { biased; result = &mut future => panic!("early deadline: {result:?}"), () = tokio::task::yield_now() => {} }
    time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        future.await,
        Err(DownloadError::Timeout),
        "deadline at exactly 30 s"
    );
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(30),
        "no automatic clock advancement"
    );
    spinner.abort();
    assert!(
        spinner
            .await
            .expect_err("fixture spinner aborted")
            .is_cancelled(),
        "spinner stopped"
    );
    assert_eq!(
        fs::read(part_path(&downloader)).expect("valid voice fixture"),
        b"abc",
        "timeout keeps prefix"
    );
}

#[tokio::test(start_paused = true)]
async fn progress_is_coalesced_to_four_updates_per_second() {
    let (sender, mut receiver) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 6,
    });
    let mut reporter = Reporter {
        sender: &sender,
        last: Instant::now(),
        value: Progress {
            phase: FetchPhase::Verifying,
            done: 0,
            total: 6,
        },
    };
    reporter.update(1);
    assert_eq!(
        *receiver.borrow_and_update(),
        Progress {
            phase: FetchPhase::Verifying,
            done: 0,
            total: 6
        },
        "initial bytes coalesced"
    );
    time::advance(Duration::from_millis(249)).await;
    reporter.update(2);
    assert!(
        !receiver.has_changed().expect("valid voice fixture"),
        "not before 250 ms"
    );
    time::advance(Duration::from_millis(1)).await;
    reporter.update(3);
    assert_eq!(
        *receiver.borrow_and_update(),
        Progress {
            phase: FetchPhase::Verifying,
            done: 3,
            total: 6
        },
        "exactly 250 ms"
    );
    reporter.update(6);
    reporter.finish().await;
    assert_eq!(
        *receiver.borrow_and_update(),
        Progress {
            phase: FetchPhase::Verifying,
            done: 6,
            total: 6
        },
        "terminal progress"
    );
    assert_eq!(
        Instant::now() - reporter.last,
        PROGRESS_INTERVAL,
        "terminal update also throttled"
    );
}

#[tokio::test]
async fn construction_makes_no_request_and_bad_roots_fail_closed() {
    let server = Server::start(vec![Reply::full()]);
    let (_scratch, _downloader) = server.downloader();
    assert_eq!(
        server.accepted.load(Ordering::SeqCst),
        0,
        "construction is offline"
    );
    for roots in [vec![], vec![vec![]], vec![b"not a certificate".to_vec()]] {
        assert_eq!(
            Downloader::from_roots(PathBuf::new(), roots, Rules::default()).err(),
            Some(DownloadError::Transport),
            "no usable OS roots"
        );
    }
}

#[tokio::test]
async fn mismatched_length_is_refused_before_waiting_for_body() {
    let server = Server::start(vec![Reply {
        wire: b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n".to_vec(),
        hold: true,
    }]);
    let (_scratch, downloader) = server.downloader();
    assert_eq!(
        fetch(&downloader).await.0,
        Err(DownloadError::Size),
        "headers alone reject the size"
    );
    assert!(!part_path(&downloader).exists(), "bad-size partial removed");
}

#[tokio::test]
async fn cancelled_public_fetch_sends_nothing_and_preserves_partial() {
    fn send_future<T: Send>(value: T) -> T {
        value
    }
    let server = Server::start(vec![]);
    let (_scratch, downloader) = server.downloader();
    seed(&part_path(&downloader), b"abc");
    let (_cancel, receiver) = watch::channel(true);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let future = downloader.fetch(VoicePart::SpeechInput, &progress, &receiver);
    assert_eq!(
        send_future(future).await,
        Err(DownloadError::Cancelled),
        "already cancelled"
    );
    assert_eq!(
        fs::read(part_path(&downloader)).expect("saved prefix"),
        b"abc",
        "partial untouched"
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0, "no request");
}

#[tokio::test(start_paused = true)]
async fn connection_and_tls_handshake_have_a_thirty_second_deadline() {
    let spinner = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let server = Server::start(vec![]);
    let (_scratch, mut downloader) = server.downloader();
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    downloader.base = Some(Url::parse(&format!("https://{address}/")).expect("fixture URL"));
    downloader.rules = Rules::Fixture {
        port: address.port(),
        hosts: vec!["127.0.0.1".into()],
    };
    let (_cancel, receiver) = watch::channel(false);
    let (progress, _) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 0,
    });
    let future = downloader.fetch_files(&[&MODEL], &progress, &receiver);
    tokio::pin!(future);
    let _socket = tokio::select! {
        result = &mut future => panic!("unexpected finish: {result:?}"),
        socket = listener.accept() => socket.expect("fixture connection"),
    };
    let started = Instant::now();
    time::advance(Duration::from_millis(29_999)).await;
    tokio::select! { biased; result = &mut future => panic!("early deadline: {result:?}"), () = tokio::task::yield_now() => {} }
    time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        future.await,
        Err(DownloadError::Timeout),
        "TLS handshake deadline"
    );
    assert_eq!(started.elapsed(), Duration::from_secs(30), "exact deadline");
    spinner.abort();
    assert!(
        spinner
            .await
            .expect_err("fixture spinner aborted")
            .is_cancelled(),
        "spinner stopped"
    );
}

#[test]
fn errors_are_closed_and_content_free() {
    use std::error::Error;
    for (error, text) in [
        (
            DownloadError::NoSpace { needed: 7 },
            "insufficient model space",
        ),
        (DownloadError::Refused, "model request refused"),
        (DownloadError::Size, "model size mismatch"),
        (DownloadError::Hash, "model hash mismatch"),
        (DownloadError::Timeout, "model download timed out"),
        (DownloadError::Transport, "model transport failed"),
        (DownloadError::Io, "model storage failed"),
        (DownloadError::Cancelled, "model download cancelled"),
    ] {
        assert_eq!(error.to_string(), text, "closed diagnostic");
        assert!(error.source().is_none(), "no raw library error");
    }
}

#[tokio::test]
#[ignore = "live HEAD requests require the owner's explicit approval"]
async fn measure_download_head() {
    let downloader = Downloader::new(
        crate::platform::data_dir()
            .expect("valid voice fixture")
            .join(kumochi_protocol::voice::MODELS_DIR),
    )
    .expect("valid voice fixture");
    for model in kumochi_protocol::voice::MODEL_FILES {
        let url = Url::parse(model.url).expect("valid voice fixture");
        assert!(
            downloader.rules.accepts(&url),
            "manifest URL meets production policy"
        );
        let response = downloader
            .client
            .head(url)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .expect("valid voice fixture");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "HEAD succeeded");
        tracing::info!(host = response.url().host_str(), "voice HEAD destination");
    }
}

#[tokio::test(start_paused = true)]
async fn a_phase_change_is_sent_at_once_and_bytes_stay_throttled() {
    let (sender, mut receiver) = watch::channel(Progress {
        phase: FetchPhase::Verifying,
        done: 0,
        total: 6,
    });
    let mut reporter = Reporter {
        sender: &sender,
        last: Instant::now(),
        value: Progress {
            phase: FetchPhase::Verifying,
            done: 0,
            total: 6,
        },
    };
    reporter.phase(FetchPhase::Verifying);
    assert!(
        !receiver.has_changed().expect("valid voice fixture"),
        "the same phase sends nothing"
    );
    time::advance(Duration::from_millis(10)).await;
    reporter.phase(FetchPhase::Downloading);
    assert_eq!(
        *receiver.borrow_and_update(),
        Progress {
            phase: FetchPhase::Downloading,
            done: 0,
            total: 6
        },
        "within 250 ms of the last send"
    );
    time::advance(Duration::from_millis(249)).await;
    reporter.update(2);
    assert!(
        !receiver.has_changed().expect("valid voice fixture"),
        "bytes wait 250 ms from the phase change"
    );
    time::advance(Duration::from_millis(1)).await;
    reporter.update(3);
    assert_eq!(
        *receiver.borrow_and_update(),
        Progress {
            phase: FetchPhase::Downloading,
            done: 3,
            total: 6
        }
    );
}
