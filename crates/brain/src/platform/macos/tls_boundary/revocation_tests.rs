use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair};
use ring::{digest, rand, signature};
use yasna::Tag;
use yasna::models::{GeneralizedTime, ObjectIdentifier};

use super::*;
use crate::platform::tls_control::ConnectCut;

const SERIAL: u64 = 42;
const FEATURE: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 1, 24];

struct Peer {
    leaf: CertificateDer<'static>,
    issuer: CertificateDer<'static>,
    key: KeyPair,
}

fn peer(feature: Option<&[u8]>) -> Peer {
    let key = KeyPair::generate().expect("issuer key");
    let mut issuer = CertificateParams::new(Vec::new()).expect("issuer params");
    issuer.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let anchor = issuer.self_signed(&key).expect("issuer");
    let mut leaf = CertificateParams::new(vec!["localhost".into()]).expect("leaf params");
    let now = time::OffsetDateTime::now_utc();
    leaf.not_before = now - time::Duration::days(1);
    leaf.not_after = now + time::Duration::days(30);
    leaf.serial_number = Some(SERIAL.into());
    leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    if let Some(feature) = feature {
        leaf.custom_extensions
            .push(CustomExtension::from_oid_content(FEATURE, feature.to_vec()));
    }
    let leaf = leaf
        .signed_by(
            &KeyPair::generate().expect("leaf key"),
            &Issuer::from_params(&issuer, &key),
        )
        .expect("issued leaf");
    Peer {
        leaf: leaf.der().clone(),
        issuer: anchor.der().clone(),
        key,
    }
}

async fn verify(
    peer: Peer,
    staple: Vec<u8>,
    policy: Option<RevocationPolicy>,
) -> Result<(), Error> {
    let signatures = rustls::crypto::ring::default_provider().signature_verification_algorithms;
    let issuer = SecCertificate::from_der(&peer.issuer).expect("issued trust anchor");
    let native = Verifier {
        signatures,
        fixture: Some(Fixture {
            anchors: vec![issuer],
            policy,
            observe: None,
        }),
    };
    let owner = super::super::Executor::new().expect("executor");
    owner
        .connect(
            Arc::new(ConnectCut::default()),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            async move {
                native
                    .verify_server_cert(
                        &peer.leaf,
                        &[],
                        &ServerName::try_from("localhost").expect("fixture hostname"),
                        &staple,
                        UnixTime::now(),
                    )
                    .map(|_| ())
            },
        )
        .await
        .expect("offline verification finished")
}

