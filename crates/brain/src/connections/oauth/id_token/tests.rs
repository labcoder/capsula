use std::collections::VecDeque;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use kumochi_protocol::Secret;
use rustls::SignatureScheme;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::PrivateKeyDer;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{IdentityError, VerifiedIdentity, check_subject, permits_plan, verify, verify_signed};
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

fn header(alg: Algorithm, kid: Option<&str>) -> Header {
    Header {
        kid: kid.map(str::to_owned),
        ..Header::new(alg)
    }
}

/// An RS256 or RS384 JWT signed by rustls's ring signer, since jsonwebtoken reads only PKCS#1.
fn signed(header: &Header, claims: &Value, key: &TestKey) -> String {
    let scheme = match header.alg {
        Algorithm::RS256 => SignatureScheme::RSA_PKCS1_SHA256,
        Algorithm::RS384 => SignatureScheme::RSA_PKCS1_SHA384,
        other => panic!("no RSA scheme for {other:?}"),
    };
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).expect("header JSON"));
    let message = format!("{header}.{}", part(claims));
    let der = STANDARD.decode(key.pkcs8).expect("fixture key is base64");
    let signer = any_supported_type(&PrivateKeyDer::Pkcs8(der.into()))
        .expect("fixture key is RSA PKCS#8")
        .choose_scheme(&[scheme])
        .expect("the key signs this scheme");
    let signature = signer.sign(message.as_bytes()).expect("sign a test token");
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn token(claims: &Value) -> String {
    signed(&header(Algorithm::RS256, Some(KID)), claims, &SIGNER)
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

fn keys(document: Value) -> JwkSet {
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
        "VerifiedIdentity { subject: ***, email: Some(***) }"
    );
}

#[test]
fn accepted_claim_edges() {
    let at_limit = |len| "x".repeat(len);
    let cases = [
        (
            "expiry inside the 30 s leeway",
            with(&[("exp", Some(json!(NOW - 30)))]),
        ),
        (
            "azp equal to the client",
            with(&[("azp", Some(json!(CLIENT)))]),
        ),
        (
            "several audiences with the client as azp",
            with(&[
                ("aud", Some(json!([CLIENT, "app_other"]))),
                ("azp", Some(json!(CLIENT))),
            ]),
        ),
        ("no email", with(&[("email", None)])),
        (
            "longest subject",
            with(&[("sub", Some(json!(at_limit(256))))]),
        ),
        (
            "longest email",
            with(&[("email", Some(json!(at_limit(254))))]),
        ),
    ];
    for (name, claims) in cases {
        assert!(check(&token(&claims)).is_ok(), "{name}");
    }
    let nonce = "n".repeat(128);
    let token = token(&with(&[("nonce", Some(json!(nonce)))]));
    assert!(
        verify_signed(&token, &keys(jwks()), CLIENT, &nonce, NOW).is_ok(),
        "longest nonce"
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
    let hmac = encode(
        &header(Algorithm::HS256, Some(KID)),
        &claims(),
        &EncodingKey::from_secret(b"test-only-hmac-key"),
    )
    .expect("sign an HMAC token");
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
        (
            "RS384",
            signed(&header(Algorithm::RS384, Some(KID)), &claims(), &SIGNER),
            Token,
        ),
        (
            "another key",
            signed(&header(Algorithm::RS256, Some(KID)), &claims(), &OTHER),
            Token,
        ),
        (
            "no kid",
            signed(&header(Algorithm::RS256, None), &claims(), &SIGNER),
            Token,
        ),
        (
            "empty kid",
            signed(&header(Algorithm::RS256, Some("")), &claims(), &SIGNER),
            Token,
        ),
        (
            "unknown kid",
            signed(&header(Algorithm::RS256, Some("gone")), &claims(), &SIGNER),
            Token,
        ),
        (
            "kid of a secret key",
            signed(
                &header(Algorithm::RS256, Some("shared")),
                &claims(),
                &SIGNER,
            ),
            Token,
        ),
        (
            "long kid",
            signed(
                &header(Algorithm::RS256, Some(&"k".repeat(257))),
                &claims(),
                &SIGNER,
            ),
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
fn caller_bindings_are_checked_before_the_token() {
    use IdentityError::{Bounds, Token};
    let valid = token(&claims());
    let keys = keys(jwks());
    let long = "x".repeat(257);
    let cases = [
        ("empty client", "", NONCE, Token),
        ("long client", long.as_str(), NONCE, Bounds),
        ("other client", "app_other", NONCE, Token),
        ("empty nonce", CLIENT, "", Token),
        ("long nonce", CLIENT, &long[..129], Bounds),
    ];
    for (name, client, nonce, expected) in cases {
        let result = verify_signed(&valid, &keys, client, nonce, NOW).map(|_| ());
        assert_eq!(result, Err(expected), "{name}");
    }
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
        body: Secret::new(Zeroizing::new(body.into())),
    })
}

fn discovery(issuer: &str, jwks_uri: &str) -> Result<Response, HttpError> {
    response(
        200,
        json!({ "issuer": issuer, "jwks_uri": jwks_uri }).to_string(),
    )
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
async fn verify_refuses_a_bad_key_set() {
    use IdentityError::{Bounds, Discovery, Status, Token};
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
            "no matching key",
            response(200, json!({ "keys": [] }).to_string()),
            Token,
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
    let http = Scripted::new([]);
    let result = verify(&http, &secret(""), CLIENT, NONCE, &FixedClock(NOW)).await;
    assert_eq!(result.map(|_| ()), Err(IdentityError::Token));
    assert!(
        http.requests().is_empty(),
        "nothing is fetched for an empty token"
    );
    let http = Scripted::new([
        discovery(ISSUER, JWKS_URI),
        response(200, jwks().to_string()),
    ]);
    let result = verify(&http, &secret(&token(&claims())), CLIENT, NONCE, &NoClock).await;
    assert_eq!(result.map(|_| ()), Err(IdentityError::Clock));
}
