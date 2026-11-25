use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use kumochi_protocol::Secret;
use ring::hmac;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use rustls::SignatureScheme;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::PrivateKeyDer;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{
    IdentityError, KeySet, VerifiedIdentity, check_subject, permits_plan, verify, verify_refreshed,
    verify_signed,
};
use crate::connections::clock::{Clock, FixedClock};
use crate::connections::http_client::{
    Endpoint, Http, HttpError, ISSUER, MAX_BODY_BYTES, Request, Response,
};
use crate::connections::records::MAX_TOKEN_BYTES;

const CLIENT: &str = "app_test_client";
const NONCE: &str = "test-nonce";
const SUBJECT: &str = "user-test-subject";
const EMAIL: &str = "person@example.invalid";
const KID: &str = "test-key";
const NOW: u64 = 1_800_000_000;
const JWKS_URI: &str = "https://auth.openai.com/.well-known/jwks.json";

/// Throwaway RSA-2048 keys generated offline with openssl, never used outside these tests.
struct TestKey {
    pkcs8: &'static str,
    n: &'static str,
    e: &'static str,
}

const SIGNER: TestKey = TestKey {
    pkcs8: "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCoDENkOJwxo20hlWLtta/t1ET7\
        cvQxQNpjubIr3LvAG5QLGbWcHnJU7py1qFOU5oSmuMgd7pnu+asFWU/+9CgUO9CibMr2AHTxfG2T\
        myAnielLfmd2X6KJABcHm75TpuHkzx4NSOkSvCslZ+Q3KkgcvphGDsdcYMm9DCkDoHGa7AbHgTm5\
        XElWmo8on0TjZPAQ3YhxPerNQUByjBpiwhZ1eZBhwsKPstkFEk0F9KOz5wnp7JRQM/Hn3B4NZSwq\
        1ioYW0JSjLoEvkK+DBFlERSR3iPNQgCvFJL2vD0RwNpGL2F+LM+s42w9Eh/O1S8ExBuksBD/iJwP\
        NTVWctXb9dBfAgMBAAECggEAA8VGq9SDWK/IjpTaA+hsOqNlKtkw9nv0ioFLqKVKxyw3PaqFgyvB\
        t/skpATTX0du2YjS4K0OYijS+AtBesCCt7WB4GsdgLiQ5yw6DzJPXeO/ytRawPKzZGyOl7qcYicf\
        8XDeRUEKDcQQGcZDfiNiO9bg9s11FrMLQ8UmlHDAq8+vpmclgw8tyNidyYahU3qNfhfAbTEfphHQ\
        UDSirdtAGqMxK1Z1fu/hGE9VpLKGKydttcEsm8WUs7P7XhOJL3yu9xoEsAxZC6XZ2BfmWnw39tG4\
        uZKZaeH4gJEppzI40fmbvgf3y7PkhSktBvWpMFx8XHQqsw98cePpEtDbvkDETQKBgQDQdnZ0tuEP\
        l8I+UW77t0DUMhMsxYYCK3i/wrKYWGge1BFPBtELJqY2soSrjrBW8m8kOtmJdS801p4OCDg0Fpdp\
        37RZOcAMoMdU/DONOYj1FRuuqdHN8NT5UWta5cQxHi7slUDa6nuR/sQrpp1tJ3RZW5orSEtvwL0+\
        /+dTKDZM+wKBgQDOXnuCCTzXYUni9vl5HYnepvOzglz5ZecTcRlVChpB0AMOKt93RrAvvO9NuQs4\
        tE6/nP03x7+hE4+GTfZYm7XWR/mNmcta9Zv3xvBgPYmckJLWDjrXhdz/e2siACNOVXOqrfSajQOf\
        6exuK8EAcjcsZ3We0kcYZQnbOkRiY5Pk7QKBgHPcs5/RGSu6dEqApkefJZgKsN+cdQv1r2odQ3gr\
        AsvHoEslBcDF3XHfLJ/Rca8RlbY0Qz4D9JXQhw5TgTFqMQO2Qx2oXFZocgGJHvnqNXXKIpWkayik\
        JyrfFuTxeWa3WOtDVUWXN9EAIXYFZ9AIMsGLiRdvWgYlwMkoICD2459LAoGAc2qaLr7V3StXNG2m\
        VttzWSgMUhANyZn5Y4E0hsAQsVAR9WF6+214aiqDt/dO1FbbWAUIBaNchEy5T4bYyPPE/ePhKRWj\
        iuqkYS5ND7Y6+WpOBROOSy1dzTEMWMa0+EGIYCPMEofdSBl/Fpq35XT7cIUoXjwCcR1JJufuiQxi\
        EX0CgYAQ1aanCwGyuJUdbm8H5cTJJd/C08cSt5LptOLG/DxFJ6FK4qgSj08mC9L9/kAQZ4hb1X9c\
        uB3Ocv/LAADpYtVs2Agz8egJwYmeEWHT8bTUYn1YSscjwN+UpDjSRzzZaNvvw7bWu5g0JziYKj5F\
        0I7jXNlwqlH2SXNTDs/mp0S17g==",
    n: "qAxDZDicMaNtIZVi7bWv7dRE-3L0MUDaY7myK9y7wBuUCxm1nB5yVO6ctahTlOaEprjIHe6Z7vmr\
        BVlP_vQoFDvQomzK9gB08Xxtk5sgJ4npS35ndl-iiQAXB5u-U6bh5M8eDUjpErwrJWfkNypIHL6Y\
        Rg7HXGDJvQwpA6BxmuwGx4E5uVxJVpqPKJ9E42TwEN2IcT3qzUFAcowaYsIWdXmQYcLCj7LZBRJN\
        BfSjs-cJ6eyUUDPx59weDWUsKtYqGFtCUoy6BL5CvgwRZREUkd4jzUIArxSS9rw9EcDaRi9hfizP\
        rONsPRIfztUvBMQbpLAQ_4icDzU1VnLV2_XQXw",
    e: "AQAB",
};