#[tokio::test]
async fn stopping_after_preparation_refuses_native_evaluation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::task::{Context, Waker};
    use std::time::{Duration, Instant};

    use tokio::sync::oneshot;

    use super::super::{Executor, Refusal};

    const BOUND: Duration = Duration::from_secs(5);
    for cancel in [true, false] {
        let peer = peer(None);
        let issuer = SecCertificate::from_der(&peer.issuer).expect("fixture root");
        let (prepared, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let (release, parked) = mpsc::channel();
        let parked = Mutex::new(parked);
        let (verdict, done) = oneshot::channel();
        let admitted = Arc::new(AtomicBool::new(false));
        let entered = Arc::clone(&admitted);
        let native = Verifier {
            signatures: rustls::crypto::ring::default_provider().signature_verification_algorithms,
            fixture: Some(Fixture {
                anchors: vec![issuer],
                policy: None,
                observe: Some(Box::new(move |point| match point {
                    EvaluationPoint::Prepared => {
                        prepared.send(()).expect("prepared trust witness");
                        parked
                            .lock()
                            .expect("test gate")
                            .recv_timeout(BOUND)
                            .expect("release");
                    }
                    EvaluationPoint::Admitted => entered.store(true, Ordering::SeqCst),
                })),
            }),
        };
        let executor = Executor::new().expect("executor");
        let stop = Arc::new(ConnectCut::default());
        let deadline = Instant::now()
            + if cancel {
                BOUND
            } else {
                Duration::from_secs(1)
            };
        let answer = executor.connect(Arc::clone(&stop), deadline, async move {
            let result = native
                .verify_server_cert(
                    &peer.leaf,
                    &[],
                    &ServerName::try_from("localhost").expect("fixture host"),
                    &[],
                    UnixTime::now(),
                )
                .map(|_| ());
            verdict.send(result).expect("worker verdict");
        });
        tokio::pin!(answer);
        assert!(
            answer
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(
            tokio::time::timeout(BOUND, ready.recv())
                .await
                .expect("prepared"),
            Some(())
        );
        let expected = if cancel {
            stop.close();
            Refusal::Cancelled
        } else {
            tokio::time::sleep_until(deadline.into()).await;
            Refusal::Deadline
        };
        release.send(()).expect("prepared worker remains parked");
        assert_eq!(
            tokio::time::timeout(BOUND, done)
                .await
                .expect("worker settles")
                .expect("reported verdict"),
            Err(Error::InvalidCertificate(refused()))
        );
        assert!(!admitted.load(Ordering::SeqCst));
        assert_eq!(answer.await, Err(expected));
    }
}

#[tokio::test]
async fn soft_fail_accepts_no_status_but_refuses_a_verified_revoked_answer() {
    let ordinary = peer(None);
    let status = revoked(&ordinary);
    assert_eq!(verify(peer(None), vec![], None).await, Ok(()));
    assert_eq!(
        verify(ordinary, status, None).await,
        Err(Error::InvalidCertificate(CertificateError::Revoked))
    );
}

#[tokio::test]
async fn a_required_native_status_and_an_unauthenticated_staple_fail_closed() {
    assert_eq!(
        verify(
            peer(None),
            vec![],
            Some(REVOCATION.union(RevocationPolicy::REQUIRE_POSITIVE_RESPONSE)),
        )
        .await,
        Err(Error::InvalidCertificate(refused()))
    );
    for response in [vec![], vec![0x30, 0x00]] {
        assert_eq!(
            verify(peer(Some(&[0x30, 0x03, 0x02, 0x01, 0x05])), response, None).await,
            Err(Error::InvalidCertificate(
                CertificateError::UnknownRevocationStatus
            ))
        );
    }
}

#[test]
fn distinguishes_status_requests_from_other_and_malformed_tls_features() {
    for (value, decision) in [
        (&[0x30, 0x03, 0x02, 0x01, 0x11][..], Ok(true)),
        (&[0x30, 0x03, 0x02, 0x01, 0x07][..], Ok(false)),
        (&[0x30, 0x00][..], Err(CertificateError::BadEncoding)),
        (
            &[0x30, 0x03, 0x02, 0x01][..],
            Err(CertificateError::BadEncoding),
        ),
    ] {
        assert_eq!(must_staple(&peer(Some(value)).leaf), decision);
    }
}

fn revoked(peer: &Peer) -> Vec<u8> {
    let (_, issuer) = x509_parser::parse_x509_certificate(&peer.issuer).expect("fixture issuer");
    let hash = |data| digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let name = hash(issuer.subject().as_raw());
    let key = hash(&issuer.public_key().subject_public_key.data);
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("whole seconds");
    let earlier = GeneralizedTime::from_datetime(now - time::Duration::minutes(1));
    let later = GeneralizedTime::from_datetime(now + time::Duration::hours(1));
    let tbs = yasna::construct_der(|der| {
        der.write_sequence(|data| {
            data.next()
                .write_tagged(Tag::context(2), |id| id.write_bytes(key.as_ref()));
            data.next().write_generalized_time(&earlier);
            data.next().write_sequence(|responses| {
                responses.next().write_sequence(|single| {
                    single.next().write_sequence(|id| {
                        id.next().write_sequence(|algorithm| {
                            algorithm
                                .next()
                                .write_oid(&ObjectIdentifier::from_slice(&[1, 3, 14, 3, 2, 26]));
                            algorithm.next().write_null();
                        });
                        id.next().write_bytes(name.as_ref());
                        id.next().write_bytes(key.as_ref());
                        id.next().write_u64(SERIAL);
                    });
                    single
                        .next()
                        .write_tagged_implicit(Tag::context(1), |status| {
                            status.write_sequence(|revoked| {
                                revoked.next().write_generalized_time(&earlier);
                            });
                        });
                    single.next().write_generalized_time(&earlier);
                    single
                        .next()
                        .write_tagged(Tag::context(0), |time| time.write_generalized_time(&later));
                });
            });
        });
    });
    let random = rand::SystemRandom::new();
    let signing_key = signature::EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        &peer.key.serialize_der(),
        &random,
    )
    .expect("issuer signing key");
    let proof = signing_key
        .sign(&random, &tbs)
        .expect("signed fixture status");
    let basic = yasna::construct_der(|der| {
        der.write_sequence(|response| {
            response.next().write_der(&tbs);
            response.next().write_sequence(|algorithm| {
                algorithm
                    .next()
                    .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 10045, 4, 3, 2]));
            });
            response
                .next()
                .write_bitvec_bytes(proof.as_ref(), 8 * proof.as_ref().len());
        });
    });
    yasna::construct_der(|der| {
        der.write_sequence(|response| {
            response.next().write_enum(0);
            response.next().write_tagged(Tag::context(0), |tag| {
                tag.write_sequence(|bytes| {
                    bytes.next().write_oid(&ObjectIdentifier::from_slice(&[
                        1, 3, 6, 1, 5, 5, 7, 48, 1, 1,
                    ]));
                    bytes.next().write_bytes(&basic);
                });
            });
        });
    })
}
