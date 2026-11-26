use std::sync::mpsc;
use std::task::{Context, Wake, Waker};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::sync::oneshot;

use super::*;

const TEST_BOUND: Duration = Duration::from_secs(5);

fn by() -> Instant {
    Instant::now() + TEST_BOUND
}

static SHARED_BOUNDARY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct NoDns(mpsc::Sender<bool>);

impl reqwest::dns::Resolve for NoDns {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let _unheard = self.0.send(
            CONNECTING
                .try_with(|attempt| Handle::current().id() == attempt.owner)
                .unwrap_or(false),
        );
        Box::pin(async { Err(io::Error::from(io::ErrorKind::NotFound).into()) })
    }
}

#[tokio::test]
async fn a_real_reqwest_connector_requires_scope_and_runs_off_the_caller() {
    let _shared = SHARED_BOUNDARY.lock().await;
    let (resolved, wait_resolve) = mpsc::channel();
    let tls = trust::config().expect("offline fixture config");
    let client = super::super::http_builder(tls)
        .no_proxy()
        .dns_resolver(Arc::new(NoDns(resolved)))
        .build()
        .expect("configure the real connector without using sockets");

    let request = || client.get("https://localhost/").send();
    assert!(
        request()
            .await
            .expect_err("no connection scope")
            .is_connect()
    );
    assert_eq!(wait_resolve.try_recv(), Err(mpsc::TryRecvError::Empty));
    let (cancel, current) = tokio::sync::watch::channel(true);
    let error = crate::platform::connect_scope(by().into(), Some(&current), request())
        .await
        .expect_err("cancelled download must not start resolution");
    assert!(error.is_connect());
    assert_eq!(wait_resolve.try_recv(), Err(mpsc::TryRecvError::Empty));
    drop(cancel);

    let stop = Arc::new(ConnectCut::default());
    let expired = Instant::now();
    for expired_owner in [true, false] {
        if !expired_owner {
            stop.close();
        }
        let deadline = expired_owner.then_some(expired.into());
        let error = crate::platform::connect_owned_scope(deadline, &stop, async {
            crate::platform::connect_scope(by().into(), None, request()).await
        })
        .await
        .expect_err("an inner HTTP request cannot renew an expired or stopped owner");
        assert!(error.is_connect());
        assert_eq!(wait_resolve.try_recv(), Err(mpsc::TryRecvError::Empty));
    }

    let error = crate::platform::connect_scope(by().into(), None, request())
        .await
        .expect_err("the synthetic resolver refuses without dialing");
    assert!(error.is_connect());
    assert_eq!(wait_resolve.try_recv(), Ok(true));
    assert_eq!(wait_resolve.try_recv(), Err(mpsc::TryRecvError::Empty));
}

#[tokio::test]
async fn either_original_cut_stops_the_direct_worker_without_polling_its_answer() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let _shared = SHARED_BOUNDARY.lock().await;
    for revoked in 0..2 {
        let cuts = [
            Arc::new(ConnectCut::default()),
            Arc::new(ConnectCut::default()),
        ];
        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();
        let (settled, wait_settled) = oneshot::channel::<()>();
        let resumed = Arc::new(AtomicBool::new(false));
        let did_dial = Arc::clone(&resumed);
        let handshake = connect::handshake(
            tokio_rustls::TlsConnector::from(Arc::new(trust::config().expect("offline verifier"))),
            ServerName::try_from("localhost").expect("fixture host"),
            async move {
                let _pending = settled;
                entered.send(()).expect("first dial poll");
                if wait_release.await.is_ok() {
                    did_dial.store(true, Ordering::SeqCst);
                }
                Err::<tokio::io::DuplexStream, _>(io::ErrorKind::ConnectionRefused.into())
            },
        );
        let mut answer = Box::pin(crate::platform::connect_scope(
            by().into(),
            None,
            crate::platform::connect_with_cut(
                Arc::clone(&cuts[0]),
                crate::platform::connect_with_cut(Arc::clone(&cuts[1]), handshake),
            ),
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(answer.as_mut().poll(&mut cx).is_pending());
        tokio::time::timeout(TEST_BOUND, wait_entered)
            .await
            .expect("dial entered")
            .expect("worker reported its first poll");
        cuts[revoked].close();
        let _unheard = release.send(());
        assert!(
            tokio::time::timeout(TEST_BOUND, wait_settled)
                .await
                .expect("the worker settled without its caller")
                .is_err()
        );
        assert!(!resumed.load(Ordering::SeqCst));
        assert_eq!(
            answer
                .await
                .expect_err("a revoked cut cannot hand off")
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
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
async fn dropping_the_owner_keeps_an_admitted_jobs_reactor_alive_until_it_exits() {
    let (reactor_ended, wait_reactor_end) = oneshot::channel();
    let mut reactor_ended = Some(reactor_ended);
    let owner = Executor::with_start(|job| {
        let reactor_ended = reactor_ended.take();
        thread::Builder::new()
            .spawn(move || {
                job();
                if let Some(ended) = reactor_ended {
                    let _unheard = ended.send(());
                }
            })
            .map(drop)
    })
    .expect("executor");
    let (entered, wait_entered) = oneshot::channel();
    let (release, wait_release) = mpsc::channel();
    let (polled, wait_polled) = oneshot::channel();
    let mut answer = Box::pin(owner.connect(Arc::new(Stop::default()), by(), async move {
        entered.send(()).expect("entered a worker poll");
        if wait_release.recv_timeout(TEST_BOUND).is_err() {
            return;
        }
        let mut timer = std::pin::pin!(tokio::time::sleep(TEST_BOUND));
        std::future::poll_fn(|cx| {
            assert!(timer.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let _unheard = polled.send(());
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(answer.as_mut().poll(&mut cx).is_pending());
    tokio::time::timeout(TEST_BOUND, wait_entered)
        .await
        .expect("worker entered within the bound")
        .expect("still in its first poll");
    let reactor = owner.reactor.clone();
    drop(answer);
    drop(owner);
    let heartbeat = reactor.spawn(async { tokio::time::sleep(Duration::from_millis(20)).await });
    assert!(
        tokio::time::timeout(TEST_BOUND, heartbeat)
            .await
            .expect("reactor heartbeat settled")
            .is_ok(),
        "dropping the owner stopped the reactor while a job still owned it"
    );
    release.send(()).expect("resume the running poll");
    tokio::time::timeout(TEST_BOUND, wait_polled)
        .await
        .expect("worker polled its reactor timer")
        .expect("the reactor timer stayed usable");
    tokio::time::timeout(TEST_BOUND, wait_reactor_end)
        .await
        .expect("the last job released the reactor")
        .expect("the reactor closed cleanly");
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
    let _shared = SHARED_BOUNDARY.lock().await;
    let (root, server) = fixture(case, version);
    let anchors = if matches!(case, Peer::UnknownIssuer) {
        vec![]
    } else {
        vec![
            security_framework::certificate::SecCertificate::from_der(&root).expect("fixture root"),
        ]
    };
    let config = trust::fixture_config(anchors, None).expect("offline verifier");
    let (client, peer) = tokio::io::duplex(16 * 1024);
    let peer = tokio::spawn(async move {
        tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(peer)
            .await
    });
    let result = crate::platform::connect_scope(
        by().into(),
        None,
        connect::handshake(
            tokio_rustls::TlsConnector::from(Arc::new(config)),
            ServerName::try_from(name).expect("fixture name"),
            async { Ok(client) },
        ),
    )
    .await;
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