const OTHER: TestKey = TestKey {
    pkcs8: "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCuMeD8ROFrQfXnouXPptxLYzDa\
        7mte6hErWm6wXONLkmPU7LfAZoXOynbb4B9KEgga/3NipK4F+dlKef6MjNiO7MCNfuBgr/o6briv\
        V7XSgV5DffYvwGklnF4ajRd+eaWPygJJYHoMDI6zqxEw8E2EDGMKG5T0b/pJMT7LCJve68iAWWu+\
        +y99vZjs8eG73jW3/8rgrgboQMGWsSNx6r2UDGD7gKepYKJmgi1+0AnFUd36/hg4T0Hh8Hct9Izv\
        IajISIG6wLdfu5cKf5K3AG/+XMOwg3kV4Cv74NEVi1z5LZm5ezdWBk6y6hxEC0GgmPb9irxamLNU\
        DOfJZb9wvKhjAgMBAAECggEAHIGzJPWMwkXAHVEuNKZE5N1tsFZWIelCOAsLlDcPR13EkqLOi2D7\
        wtSlUORDmpaK1wg49MqoY/f4VFTUkGG4SuN5u24pAAmsdIWMGb3L1pAvQfJKKJRfYLVPPR8b2inE\
        K1zOtP1h27juXX7ASHb66IIybwQGg14T4oh5kzZsc7VyweYnBceD0ExxTE1LpS/wMCij3js5Bfl7\
        so6ZGd2FzRpOSX5y381wIFRCjxe+Gy5g11R3whLbAc8H6r5RoLt7Zz3ILead4qHL4Wq5CnY73cDU\
        VcqoUzwy0L0MNZoLf6kOUtl7PqCBRlOdeMPIA9DD/SPz5IkDykb4OOv9BYaBmQKBgQDsfzInF4g3\
        eTMmsFoa5nITNg2D+meJfHuZoDHij0dnJm2VDPzLqhxfAeRBTzTGe2JevkqdbaZDK5rAhQJH93a8\
        g+Bug1I3XUnbGPcE4fzvjmVFFN46vvl7AaI6L71zCWO58cgfRYBWhPdd1+sSMf1vNBbxovXqeLOB\
        jddytQT/FQKBgQC8j2TIGthaJkk+ltpfrqT7nzbmqTD0Hn4awxlc5aF1uuIZZJRqfekbg0GM6h2D\
        hfrDS8FenN0l16G+itFF346QEOpqieGy/SEE7mRhgIh4fTi6Smr5sYVf0qX4JnXttw+9jbORs2QB\
        a8XTiauKXEfxuMr2XOkGUFC5CdSrE7YnlwKBgQDnlPpYe1ug7qbfGMtzEfvcwg5kEV03rVUfL1YK\
        A8GoyWF/FhOpn69efWy1Y0lLnuDJ488O76NinR1Rri14qWN04DJyJVfp7Qj4ai7hciYLssWJ6xBe\
        OEn2YLgqADuknSoxmAgeTyGIF3ZfR6Lh01/XeAuLnMbwTjeBApNZuNbmrQKBgQCEg113R25uhGOL\
        CebzwT+O2chyd8rAdMF4u02SvIoou6pz3/xxjoUI4chqaIlPShP9tbpybFj6ZDz2NIZ3zXFaC5lC\
        QB4OdNgEqCiBYRwHWTFJskzVp40s11PuPdE4uyLjurbMmT6ZxQyk6iZUuR+SX6S7WHYnCsCAGcmM\
        ZPDphwKBgDnurQZoVHeV9IRTJd8dUI2b5I5c/6as0Su8csPOj/kMmljKve9G6baEbgmYNxy8aSyC\
        FIj112x5smuNVC3tAfAuw8AiXGKlPE8LELmGUbxIrrUkEZOnxCc6bPwEX91IgqfHosgB23qIT02/\
        4TdvYpArlKnlwbP2IjWdWPkrdFmN",
    n: "rjHg_ETha0H156Llz6bcS2Mw2u5rXuoRK1pusFzjS5Jj1Oy3wGaFzsp22-AfShIIGv9zYqSuBfnZ\
        Snn-jIzYjuzAjX7gYK_6Om64r1e10oFeQ332L8BpJZxeGo0Xfnmlj8oCSWB6DAyOs6sRMPBNhAxj\
        ChuU9G_6STE-ywib3uvIgFlrvvsvfb2Y7PHhu941t__K4K4G6EDBlrEjceq9lAxg-4CnqWCiZoIt\
        ftAJxVHd-v4YOE9B4fB3LfSM7yGoyEiBusC3X7uXCn-StwBv_lzDsIN5FeAr--DRFYtc-S2ZuXs3\
        VgZOsuocRAtBoJj2_Yq8WpizVAznyWW_cLyoYw",
    e: "AQAB",
};

/// A 2047-bit modulus: one bit under the minimum, in the same 256 bytes as a 2048-bit one. ring
/// can't load its private key to sign, so the token, with the valid header and claims, was
/// signed offline with openssl by that throwaway key.
const SHORT_N: &str = "ZN1O4H9HXmc9p2VTEmTdF_pgtud6CbD55JVkQvqEETrVwhxm7jSI5TkCgOdzukuUsphhvrX_zb7vWBkFBjkXQG0P4Ux9\
    FJgexPFfxOX96WCMNEgG-IfHuUiqXbpYpEvdb2lJ23hu8MBwI0pXdTaenvzQYhz0achlx215qARTx1OyXBexMF1p2uOp\
    SpUzbPmQ24lr5zAbo6CkxZcd0h5GPJH107wz43HLW9DvjMvGP8_zGPtorUGBc3vAZ6_xu07KiAqFB2cNc83KM83FFVdG\
    AgYl44-MFK5YxtutrCV4-fImBaJifC3uUWjU41ME5LepNkNIOWkYiNt_8dbgpyQSiQ";
