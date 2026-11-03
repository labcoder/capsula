use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(super) fn random() -> Result<Zeroizing<String>, getrandom::Error> {
    let mut bytes = Zeroizing::new([0; 32]);
    getrandom::fill(bytes.as_mut())?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_ref())))
}

pub(super) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::{challenge, random};

    #[test]
    fn s256_matches_the_rfc_7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_are_unpadded_256_bit_base64url() {
        let first = random().expect("random source");
        let second = random().expect("random source");
        assert_eq!(first.len(), 43);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_ne!(first, second);
    }
}
