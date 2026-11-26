use std::sync::mpsc;
use std::task::{Context, Wake, Waker};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::sync::oneshot;

use super::*;

const TEST_BOUND: Duration = Duration::from_secs(5);

fn by() -> Instant {
    Instant::now() + TEST_BOUND
}

#[tokio::test]
async fn a_stalled_check_times_out_but_keeps_the_only_native_slot() {
    let owner = Arc::new(Executor::new().expect("executor"));
    let (entered, wait_entered) = oneshot::channel();
    let (release, wait_release) = mpsc::channel();
    let request = Arc::new(Stop::default());
    let deadline = Instant::now() + Duration::from_secs(1);
    let worker = Arc::clone(&owner);
    let connection = tokio::spawn(async move {
        worker
            .connect(request, deadline, async move {
                evaluate(|| {
                    entered.send(()).expect("report native entry");
                    wait_release.recv().expect("release the native call");
                })
            })
            .await
    });
    tokio::time::timeout(TEST_BOUND, wait_entered)
        .await
        .expect("the brain can poll while evaluation is parked")
        .expect("the worker reached evaluation");
    let reactor_timer = {
        let _reactor = owner.reactor.enter();
        tokio::time::sleep(Duration::from_millis(1))
    };
    tokio::time::timeout(TEST_BOUND, reactor_timer)
        .await
        .expect("the connection reactor also runs while evaluation is parked");
    assert_eq!(
        connection.await.expect("caller completed"),
        Err(Refusal::Deadline)
    );

    assert_eq!(
        owner
            .connect(Arc::new(Stop::default()), by(), async {
                panic!("the next connection must not start or queue");
            })
            .await,
        Err::<(), _>(Refusal::Busy)
    );

    release.send(()).expect("release the native call");
    tokio::time::timeout(TEST_BOUND, async {
        while owner.checks.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the slot returns only once the native call finishes");
    assert_eq!(
        owner
            .connect(Arc::new(Stop::default()), by(), async { evaluate(|| 7) })
            .await,
        Ok(Ok(7))
    );
}

#[tokio::test]
async fn cancelling_an_unpolled_answer_stops_new_native_work() {
    let owner = Executor::new().expect("executor");
    let cancel = Arc::new(Stop::default());
    let (entered, wait_entered) = oneshot::channel();
    let (unblock, gate) = mpsc::channel();
    let (checked, wait_checked) = oneshot::channel();
    let mut connection = std::pin::pin!(owner.connect(Arc::clone(&cancel), by(), async move {
        entered.send(()).expect("the attempt was dispatched");
        gate.recv().expect("release the first poll");
        let result = evaluate(|| panic!("a revoked attempt cannot reach the verifier"));
        checked.send(result).expect("report refusal");
    }));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(connection.as_mut().poll(&mut cx).is_pending());
    tokio::time::timeout(TEST_BOUND, wait_entered)
        .await
        .expect("dispatch is bounded")
        .expect("the attempt reached the worker");
    cancel.close();
    unblock.send(()).expect("try native admission");
    assert_eq!(
        tokio::time::timeout(TEST_BOUND, wait_checked)
            .await
            .expect("native admission is bounded")
            .expect("native admission reported"),
        Err::<(), _>(Refusal::Cancelled)
    );
    assert_eq!(connection.await, Err(Refusal::Cancelled));
}

#[derive(Default)]
struct AnswerWake(Notify);

impl Wake for AnswerWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn an_already_ready_answer_does_not_extend_the_callers_deadline() {
    let owner = Executor::new().expect("executor");
    let deadline = Instant::now() + Duration::from_secs(3);
    let (release, gate) = oneshot::channel();
    let mut connection =
        std::pin::pin!(owner.connect(Arc::new(Stop::default()), deadline, async {
            gate.await.expect("release the answer after the first poll");
            evaluate(|| 7)
        }));
    let wake = Arc::new(AnswerWake::default());
    let waker = Waker::from(Arc::clone(&wake));
    let mut cx = Context::from_waker(&waker);
    assert!(connection.as_mut().poll(&mut cx).is_pending());
    release.send(()).expect("worker can complete");
    tokio::time::timeout_at(deadline.into(), wake.0.notified())
        .await
        .expect("the result woke its receiver before its deadline");
    assert!(
        Instant::now() < deadline,
        "the worker finished within the caller's bound"
    );
    tokio::time::sleep_until(deadline.into()).await;
    assert_eq!(connection.await, Err(Refusal::Deadline));
}

#[tokio::test]
async fn thread_start_refusal_closes_every_prestarted_lane_without_joining() {
    for fail_at in 0..=MAX_CONNECTS {
        let mut started = Vec::new();
        let mut attempts = 0;
        let result = Executor::with_start(|job| {
            let index = attempts;
            attempts += 1;
            if index == fail_at {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let (done, settled) = oneshot::channel();
            thread::Builder::new()
                .spawn(move || {
                    job();
                    done.send(()).expect("report executor lane closed");
                })
                .map(drop)?;
            started.push(settled);
            Ok(())
        });
        assert!(matches!(result, Err(Refusal::Unavailable)));
        assert_eq!(attempts, fail_at + 1);
        assert_eq!(started.len(), fail_at);
        for lane in started {
            tokio::time::timeout(TEST_BOUND, lane)
                .await
                .expect("an executor lane did not close")
                .expect("the executor lane closed cleanly");
        }
    }
}

#[tokio::test]
async fn a_spent_deadline_and_missing_worker_refuse_before_evaluation() {
    let owner = Executor::new().expect("executor");
    assert_eq!(
        owner
            .connect(Arc::new(Stop::default()), Instant::now(), async {
                panic!("an already spent connection cannot get scheduled");
            })
            .await,
        Err::<(), _>(Refusal::Deadline)
    );
    assert_eq!(
        evaluate(|| panic!("there is no worker authority on the brain")),
        Err::<(), _>(Refusal::Unavailable)
    );
    let stopped = Arc::new(Stop::default());
    stopped.close();
    assert_eq!(
        owner
            .connect(stopped, by(), async {
                panic!("no dispatch after cancellation")
            })
            .await,
        Err::<(), _>(Refusal::Cancelled)
    );
}

enum End {
    DropAnswer,
    Cancel,
    Deadline,
}

async fn recover_capacity(end: End) {
    let owner = Executor::new().expect("executor");
    let cancel = Arc::new(Stop::default());
    let deadline = if matches!(end, End::Deadline) {
        Instant::now() + Duration::from_secs(3)
    } else {
        Instant::now() + CONNECT_CAP
    };
    let (entered, mut wait_entered) = tokio::sync::mpsc::channel(MAX_CONNECTS);
    let mut answers = Vec::new();
    let mut settlements = Vec::new();
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    for _ in 0..MAX_CONNECTS {
        let entered = entered.clone();
        let (pending, settled) = oneshot::channel::<()>();
        let mut answer = Box::pin(owner.connect(Arc::clone(&cancel), deadline, async move {
            let _pending = pending;
            entered.send(()).await.expect("report entry");
            std::future::pending::<()>().await;
        }));
        assert!(answer.as_mut().poll(&mut cx).is_pending());
        answers.push(answer);
        settlements.push(settled);
    }
    for _ in 0..MAX_CONNECTS {
        assert_eq!(
            tokio::time::timeout(TEST_BOUND, wait_entered.recv())
                .await
                .expect("connect started"),
            Some(())
        );
    }
    assert_eq!(
        owner
            .connect(Arc::new(Stop::default()), by(), async {
                panic!("a full executor has no pending queue");
            })
            .await,
        Err::<(), _>(Refusal::Busy)
    );
    match end {
        End::DropAnswer => answers.clear(),
        End::Cancel => cancel.close(),
        End::Deadline => {}
    }
    for worker in settlements {
        assert!(
            tokio::time::timeout(TEST_BOUND, worker)
                .await
                .expect("the worker settled without polling its answer")
                .is_err()
        );
    }
    let fresh = tokio::time::timeout(TEST_BOUND, async {
        loop {
            match owner
                .connect(Arc::new(Stop::default()), by(), async { 7 })
                .await
            {
                Err(Refusal::Busy) => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await;
    assert_eq!(fresh.expect("a settled worker frees its slot"), Ok(7));
    drop(answers);
}

#[tokio::test]
async fn full_capacity_is_recovered_without_polling_parked_answers() {
    for end in [End::DropAnswer, End::Cancel, End::Deadline] {
        recover_capacity(end).await;
    }
}

#[derive(Clone, Copy)]
enum Peer {
    Valid,
    Expired,
    UnknownIssuer,
    WrongKey,
}

fn fixture(
    case: Peer,
    version: &'static rustls::SupportedProtocolVersion,
) -> (CertificateDer<'static>, rustls::ServerConfig) {
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };

    let ca_key = KeyPair::generate().expect("fixture CA key");
    let mut ca = CertificateParams::new(Vec::new()).expect("CA parameters");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let root = ca.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = KeyPair::generate().expect("fixture leaf key");
    let mut leaf = CertificateParams::new(vec!["localhost".into()]).expect("leaf parameters");
    let now = time::OffsetDateTime::now_utc();
    leaf.not_before = now - time::Duration::days(2);
    leaf.not_after = now
        + time::Duration::days(if matches!(case, Peer::Expired) {
            -1
        } else {
            30
        });
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf
        .signed_by(&leaf_key, &Issuer::from_params(&ca, &ca_key))
        .expect("CA-signed leaf");
    let private = if matches!(case, Peer::WrongKey) {
        KeyPair::generate().expect("unrelated key")
    } else {
        leaf_key
    };
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        private.serialize_der().into(),
    ))
    .expect("fixture signing key");
    let certified = rustls::sign::CertifiedKey::new(vec![leaf.der().clone()], signing_key);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[version])
        .expect("fixture protocols")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
    (root.der().clone(), server)
}

async fn handshake(
    name: &'static str,
    case: Peer,
    version: &'static rustls::SupportedProtocolVersion,
) -> Result<(), rustls::Error> {
    let (root, server) = fixture(case, version);
    let anchors = if matches!(case, Peer::UnknownIssuer) {
        vec![]
    } else {
        vec![
            security_framework::certificate::SecCertificate::from_der(&root).expect("fixture root"),
        ]
    };
    let config = trust::config(anchors).expect("offline verifier");
    let (client, peer) = tokio::io::duplex(16 * 1024);
    let peer = tokio::spawn(async move {
        tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(peer)
            .await
    });
    let result = Executor::new()
        .expect("executor")
        .connect(Arc::new(Stop::default()), by(), async move {
            tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(ServerName::try_from(name).expect("fixture name"), client)
                .await
        })
        .await
        .expect("the handshake completed within the boundary");
    if let Err(error) = result {
        peer.abort();
        return Err(error
            .into_inner()
            .expect("TLS error")
            .downcast::<rustls::Error>()
            .map(|error| *error)
            .expect("rustls proof error"));
    }
    drop(result);
    assert!(peer.await.expect("peer settled").is_ok());
    Ok(())
}

#[tokio::test]
async fn tls_requires_the_leaf_name_native_chain_and_handshake_key() {
    use rustls::{CertificateError, Error};

    let tls13 = &rustls::version::TLS13;
    assert_eq!(handshake("localhost", Peer::Valid, tls13).await, Ok(()));
    assert_eq!(
        handshake("wrong.invalid", Peer::Valid, tls13).await,
        Err(Error::InvalidCertificate(CertificateError::NotValidForName))
    );
    for case in [Peer::UnknownIssuer, Peer::Expired] {
        assert_eq!(
            handshake("localhost", case, tls13).await,
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure
            ))
        );
    }
    for version in [&rustls::version::TLS12, tls13] {
        assert_eq!(
            handshake("localhost", Peer::WrongKey, version).await,
            Err(Error::InvalidCertificate(CertificateError::BadSignature))
        );
    }
}