const SHORT_TOKEN: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiIsImtpZCI6InRlc3Qta2V5In0.eyJpc3MiOiJodHRwczovL2F1dGgub3Bl\
    bmFpLmNvbSIsImF1ZCI6ImFwcF90ZXN0X2NsaWVudCIsInN1YiI6InVzZXItdGVzdC1zdWJqZWN0IiwiZXhwIjoxODAw\
    MDAwNjAwLCJub25jZSI6InRlc3Qtbm9uY2UiLCJlbWFpbCI6InBlcnNvbkBleGFtcGxlLmludmFsaWQifQ.KtfEbXdJf\
    cvMfwc-4s27RsdnOzSuTQsfV8OaOQPjyqwbCX2D8a623V3TlCZpmLWgovKuOEdHGmZpV1XYCHDh_2w1jhxxHFtbPhvOc\
    93ywiY9sPFjJ63AxxeBPYRbfJRg1xhsCh7K4uQSP0EF8HT3b5aEetSXYeET3NZ2_fRfzZfLN5xRde9AyzrnHj7OCwfy7\
    m4NjC7vSbX7Tlu_8EcolML7SmaddCh2wgxL9Hb62o90zJf42vpxUr2nD65e2s2AMnf8du6xC4E6UwAind5aGNJDgz2el\
    NDkFWD4ZUseZ8oUeEILRRpTxzps3LKM9h0e1IlSdvEfs4-Gc2T-a6hB3g";

fn claims() -> Value {
    json!({
        "iss": ISSUER, "aud": CLIENT, "sub": SUBJECT, "exp": NOW + 600, "nonce": NONCE,
        "email": EMAIL,
    })
}

/// The valid claims with each named claim replaced, or removed when its value is None.
fn with(changes: &[(&str, Option<Value>)]) -> Value {
    let mut claims = claims();
    let object = claims.as_object_mut().expect("claims are an object");
    for (name, value) in changes {
        match value {
            Some(value) => object.insert((*name).to_owned(), value.clone()),
            None => object.remove(*name),
        };
    }
    claims
}

fn header(alg: &str, kid: Option<&str>) -> Value {
    let mut header = json!({ "typ": "JWT", "alg": alg });
    if let Some(kid) = kid {
        header["kid"] = json!(kid);
    }
    header
}

/// An RS256 or RS384 JWT signed by rustls's ring signer from the PKCS#8 fixture keys.
fn signed(header: &Value, claims: &Value, key: &TestKey) -> String {
    let scheme = match header["alg"].as_str() {
        Some("RS384") => SignatureScheme::RSA_PKCS1_SHA384,
        _ => SignatureScheme::RSA_PKCS1_SHA256,
    };
    signed_raw(scheme, &format!("{}.{}", part(header), part(claims)), key)
}

/// A signing input with its signature appended, for parts that aren't JSON values.
fn signed_raw(scheme: SignatureScheme, message: &str, key: &TestKey) -> String {
    let der = STANDARD.decode(key.pkcs8).expect("fixture key is base64");
    let signer = any_supported_type(&PrivateKeyDer::Pkcs8(der.into()))
        .expect("fixture key is RSA PKCS#8")
        .choose_scheme(&[scheme])
        .expect("the key signs this scheme");
    let signature = signer.sign(message.as_bytes()).expect("sign a test token");
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn token(claims: &Value) -> String {
    signed(&header("RS256", Some(KID)), claims, &SIGNER)
}

fn jwk(kid: &str, key: &TestKey) -> Value {
    json!({
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
        "n": key.n, "e": key.e,
    })
}

fn jwks() -> Value {
    let secret = URL_SAFE_NO_PAD.encode(b"test-only-hmac-key");
    json!({ "keys": [jwk(KID, &SIGNER), { "kty": "oct", "kid": "shared", "k": secret }] })
}

fn keys(document: Value) -> KeySet {
    serde_json::from_value(document).expect("a JWK set")
}

fn check(token: &str) -> Result<VerifiedIdentity, IdentityError> {
    verify_signed(token, &keys(jwks()), CLIENT, NONCE, NOW)
}

fn part(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

#[test]
fn a_valid_token_yields_its_redacted_identity() {
    let identity = check(&token(&claims())).expect("valid token");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );
    assert_eq!(
        format!("{identity:?}"),
        format!(
            "VerifiedIdentity {{ subject: ***, email: Some(***), expires_at: {} }}",
            NOW + 600
        )
    );
}

#[test]
fn accepted_claim_edges() {
    let longest_subject = "x".repeat(256);
    let longest_email = "x".repeat(254);
    let cases = [
        (
            "expiry inside the 30 s leeway",
            with(&[("exp", Some(json!(NOW - 30)))]),
            SUBJECT,
            Some(EMAIL),
        ),
        (
            "azp equal to the client",
            with(&[("azp", Some(json!(CLIENT)))]),
            SUBJECT,
            Some(EMAIL),
        ),
        (
            "several audiences with the client as azp",
            with(&[
                ("aud", Some(json!([CLIENT, "app_other"]))),
                ("azp", Some(json!(CLIENT))),
            ]),
            SUBJECT,
            Some(EMAIL),
        ),
        ("no email", with(&[("email", None)]), SUBJECT, None),
        (
            "longest subject",
            with(&[("sub", Some(json!(longest_subject)))]),
            longest_subject.as_str(),
            Some(EMAIL),
        ),
        (
            "longest email",
            with(&[("email", Some(json!(longest_email)))]),
            SUBJECT,
            Some(longest_email.as_str()),
        ),
    ];
    for (name, claims, subject, email) in cases {
        let identity = check(&token(&claims)).expect(name);
        assert_eq!(identity.subject.expose().as_str(), subject, "{name}");
        assert_eq!(
            identity.email.as_ref().map(|email| email.expose().as_str()),
            email,
            "{name}"
        );
    }
    let nonce = "n".repeat(128);
    let token = token(&with(&[("nonce", Some(json!(nonce)))]));
    let identity =
        verify_signed(&token, &keys(jwks()), CLIENT, &nonce, NOW).expect("longest nonce");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );
}

