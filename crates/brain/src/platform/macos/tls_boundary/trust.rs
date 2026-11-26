use std::sync::Arc;

use core_foundation::date::CFDate;
use objc2_security::errSecCertificateRevoked;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{Resumption, verify_server_name};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use security_framework::certificate::SecCertificate;
use security_framework::policy::{RevocationPolicy, SecPolicy};
use security_framework::secure_transport::SslProtocolSide;
use security_framework::trust::SecTrust;
use x509_parser::der_parser::der::{parse_der_sequence_of_v, parse_der_u32};
use x509_parser::der_parser::oid;

const REVOCATION: RevocationPolicy = RevocationPolicy::USE_ANY_METHOD_AVAILABLE;

#[cfg(test)]
#[path = "revocation_tests.rs"]
mod revocation_tests;

pub(in crate::platform) fn config() -> Result<ClientConfig, Error> {
    build(
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
pub(super) fn fixture_config(
    anchors: Vec<SecCertificate>,
    policy: Option<RevocationPolicy>,
) -> Result<ClientConfig, Error> {
    build(Some(Fixture { anchors, policy }))
}

#[cfg(test)]
struct Fixture {
    anchors: Vec<SecCertificate>,
    policy: Option<RevocationPolicy>,
}

fn build(#[cfg(test)] fixture: Option<Fixture>) -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Verifier {
        signatures: provider.signature_verification_algorithms,
        #[cfg(test)]
        fixture,
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.resumption = Resumption::disabled();
    config.enable_early_data = false;
    Ok(config)
}

struct Verifier {
    signatures: WebPkiSupportedAlgorithms,
    #[cfg(test)]
    fixture: Option<Fixture>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacTrust")
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        staple: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        super::evaluate(|| {
            let parsed =
                ParsedCertificate::try_from(leaf).map_err(|_| CertificateError::BadEncoding)?;
            verify_server_name(&parsed, name).map_err(|_| CertificateError::NotValidForName)?;
            if must_staple(leaf)? {
                // shortcut: SecTrust cannot attest the staple itself; refuse until it can be authenticated.
                return Err(CertificateError::UnknownRevocationStatus.into());
            }
            let name = name.to_str();
            let peer = std::iter::once(leaf)
                .chain(chain)
                .map(|der| SecCertificate::from_der(der).map_err(|_| CertificateError::BadEncoding))
                .collect::<Result<Vec<_>, _>>()?;
            let ssl = SecPolicy::create_ssl(SslProtocolSide::SERVER, Some(&name));
            let flags = REVOCATION;
            #[cfg(test)]
            let flags = self
                .fixture
                .as_ref()
                .and_then(|fixture| fixture.policy)
                .unwrap_or(flags)
                .union(RevocationPolicy::NETWORK_ACCESS_DISABLED);
            let revocation = SecPolicy::create_revocation(flags).map_err(|_| refused())?;
            let mut trust = SecTrust::create_with_certificates(&peer, &[ssl, revocation])
                .map_err(|_| refused())?;
            trust
                .set_network_fetch_allowed(!cfg!(test))
                .map_err(|_| refused())?;
            trust
                .set_trust_verify_date(&verify_date(now)?)
                .map_err(|_| refused())?;
            if !staple.is_empty() {
                trust
                    .set_trust_ocsp_response(std::iter::once(staple))
                    .map_err(|_| refused())?;
            }
            #[cfg(test)]
            if let Some(fixture) = &self.fixture {
                trust
                    .set_anchor_certificates(&fixture.anchors)
                    .map_err(|_| refused())?;
                trust
                    .set_trust_anchor_certificates_only(true)
                    .map_err(|_| refused())?;
            }
            trust.evaluate_with_error().map_err(|error| {
                if i32::try_from(error.code()) == Ok(errSecCertificateRevoked) {
                    CertificateError::Revoked
                } else {
                    refused()
                }
            })?;
            Ok(ServerCertVerified::assertion())
        })
        .map_err(|_| refused())?
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, signature, &self.signatures)
            .map_err(|_| CertificateError::BadSignature.into())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, signature, &self.signatures)
            .map_err(|_| CertificateError::BadSignature.into())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signatures.supported_schemes()
    }
}

fn must_staple(leaf: &CertificateDer<'_>) -> Result<bool, CertificateError> {
    let (rest, cert) =
        x509_parser::parse_x509_certificate(leaf).map_err(|_| CertificateError::BadEncoding)?;
    if !rest.is_empty() {
        return Err(CertificateError::BadEncoding);
    }
    let Some(feature) = cert
        .get_extension_unique(&oid!(1.3.6.1.5.5.7.1.24))
        .map_err(|_| CertificateError::BadEncoding)?
    else {
        return Ok(false);
    };
    let (rest, features) = parse_der_sequence_of_v(parse_der_u32)(feature.value)
        .map_err(|_| CertificateError::BadEncoding)?;
    if !rest.is_empty()
        || features.is_empty()
        || features.iter().any(|value| u16::try_from(*value).is_err())
    {
        return Err(CertificateError::BadEncoding);
    }
    Ok(features.iter().any(|feature| matches!(feature, 5 | 17)))
}

fn refused() -> CertificateError {
    CertificateError::ApplicationVerificationFailure
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "the bounded whole seconds and the epoch subtraction are exact in CFAbsoluteTime"
)]
fn verify_date(now: UnixTime) -> Result<CFDate, CertificateError> {
    let seconds = now.as_secs();
    if seconds > 1 << 53 {
        return Err(refused());
    }
    Ok(CFDate::new(seconds as f64 - 978_307_200.0))
}
