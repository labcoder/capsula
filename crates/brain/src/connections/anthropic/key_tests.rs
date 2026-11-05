use kumochi_protocol::Secret;
use kumochi_protocol::message::KeyRejection;
use zeroize::Zeroizing;

use super::AnthropicKey;

#[test]
fn credential_bytes_use_the_shared_format_rule() {
    let parse = |bytes| AnthropicKey::parse(Secret::new(Zeroizing::new(bytes)));
    let key = parse(b" \tsk-ant-api-fake-only\r\n".to_vec()).expect("synthetic API key");
    assert_eq!(key.expose().as_str(), "sk-ant-api-fake-only");
    for (bytes, expected) in [
        (b"sk-ant-api-fake-\xff".to_vec(), KeyRejection::Format),
        (
            b"sk-ant-oat01-fake-token".to_vec(),
            KeyRejection::SubscriptionToken,
        ),
        (b"sk-ant-admin-fake-key".to_vec(), KeyRejection::AdminKey),
        (Vec::new(), KeyRejection::Format),
    ] {
        assert_eq!(parse(bytes).expect_err("refused bytes"), expected);
    }
}

#[test]
fn keys_and_outer_carriers_redact_text_and_bytes() {
    let marker = "sk-ant-api-sentinel-not-real";
    let key = AnthropicKey::parse(Secret::new(Zeroizing::new(marker.as_bytes().to_vec())))
        .expect("synthetic API key");
    let outer = Some(Ok::<_, KeyRejection>(key.clone()));
    for printed in [format!("{:?}", key.expose()), format!("{outer:?}")] {
        assert!(!printed.contains(marker), "key text is redacted");
        assert!(
            !printed.contains(&format!("{:?}", marker.as_bytes())),
            "key byte list is redacted"
        );
    }
    assert_eq!(key.expose().as_str(), marker);
}