#[test]
fn exact_token_and_client_byte_limits_are_accepted() {
    let mut header = header("RS256", Some(KID));
    header["cty"] = json!("application/json");
    let padded_token = signed(
        &header,
        &with(&[("padding", Some(json!("x".repeat(11_794))))]),
        &SIGNER,
    );
    assert_eq!(padded_token.len(), 16 * 1024);
    let identity = check(&padded_token).expect("a valid token of exactly 16 KiB");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );

    let client = "c".repeat(256);
    let token = token(&with(&[("aud", Some(json!(client)))]));
    let identity = verify_signed(&token, &keys(jwks()), &client, NONCE, NOW)
        .expect("a valid token bound to a 256-byte client");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );
}

#[test]
fn claim_rejections_have_exact_kinds() {
    use IdentityError::{Bounds, Token};
    let long = json!("x".repeat(257));
    let mut audiences = vec![json!(CLIENT)];
    audiences.extend((0..16).map(|index| json!(format!("app_{index}"))));
    let azp = ("azp", Some(json!(CLIENT)));
    let cases = [
        ("missing iss", with(&[("iss", None)]), Token),
        ("empty iss", with(&[("iss", Some(json!("")))]), Token),
        (
            "other iss",
            with(&[("iss", Some(json!("https://auth.openai.com/")))]),
            Token,
        ),
        ("iss list", with(&[("iss", Some(json!([ISSUER])))]), Token),
        ("missing aud", with(&[("aud", None)]), Token),
        ("empty aud", with(&[("aud", Some(json!("")))]), Token),
        (
            "other aud",
            with(&[("aud", Some(json!("app_other")))]),
            Token,
        ),
        ("empty aud list", with(&[("aud", Some(json!([])))]), Token),
        (
            "aud list without the client",
            with(&[("aud", Some(json!(["app_other"])))]),
            Token,
        ),
        (
            "several audiences, no azp",
            with(&[("aud", Some(json!([CLIENT, "app_other"])))]),
            Token,
        ),
        (
            "several audiences, other azp",
            with(&[
                ("aud", Some(json!([CLIENT, "app_other"]))),
                ("azp", Some(json!("app_other"))),
            ]),
            Token,
        ),
        (
            "other azp",
            with(&[("azp", Some(json!("app_other")))]),
            Token,
        ),
        ("missing exp", with(&[("exp", None)]), Token),
        (
            "expired past the leeway",
            with(&[("exp", Some(json!(NOW - 31)))]),
            Token,
        ),
        (
            "exp not a number",
            with(&[("exp", Some(json!("later")))]),
            Token,
        ),
        ("missing sub", with(&[("sub", None)]), Token),
        ("empty sub", with(&[("sub", Some(json!("")))]), Token),
        ("missing nonce", with(&[("nonce", None)]), Token),
        ("empty nonce", with(&[("nonce", Some(json!("")))]), Token),
        (
            "other nonce",
            with(&[("nonce", Some(json!("other-nonce")))]),
            Token,
        ),
        (
            "nonce prefix",
            with(&[("nonce", Some(json!("test-non")))]),
            Token,
        ),
        ("long sub", with(&[("sub", Some(long.clone()))]), Bounds),
        (
            "long email",
            with(&[("email", Some(json!("x".repeat(255))))]),
            Bounds,
        ),
        (
            "long nonce",
            with(&[("nonce", Some(json!("n".repeat(129))))]),
            Bounds,
        ),
        ("long azp", with(&[("azp", Some(long.clone()))]), Bounds),
        (
            "17 audiences",
            with(&[("aud", Some(json!(audiences))), azp.clone()]),
            Bounds,
        ),
        (
            "long audience",
            with(&[("aud", Some(json!([CLIENT, long]))), azp]),
            Bounds,
        ),
    ];
    for (name, claims, expected) in cases {
        assert_eq!(check(&token(&claims)).map(|_| ()), Err(expected), "{name}");
    }
}

#[test]
fn a_claim_with_the_wrong_json_type_is_refused_not_read_as_absent() {
    for (name, value) in [
        ("iss", json!(1)),
        ("aud", json!(5)),
        ("aud", json!([CLIENT, 5])),
        ("sub", json!(7)),
        ("azp", json!(3)),
        ("email", json!(true)),
        ("nonce", json!(4)),
    ] {
        let claims = with(&[(name, Some(value.clone()))]);
        assert_eq!(
            check(&token(&claims)).map(|_| ()),
            Err(IdentityError::Token),
            "{name}: {value}"
        );
    }
}

