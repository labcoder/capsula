use std::path::PathBuf;

use super::*;

fn config() -> PathBuf {
    PathBuf::from(r"C:\Users\someone\AppData\Local\Kumochi\claude-code\config")
}

fn status(logged_in: &str, dir: &str) -> Vec<u8> {
    let method = if logged_in == "true" {
        "claude.ai"
    } else {
        "none"
    };
    signed_in_by(logged_in, method, "firstParty", dir)
}

fn signed_in_by(logged_in: &str, method: &str, provider: &str, dir: &str) -> Vec<u8> {
    format!(
        r#"{{"loggedIn":{logged_in},"authMethod":"{method}","apiProvider":"{provider}","configDirectory":{}}}"#,
        serde_json::to_string(dir).expect("a string")
    )
    .into_bytes()
}

#[test]
fn a_signed_out_status_is_read_with_or_without_its_method_and_provider() {
    let dir = config().to_str().expect("text").to_owned();
    let dir = serde_json::to_string(&dir).expect("a string");
    // Exactly the keys 2.1.306 printed signed out (handoffs/CAPS-158/signin-followups.md).
    let printed = format!(
        r#"{{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty","analyticsDisabled":true,"projectsDirectory":"C:\\Users\\someone\\projects","configDirectory":{dir}}}"#
    );
    assert_eq!(parse_status(printed.as_bytes(), 1, &config()), Ok(false));
    let bare = format!(r#"{{"loggedIn":false,"configDirectory":{dir}}}"#);
    assert_eq!(parse_status(bare.as_bytes(), 1, &config()), Ok(false));
    let signed_in = format!(r#"{{"loggedIn":true,"configDirectory":{dir}}}"#);
    assert_eq!(
        parse_status(signed_in.as_bytes(), 0, &config()),
        Err(StatusFailure::Malformed),
        "signed in, the method and provider are required"
    );
}

#[test]
fn signed_in_counts_only_as_a_claude_ai_sign_in_to_anthropic() {
    let dir = config().to_str().expect("text").to_owned();
    // 2.1.306's answers on Rez (handoffs/CAPS-158/signin-followups.md), and two crossings.
    for (method, provider) in [
        ("api_key", "firstParty"),
        ("oauth_token", "firstParty"),
        ("third_party", "bedrock"),
        ("claude.ai", "bedrock"),
        ("none", "firstParty"),
    ] {
        assert_eq!(
            parse_status(&signed_in_by("true", method, provider, &dir), 0, &config()),
            Err(StatusFailure::Malformed),
            "{method} {provider}"
        );
    }
}

#[test]
fn signed_in_and_out_are_read_from_the_json() {
    let dir = config().to_str().expect("text").to_owned();
    assert_eq!(
        parse_status(&status("false", &dir), 1, &config()),
        Ok(false)
    );
    assert_eq!(parse_status(&status("true", &dir), 0, &config()), Ok(true));
}

#[test]
fn an_exit_other_than_0_or_1_fails_whatever_it_printed() {
    let dir = config().to_str().expect("text").to_owned();
    for exit in [2, 255, u32::MAX] {
        assert_eq!(
            parse_status(&status("true", &dir), exit, &config()),
            Err(StatusFailure::Exit)
        );
    }
}

#[test]
fn anything_but_the_expected_object_is_malformed() {
    let dir = config().to_str().expect("text").to_owned();
    let whole = status("true", &dir);
    let truncated = &whole[..whole.len() - 1];
    for bad in [
        &b""[..],
        b"signed in",
        b"[]",
        b"null",
        truncated,
        br#"{"configDirectory":"C:\\x"}"#,
        br#"{"loggedIn":true}"#,
        br#"{"loggedIn":"true","configDirectory":"C:\\x"}"#,
        br#"{"loggedIn":true,"configDirectory":7}"#,
    ] {
        assert_eq!(
            parse_status(bad, 0, &config()),
            Err(StatusFailure::Malformed),
            "{bad:?}"
        );
    }
}

#[test]
fn only_kumochis_exact_folder_is_accepted() {
    let dir = config().to_str().expect("text").to_owned();
    for other in [
        r"C:\Users\someone\.claude".to_owned(),
        dir.to_lowercase(),
        format!(r"{dir}\"),
        dir.replace('\\', "/"),
        format!(r"{dir}\.."),
        String::new(),
    ] {
        assert_eq!(
            parse_status(&status("true", &other), 0, &config()),
            Err(StatusFailure::OtherFolder),
            "{other}"
        );
    }
}