#[test]
fn token_and_key_rejections_have_exact_kinds() {
    use IdentityError::{Bounds, Token};
    let valid = token(&claims());
    let [header_part, _, signature] = valid.split('.').collect::<Vec<_>>()[..] else {
        panic!("a three-part token");
    };
    let tampered = format!(
        "{header_part}.{}.{signature}",
        part(&with(&[("sub", Some(json!("user-other")))]))
    );
    let unsigned = format!(
        "{}.{}.",
        part(&json!({ "alg": "none", "kid": KID })),
        part(&claims())
    );
    let message = format!("{}.{}", part(&header("HS256", Some(KID))), part(&claims()));
    let mac = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, b"test-only-hmac-key"),
        message.as_bytes(),
    );
    let hmac = format!("{message}.{}", URL_SAFE_NO_PAD.encode(mac));
    let mut critical = header("RS256", Some(KID));
    critical["crit"] = json!(["exp"]);
    let mut numbered = header("RS256", None);
    numbered["kid"] = json!(7);
    let padded = format!("{valid}==");
    let cases = [
        ("empty", String::new(), Token),
        ("not a JWT", "not-a-jwt".to_owned(), Token),
        ("two parts", "e30.e30".to_owned(), Token),
        ("bad base64", "!!.!!.!!".to_owned(), Token),
        ("extra part", format!("{valid}.e30"), Token),
        ("oversized", "a".repeat(MAX_TOKEN_BYTES + 1), Bounds),
        ("tampered payload", tampered, Token),
        ("alg none", unsigned, Token),
        ("HS256", hmac, Token),
        ("crit header", signed(&critical, &claims(), &SIGNER), Token),
        (
            "kid not a string",
            signed(&numbered, &claims(), &SIGNER),
            Token,
        ),
        ("padded signature", padded, Token),
        (
            "alg not a string",
            signed(&json!({ "alg": ["RS256"], "kid": KID }), &claims(), &SIGNER),
            Token,
        ),
        (
            "RS384",
            signed(&header("RS384", Some(KID)), &claims(), &SIGNER),
            Token,
        ),
        (
            "another key",
            signed(&header("RS256", Some(KID)), &claims(), &OTHER),
            Token,
        ),
        (
            "no kid",
            signed(&header("RS256", None), &claims(), &SIGNER),
            Token,
        ),
        (
            "empty kid",
            signed(&header("RS256", Some("")), &claims(), &SIGNER),
            Token,
        ),
        (
            "unknown kid",
            signed(&header("RS256", Some("gone")), &claims(), &SIGNER),
            IdentityError::UnknownKey,
        ),
        (
            "kid of a secret key",
            signed(&header("RS256", Some("shared")), &claims(), &SIGNER),
            Token,
        ),
        (
            "long kid",
            signed(&header("RS256", Some(&"k".repeat(257))), &claims(), &SIGNER),
            Bounds,
        ),
    ];
    for (name, token, expected) in cases {
        assert_eq!(check(&token).map(|_| ()), Err(expected), "{name}");
    }
    let crowded = keys(json!({ "keys": vec![jwk(KID, &SIGNER); 65] }));
    assert_eq!(
        verify_signed(&valid, &crowded, CLIENT, NONCE, NOW).map(|_| ()),
        Err(Bounds)
    );
}

#[test]
fn malformed_compact_parts_are_refused_one_at_a_time() {
    let sign = |header: &str, payload: &str| {
        signed_raw(
            SignatureScheme::RSA_PKCS1_SHA256,
            &format!("{header}.{payload}"),
            &SIGNER,
        )
    };
    let padded = |value: &Value| {
        let mut text = value.to_string();
        while text.len().is_multiple_of(3) {
            text.push(' ');
        }
        (URL_SAFE.encode(&text), URL_SAFE_NO_PAD.encode(&text))
    };
    let header_part = part(&header("RS256", Some(KID)));
    let payload_part = part(&claims());
    let (padded_header, unpadded_header) = padded(&header("RS256", Some(KID)));
    let (padded_payload, unpadded_payload) = padded(&claims());
    let identity = |token: &str| {
        check(token).map(|identity| {
            let email = identity
                .email
                .map(|email| email.expose().as_str().to_owned());
            (identity.subject.expose().as_str().to_owned(), email)
        })
    };
    for control in [
        sign(&header_part, &payload_part),
        sign(&unpadded_header, &payload_part),
        sign(&header_part, &unpadded_payload),
    ] {
        assert_eq!(
            identity(&control),
            Ok((SUBJECT.to_owned(), Some(EMAIL.to_owned()))),
            "an unbroken twin is accepted with the fixture's identity"
        );
    }
    let cases = [
        ("empty header", sign("", &payload_part)),
        ("empty payload", sign(&header_part, "")),
        ("empty signature", format!("{header_part}.{payload_part}.")),
        (
            "header not JSON",
            sign(&URL_SAFE_NO_PAD.encode("not json"), &payload_part),
        ),
        (
            "header not an object",
            sign(&part(&json!(["RS256", KID])), &payload_part),
        ),
        ("padded header", sign(&padded_header, &payload_part)),
        ("padded payload", sign(&header_part, &padded_payload)),
    ];
    for (name, token) in cases {
        assert_eq!(
            check(&token).map(|_| ()),
            Err(IdentityError::Token),
            "{name}"
        );
    }
}

#[test]
fn a_modulus_under_2048_bits_is_refused_though_ring_would_verify_it() {
    let short = URL_SAFE_NO_PAD.decode(SHORT_N).expect("fixture modulus");
    let full = URL_SAFE_NO_PAD.decode(SIGNER.n).expect("fixture modulus");
    assert_eq!((short.len(), short[0].leading_zeros()), (256, 1));
    assert_eq!((full.len(), full[0].leading_zeros()), (256, 0));
    let (message, signature) = SHORT_TOKEN.rsplit_once('.').expect("a signed token");
    let (head, payload) = message.split_once('.').expect("a header and a payload");
    let decoded = |part: &str| {
        let json = URL_SAFE_NO_PAD.decode(part).expect("a base64url part");
        serde_json::from_slice::<Value>(&json).expect("a JSON part")
    };
    // Compared as values: key order depends on whether serde_json's preserve_order is unified in.
    assert_eq!(
        (decoded(head), decoded(payload)),
        (header("RS256", Some(KID)), claims())
    );
    let ring_alone = RsaPublicKeyComponents {
        n: &short,
        e: &URL_SAFE_NO_PAD.decode(SIGNER.e).expect("fixture exponent"),
    }
    .verify(
        &RSA_PKCS1_2048_8192_SHA256,
        message.as_bytes(),
        &URL_SAFE_NO_PAD.decode(signature).expect("a signature"),
    );
    assert!(ring_alone.is_ok(), "ring's byte-rounded minimum admits it");
    let mut short_key = jwk(KID, &SIGNER);
    short_key["n"] = json!(SHORT_N);
    let set = keys(json!({ "keys": [short_key] }));
    assert_eq!(
        verify_signed(SHORT_TOKEN, &set, CLIENT, NONCE, NOW).map(|_| ()),
        Err(IdentityError::Token)
    );
    let identity = check(&token(&claims())).expect("2048 bits is accepted");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );
}

#[test]
fn a_returning_subject_must_match() {
    assert_eq!(check_subject(None, SUBJECT), Ok(()));
    assert_eq!(check_subject(Some(SUBJECT), SUBJECT), Ok(()));
    assert_eq!(
        check_subject(Some("user-other"), SUBJECT),
        Err(IdentityError::OtherAccount)
    );
    assert_eq!(check_subject(Some(SUBJECT), ""), Err(IdentityError::Token));
    assert_eq!(
        check_subject(None, &"x".repeat(257)),
        Err(IdentityError::Token)
    );
}

#[test]
fn only_the_exact_plan_scope_permits_plan_use() {
    for scope in [
        "chatgpt.tokens.use.direct",
        "openid email\tchatgpt.tokens.use.direct offline_access",
    ] {
        assert!(permits_plan(scope), "{scope:?}");
    }
    for scope in [
        "",
        "openid email",
        "chatgpt.tokens.use",
        "chatgpt.tokens.use.directx",
        "CHATGPT.TOKENS.USE.DIRECT",
        "chatgpt.tokens.use.direct,openid",
    ] {
        assert!(!permits_plan(scope), "{scope:?}");
    }
}

/// Replays scripted responses in order and keeps every request it was given.
struct Scripted {
    responses: Mutex<VecDeque<Result<Response, HttpError>>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(responses: impl IntoIterator<Item = Result<Response, HttpError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Request> {
        std::mem::take(&mut *self.requests.lock().expect("request log"))
    }
}

impl Http for Scripted {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        self.requests.lock().expect("request log").push(request);
        std::future::ready(
            self.responses
                .lock()
                .expect("scripted responses")
                .pop_front()
                .expect("a scripted response for every request"),
        )
    }
}

struct NoClock;

impl Clock for NoClock {
    fn unix_seconds(&self) -> Option<u64> {
        None
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "scripted replies mix transport errors with HTTP responses"
)]
fn response(status: u16, body: impl Into<Vec<u8>>) -> Result<Response, HttpError> {
    Ok(Response {
        status,
        request_id: None,
        meta: crate::connections::http_client::ResponseMeta::default(),
        body: Secret::new(Zeroizing::new(body.into())),
    })
}

fn discovery(issuer: &str, jwks_uri: &str) -> Result<Response, HttpError> {
    response(
        200,
        json!({ "issuer": issuer, "jwks_uri": jwks_uri }).to_string(),
    )
}

#[derive(Clone, Copy)]
pub(crate) enum FixtureSignature {
    Valid,
    WrongKey,
}

pub(crate) fn repair_exchange_fixture(
    audience: &str,
    nonce: &str,
    expires_at: u64,
    signature: FixtureSignature,
    scope: &str,
) -> impl Http + use<> {
    let claims = with(&[
        ("aud", Some(json!(audience))),
        ("nonce", Some(json!(nonce))),
        ("exp", Some(json!(expires_at))),
    ]);
    let id = match signature {
        FixtureSignature::Valid => token(&claims),
        FixtureSignature::WrongKey => signed(&header("RS256", Some(KID)), &claims, &OTHER),
    };
    Scripted::new([
        response(
            200,
            json!({
                "access_token": "replacement-access-SENTINEL",
                "refresh_token": "replacement-refresh-SENTINEL",
                "id_token": id,
                "token_type": "bearer",
                "scope": scope,
                "expires_in": 3600
            })
            .to_string(),
        ),
        discovery(ISSUER, JWKS_URI),
        response(200, jwks().to_string()),
    ])
}

fn secret(token: &str) -> Secret<Zeroizing<String>> {
    Secret::new(Zeroizing::new(token.to_owned()))
}

#[tokio::test]
async fn verify_reads_discovery_then_its_jwks_without_credentials() {
    let token = token(&claims());
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, jwks().to_string()),
    ]);
    let identity = verify(&http, &secret(&token), CLIENT, NONCE, &FixedClock(NOW))
        .await
        .expect("verified");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    let requests = http.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        matches!(requests[0].endpoint, Endpoint::Discovery),
        "discovery first"
    );
    assert!(
        matches!(requests[1].endpoint, Endpoint::Jwks(_)),
        "then its JWKS"
    );
    assert!(
        requests.iter().all(|request| request.fields.is_empty()),
        "no credentials sent"
    );
    let logged = format!("{requests:?}");
    assert!(
        !logged.contains(&token) && !logged.contains("jwks.json"),
        "requests print redacted"
    );
}

/// A `kid` the key set lacks gets the set fetched once more, as after a key rotation; still
/// missing, it's `UnknownKey`, never `Token`, and nothing is fetched a third time.
#[tokio::test]
async fn a_key_missing_from_the_jwks_is_fetched_once_more() {
    let valid = token(&claims());
    let stale = json!({ "keys": [jwk("old-key", &SIGNER)] }).to_string();
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, stale.clone()),
        response(200, jwks().to_string()),
    ]);
    let identity = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW))
        .await
        .expect("verified with the refetched set");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(http.requests().len(), 3);
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, stale),
        response(200, json!({ "keys": [] }).to_string()),
    ]);
    let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
    assert_eq!(result.map(|_| ()), Err(IdentityError::UnknownKey));
    assert_eq!(http.requests().len(), 3);
}

/// Scripted responses, and a wall clock the second key-set response moves past the token's
/// expiry plus the leeway.
struct SlowRefetch {
    http: Scripted,
    served: AtomicUsize,
    unix: AtomicU64,
}

impl SlowRefetch {
    fn new() -> Self {
        let stale = json!({ "keys": [jwk("old-key", &SIGNER)] }).to_string();
        Self {
            http: Scripted::new([
                discovery(ISSUER, JWKS_URI),
                response(200, stale),
                response(200, jwks().to_string()),
            ]),
            served: AtomicUsize::new(0),
            unix: AtomicU64::new(NOW),
        }
    }
}

impl Http for SlowRefetch {
    fn execute(
        &self,
        request: Request,
    ) -> impl std::future::Future<Output = Result<Response, HttpError>> + Send {
        if self.served.fetch_add(1, Ordering::SeqCst) == 2 {
            self.unix.store(NOW + 600 + 31, Ordering::SeqCst);
        }
        self.http.execute(request)
    }
}

impl Clock for SlowRefetch {
    fn unix_seconds(&self) -> Option<u64> {
        Some(self.unix.load(Ordering::SeqCst))
    }
}

/// #103 M1: a sign-in token that expires while its key set is fetched again is judged when the
/// refetch ends, so it's refused; a refreshed set's check, which ignores expiry, still passes.
#[tokio::test]
async fn a_token_that_expires_during_the_refetch_is_refused() {
    let valid = token(&claims());
    let late = SlowRefetch::new();
    let result = verify(&late, &secret(&valid), CLIENT, NONCE, &late).await;
    assert_eq!(result.map(|_| ()), Err(IdentityError::Token));
    assert_eq!(late.http.requests().len(), 3);
    let late = SlowRefetch::new();
    let identity = verify_refreshed(&late, &secret(&valid), CLIENT, SUBJECT, &late)
        .await
        .expect("a refreshed set's expiry isn't checked");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(late.http.requests().len(), 3);
}

#[tokio::test]
async fn verify_refuses_bad_discovery_before_fetching_keys() {
    use IdentityError::{Bounds, Discovery, Http as Transport, Status};
    let valid = token(&claims());
    let cases = [
        ("unavailable", response(503, "{}"), Status { status: 503 }),
        ("not JSON", response(200, "<html>"), Discovery),
        (
            "no jwks_uri",
            response(200, json!({ "issuer": ISSUER }).to_string()),
            Discovery,
        ),
        (
            "issuer with a slash",
            discovery("https://auth.openai.com/", JWKS_URI),
            Discovery,
        ),
        (
            "other issuer",
            discovery("https://issuer.example.invalid", JWKS_URI),
            Discovery,
        ),
        (
            "http JWKS",
            discovery(ISSUER, "http://auth.openai.com/jwks"),
            Discovery,
        ),
        (
            "other host",
            discovery(ISSUER, "https://keys.example.invalid/jwks"),
            Discovery,
        ),
        (
            "lookalike host",
            discovery(ISSUER, "https://auth.openai.com.example.invalid/jwks"),
            Discovery,
        ),
        (
            "other port",
            discovery(ISSUER, "https://auth.openai.com:8443/jwks"),
            Discovery,
        ),
        (
            "userinfo",
            discovery(ISSUER, "https://user@auth.openai.com/jwks"),
            Discovery,
        ),
        (
            "password",
            discovery(ISSUER, "https://:pass@auth.openai.com/jwks"),
            Discovery,
        ),
        (
            "fragment",
            discovery(ISSUER, "https://auth.openai.com/jwks#keys"),
            Discovery,
        ),
        ("not a URL", discovery(ISSUER, "jwks"), Discovery),
        (
            "long URL",
            discovery(ISSUER, &format!("{JWKS_URI}?{}", "q".repeat(2048))),
            Discovery,
        ),
        (
            "oversized body",
            response(200, vec![b' '; MAX_BODY_BYTES + 1]),
            Bounds,
        ),
        (
            "timeout",
            Err(HttpError::Timeout),
            Transport(HttpError::Timeout),
        ),
    ];
    for (name, first, expected) in cases {
        let http = Scripted::new([first]);
        let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
        assert_eq!(result.map(|_| ()), Err(expected), "{name}");
        assert_eq!(http.requests().len(), 1, "{name}: no JWKS request");
    }
}

#[tokio::test]
async fn verify_ignores_unusable_unselected_keys() {
    let valid = signed(&header("RS256", Some(KID)), &claims(), &OTHER);
    let cases = [
        ("another usable RSA key", jwk("first-key", &SIGNER)),
        ("not an object", Value::Null),
        (
            "unknown key type",
            json!({ "kty": "future", "kid": "future-key" }),
        ),
        (
            "unknown EC curve",
            json!({ "kty": "EC", "kid": "future-curve", "crv": "future", "x": "AQAB", "y": "AQAB" }),
        ),
        (
            "malformed RSA key",
            json!({ "kty": "RSA", "kid": "broken-key", "n": 23, "e": "AQAB" }),
        ),
    ];
    for (name, unusable) in cases {
        let http = Scripted::new([
            discovery(ISSUER, JWKS_URI),
            response(
                200,
                json!({ "keys": [unusable, jwk(KID, &OTHER)] }).to_string(),
            ),
        ]);
        let identity = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW))
            .await
            .expect(name);
        assert_eq!(identity.subject.expose().as_str(), SUBJECT, "{name}");
        assert_eq!(
            identity.email.as_ref().map(|email| email.expose().as_str()),
            Some(EMAIL),
            "{name}"
        );
    }
}

#[tokio::test]
async fn verify_accepts_missing_key_metadata() {
    let valid = token(&claims());
    for omitted in [&["use"][..], &["alg"][..], &["use", "alg"][..]] {
        let mut key = jwk(KID, &SIGNER);
        for field in omitted {
            key.as_object_mut().expect("JWK object").remove(*field);
        }
        let http = Scripted::new([
            discovery(ISSUER, JWKS_URI),
            response(200, json!({ "keys": [key] }).to_string()),
        ]);
        let identity = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW))
            .await
            .expect("use and alg are optional");
        assert_eq!(identity.subject.expose().as_str(), SUBJECT, "{omitted:?}");
        assert_eq!(
            identity.email.as_ref().map(|email| email.expose().as_str()),
            Some(EMAIL),
            "{omitted:?}"
        );
    }
}

#[tokio::test]
async fn verify_rejects_conflicting_selected_key_metadata() {
    let valid = token(&claims());
    let cases = [
        ("encryption use", "use", json!("enc")),
        ("unknown use", "use", json!("other")),
        ("null use", "use", Value::Null),
        ("other algorithm", "alg", json!("RS512")),
        ("unknown algorithm", "alg", json!("future")),
        ("null algorithm", "alg", Value::Null),
        ("not RSA", "kty", json!("EC")),
    ];
    for (name, field, value) in cases {
        let mut key = jwk(KID, &SIGNER);
        key[field] = value;
        let http = Scripted::new([
            discovery(ISSUER, JWKS_URI),
            response(200, json!({ "keys": [key] }).to_string()),
        ]);
        let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
        assert_eq!(result.map(|_| ()), Err(IdentityError::Token), "{name}");
        assert_eq!(
            http.requests().len(),
            2,
            "a selected key refusal is not a refetch"
        );
    }
}

#[tokio::test]
async fn verify_refuses_duplicate_string_key_ids() {
    let valid = token(&claims());
    let cases = [
        ("selected kid", vec![jwk(KID, &SIGNER), jwk(KID, &OTHER)]),
        (
            "unknown key type with the selected kid",
            vec![jwk(KID, &SIGNER), json!({ "kty": "future", "kid": KID })],
        ),
        (
            "unrelated malformed keys",
            vec![
                jwk(KID, &SIGNER),
                json!({ "kid": "duplicate" }),
                json!({ "kid": "duplicate" }),
            ],
        ),
    ];
    for (name, entries) in cases {
        let http = Scripted::new([
            discovery(ISSUER, JWKS_URI),
            response(200, json!({ "keys": entries }).to_string()),
        ]);
        let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
        assert_eq!(result.map(|_| ()), Err(IdentityError::Discovery), "{name}");
    }
}

#[tokio::test]
async fn verify_counts_raw_keys_before_ignoring_unusable_entries() {
    let valid = token(&claims());
    let mut entries = vec![Value::Null; 63];
    entries.push(jwk(KID, &SIGNER));
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, json!({ "keys": entries }).to_string()),
    ]);
    let identity = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW))
        .await
        .expect("64 raw keys with only one usable key");
    assert_eq!(identity.subject.expose().as_str(), SUBJECT);
    assert_eq!(
        identity.email.as_ref().map(|email| email.expose().as_str()),
        Some(EMAIL)
    );

    entries.push(Value::Null);
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, json!({ "keys": entries }).to_string()),
    ]);
    let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
    assert_eq!(result.map(|_| ()), Err(IdentityError::Bounds));
}

#[tokio::test]
async fn verify_refuses_a_bad_key_set() {
    use IdentityError::{Bounds, Discovery, Status};
    let valid = token(&claims());
    let cases = [
        ("missing", response(404, "{}"), Status { status: 404 }),
        ("not JSON", response(200, "keys"), Discovery),
        (
            "keys not a list",
            response(200, json!({ "keys": "none" }).to_string()),
            Discovery,
        ),
        (
            "oversized body",
            response(200, vec![b' '; MAX_BODY_BYTES + 1]),
            Bounds,
        ),
    ];
    for (name, keys, expected) in cases {
        let http = Scripted::new([discovery(ISSUER, JWKS_URI), keys]);
        let result = verify(&http, &secret(&valid), CLIENT, NONCE, &FixedClock(NOW)).await;
        assert_eq!(result.map(|_| ()), Err(expected), "{name}");
    }
}

#[tokio::test]
async fn verify_checks_inputs_first_and_needs_a_clock() {
    use IdentityError::{Bounds, Token};

    let valid = token(&claims());
    let long = "x".repeat(257);
    for (value, client, nonce, expected) in [
        ("", CLIENT, NONCE, Token),
        (valid.as_str(), "", NONCE, Token),
        (valid.as_str(), long.as_str(), NONCE, Bounds),
        (valid.as_str(), CLIENT, "", Token),
        (valid.as_str(), CLIENT, &long[..129], Bounds),
    ] {
        let http = Scripted::new([]);
        let result = verify(&http, &secret(value), client, nonce, &FixedClock(NOW)).await;
        assert_eq!(result.map(|_| ()), Err(expected));
        assert!(
            http.requests().is_empty(),
            "invalid input never fetches keys"
        );
    }
    let after_fetch: [(&str, &dyn Clock, IdentityError); 2] = [
        ("app_other", &FixedClock(NOW), Token),
        (CLIENT, &NoClock, IdentityError::Clock),
    ];
    for (client, clock, expected) in after_fetch {
        let http = Scripted::new([
            discovery(ISSUER, JWKS_URI),
            response(200, jwks().to_string()),
        ]);
        let result = verify(&http, &secret(&valid), client, NONCE, clock).await;
        assert_eq!(result.map(|_| ()), Err(expected));
        assert_eq!(http.requests().len(), 2);
    }
}

/// Design §3 rule 3: a refreshed set's token is bound to the subject already verified, with no
/// nonce of ours and no expiry check; a sign-in keeps both.
#[test]
fn a_refreshed_token_is_bound_to_the_saved_subject_alone() {
    use super::{Binding, signed};
    use IdentityError::{Bounds, OtherAccount, Token};
    let keys = keys(jwks());
    let refreshed = token(&with(&[("exp", Some(json!(NOW - 3600))), ("nonce", None)]));
    let subject = |token: &str, binding| {
        signed(token, &keys, CLIENT, binding, NOW)
            .map(|identity| identity.subject.expose().as_str().to_owned())
    };
    assert_eq!(
        subject(&refreshed, Binding::Refresh(SUBJECT)),
        Ok(SUBJECT.to_owned())
    );
    assert_eq!(subject(&refreshed, Binding::SignIn(NONCE)), Err(Token));
    assert_eq!(
        subject(&refreshed, Binding::Refresh("user-other")),
        Err(OtherAccount)
    );
    assert_eq!(subject(&refreshed, Binding::Refresh("")), Err(Token));
    let longest = "x".repeat(256);
    let at_limit = token(&with(&[("sub", Some(json!(longest))), ("nonce", None)]));
    assert_eq!(
        subject(&at_limit, Binding::Refresh(&longest)),
        Ok(longest.clone())
    );
    assert_eq!(
        subject(&at_limit, Binding::Refresh(&"x".repeat(257))),
        Err(Bounds)
    );
}
