//! SES-SNS signature verification + canonical-string + cert-allowlist tests.
//!
//! Crypto-only — does NOT spin up PG or the ntex test server. The cert +
//! signature fixture below was generated with:
//!
//! ```bash
//! openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem \
//!     -days 3650 -nodes -sha256 \
//!     -subj "/CN=sns.us-east-1.amazonaws.com"
//! printf 'Message\nhello\nMessageId\nmid-1\nTimestamp\n2026-01-01T00:00:00.000Z\nTopicArn\narn:aws:sns:us-east-1:1:t\nType\nNotification\n' \
//!   > canonical.txt
//! openssl dgst -sha1 -sign key.pem -out signature.bin canonical.txt
//! base64 -w0 signature.bin
//! ```
//!
//! For SignatureVersion 2, use `openssl dgst -sha256 -sign ...` with
//! the same `canonical.txt`.
//!
//! The exact `canonical.txt` shape MUST match what
//! `sns::canonical_string` produces for the same envelope; if you tweak
//! the field order or add a field, regenerate the fixture.

use zeroship_auth::mailer::sns::{
    canonical_string, is_valid_sns_cert_url, spki_from_pem, verify_with_cert, SnsEnvelope,
};

/// The fixture cert (CN=sns.us-east-1.amazonaws.com, RSA-2048, 10y validity).
const FIXTURE_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIDLTCCAhWgAwIBAgIUTH7FuxHVeHJu9tnGVBwBH/CBop4wDQYJKoZIhvcNAQEL
BQAwJjEkMCIGA1UEAwwbc25zLnVzLWVhc3QtMS5hbWF6b25hd3MuY29tMB4XDTI2
MDUyODAwNDQ0NloXDTM2MDUyNTAwNDQ0NlowJjEkMCIGA1UEAwwbc25zLnVzLWVh
c3QtMS5hbWF6b25hd3MuY29tMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKC
AQEAkK008VRZiUHwz7t+R+QGZQWhFWn7yiZrPeIxQoKd6iwI9ghdo79cImCQw6bP
cUk2lHRhZ8U25RZiFxyXclJol2+cStyfFq8s0RuJgyYsdBNfwetec6Mv6P2Z1hnx
R0dZcqydVewxcqkqrx2BJTZw4EFb/j5/2ezSfkxURGw/3rP04fhhqW5Pog8cikiS
prsIIrebaYpn4QZTakBPV6bFjJPZrqQ+9FZeLfjspSYOtUghxncUCfdsc7e0SxL/
W0jcXEaSaZNft6IIvWeKBNEF1qwNqNcrmTqAuPC3bPc8xcI1xH3OlcFKnukCj3JD
sYO5nvjWBlyOZpJnB+LFjyE2OQIDAQABo1MwUTAdBgNVHQ4EFgQUM15EoWZK2PfS
Zgng6odCSf/p5xgwHwYDVR0jBBgwFoAUM15EoWZK2PfSZgng6odCSf/p5xgwDwYD
VR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAB6LKj0MvFjXptPqsqIO1
hIQc7/DQSKcM4foxX3DRoXZLZaS/1U9/1DPgOnEXd0vo91cQw8Qz25+bgrbyKUR2
L7wB2ZsSXf1b74ARghuTztGTUyfurkKwSYeuIsDUgj9QuV5D3rWxb2HGUlvIqeaY
ZjThzyyXRPfgfUpiyz7paFwS7ABz8q1+CgJQWcSaXv8yw9VRSrtFN+G87TGBx52t
K+O517WSLOOpuY7JlfGkPTvFvg+IKwR+MHet4N2+8GzraVvrvZe0dRqHHPQwPEhi
kAF/DThSBry+8wKx8Mh6Wg9l2NJhTxbbr5fBJsNiUxGwJY/80bZK0ssJ1vGLK+vQ
Fw==
-----END CERTIFICATE-----";

/// Real RSA-SHA1 signature (base64) over the canonical string emitted by
/// `canonical_string` for the envelope in [`fixture_envelope`].
const FIXTURE_SIGNATURE_B64: &str = "juExdxQuCUsuYOiB+cjWm4yw2Gqy59eFTp4fZaFPg1BSnCfbaScDEY1dSjNnI9YELGbdSvmCLCFkkA3FAaDKpzDedNedjaCyTv6FZ7EXbnYAL+PoIlDz43YRCE8lclNGudMJiHtNO0+UCOo0FbzOvDAuf7kfX7buwl3wapV8Gf/dd3pYe8klln8Fkk0dS4qjRyigx6qaRc+wwwvbY4WTRJjypAB3y39BZWNUFJbrQy2HfUbdYdiq+08Gnh+1ABsFbuM76A09p29DD8zP6P1BosOIDH9AoihreEnzxIV4ATplxii43GSlPX2n8jFpfufXi4GiVBjaXEGolSNrHpaPmQ==";

const FIXTURE_V2_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIDLTCCAhWgAwIBAgIUWIbxWicKhwSCFh+VRG1JjjXE9JswDQYJKoZIhvcNAQEL
BQAwJjEkMCIGA1UEAwwbc25zLnVzLWVhc3QtMS5hbWF6b25hd3MuY29tMB4XDTI2
MDUyODIxMDUwNFoXDTM2MDUyNTIxMDUwNFowJjEkMCIGA1UEAwwbc25zLnVzLWVh
c3QtMS5hbWF6b25hd3MuY29tMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKC
AQEAqfj3hAbwLJbzTa1d8IAKl1rGY2Rji37c6O0ZjrtzDm/LfX8575s+6qMxAMY+
XXfBnxuH65bfD9X5MCyT6eSG6GcE3vQaylfY6ZmsYuhD3O9TrMIKfwOpxW0PbbFQ
kE3LJ5isrVTtCZYQ0ay0wc5RCKsLjirqz1mIEwOd4XXmfhx/HsGjYVEQB2ewksrb
oMCX68EeHexcvZFv9ofvzMX3ncUofsXeL2LrSkonJnT87I3e29isqT5P9mXPTwfo
x9/z16F2SYKFi8aPRdqoI1i8W5UEfrhsvYZqknE84zUA+45VQQXLm7aoPe3phvQG
JI/P3NdSRW0A5iOhBv3uibRy4QIDAQABo1MwUTAdBgNVHQ4EFgQUVURFmqYghUa3
R4csjky5E1gX190wHwYDVR0jBBgwFoAUVURFmqYghUa3R4csjky5E1gX190wDwYD
VR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAUhr56zra15FZN53tz4Dz
6tfjJVxuAfJctyD1Id5PuoC4ifrrwe859/d4UHKetUQldrT8bgUjS9sgTD0xkBeo
maYJcwhSwOND3g3sLowbFTmb7Y/iyQewnjLswk1sPG+dRtXdn23PzzWR/ny2PxaU
3ynrZ9MX0uUDPy4e7els8gnfkZTeePY8nfLGNx5aE7RD4FqViI08GSTJBbtR55g5
bArdu2DxZE6oabLXqlkPybDur/Uhdjc5kePKa4Wvwh8sjKJwhXLwm8D/0ynX1TXk
E2PjBhFBxQWzIncHeka8ZMBH8otokEjz/5b9S3pLMPuSf8z/q71fQXrg6bQ7S/RC
Kg==
-----END CERTIFICATE-----";

const FIXTURE_V2_SIGNATURE_B64: &str = "bg9u688AXrzxGw2E7u3bDQhbuI9cL66j6E3p2R1NjB1qPjeW05HTjGMYXG3M6JU1GNf+fJGqKtUArQa1OnwL8o4E1wCOJhK3b/AtRncjX9sykLLtUbnVdlUhsEO6x5J3tJC8P+l/3nyqVr702W92G+cJApnqVDYB8qNdHZFW+V81M9Oc9LMhVG8Lcg51I3+UXrkKcf1Agw4nK/pIwBhd8F+6hPbMw+eY7TJlxeP8zOQRO94hrxuRX5uZRWP0uJu63WeE6DgIkmbewL3GUrN76MVXt1PTkj/VuCC7aGBvE5A2f2LDRhgZ9Yi2s8ojVQgfdO3KNaza6sLM6Q6XXR5pnA==";

/// The Notification envelope the fixture was signed against. No
/// `Subject`, no `UnsubscribeURL` — matches the canonical string used
/// by `openssl dgst -sha1 -sign`.
fn fixture_envelope() -> SnsEnvelope {
    SnsEnvelope {
        r#type: "Notification".into(),
        message_id: "mid-1".into(),
        topic_arn: "arn:aws:sns:us-east-1:1:t".into(),
        message: "hello".into(),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        signature: FIXTURE_SIGNATURE_B64.to_string(),
        signature_version: "1".into(),
        signing_cert_url: "https://sns.us-east-1.amazonaws.com/test.pem".into(),
        subject: None,
        unsubscribe_url: None,
        token: None,
        subscribe_url: None,
    }
}

#[test]
fn verify_accepts_valid_signature() {
    let env = fixture_envelope();
    // Sanity: canonical string still matches the bytes we signed.
    assert_eq!(
        canonical_string(&env),
        "Message\nhello\n\
         MessageId\nmid-1\n\
         Timestamp\n2026-01-01T00:00:00.000Z\n\
         TopicArn\narn:aws:sns:us-east-1:1:t\n\
         Type\nNotification\n",
        "canonical string drifted from fixture — regenerate signature"
    );
    verify_with_cert(&env, FIXTURE_CERT_PEM)
        .expect("fixture signature must verify against fixture cert");
}

#[test]
fn verify_accepts_signature_version_2() {
    let mut env = fixture_envelope();
    env.signature_version = "2".into();
    env.signature = FIXTURE_V2_SIGNATURE_B64.to_string();
    verify_with_cert(&env, FIXTURE_V2_CERT_PEM)
        .expect("v2 fixture signature must verify against fixture cert");
}

#[test]
fn verify_rejects_tampered_signature() {
    let mut env = fixture_envelope();
    // Flip the first base64 char (decodes to a different byte → bad sig).
    let mut sig: Vec<u8> = env.signature.bytes().collect();
    sig[0] = if sig[0] == b'A' { b'B' } else { b'A' };
    env.signature = String::from_utf8(sig).expect("ascii");
    let err = verify_with_cert(&env, FIXTURE_CERT_PEM)
        .expect_err("tampered signature must NOT verify");
    let msg = format!("{err}");
    assert!(
        msg.contains("rejected") || msg.contains("base64"),
        "unexpected error variant: {msg}"
    );
}

#[test]
fn verify_rejects_tampered_message() {
    let mut env = fixture_envelope();
    env.message = "goodbye".into(); // different canonical string → bad sig
    let err = verify_with_cert(&env, FIXTURE_CERT_PEM)
        .expect_err("tampered message must NOT verify");
    assert!(
        format!("{err}").contains("rejected"),
        "expected signature-rejection error"
    );
}

#[test]
fn ssrf_check_rejects_non_amazonaws_hosts() {
    // Belt-and-braces — the unit-test inside the module already
    // exercises these, but we re-verify from the test binary's
    // public-API perspective so a future refactor of the impl can't
    // weaken the contract by accident.
    assert!(is_valid_sns_cert_url(
        "https://sns.us-east-1.amazonaws.com/x.pem"
    ));
    assert!(!is_valid_sns_cert_url("https://sns.evil.com/x.pem"));
    assert!(!is_valid_sns_cert_url(
        "https://attacker.amazonaws.com.fake.com/x.pem"
    ));
    assert!(!is_valid_sns_cert_url(
        "http://sns.us-east-1.amazonaws.com/x.pem"
    ));
    assert!(!is_valid_sns_cert_url(
        "https://sns.us-east-1.amazonaws.com:8443/x.pem"
    ));
}

#[test]
fn spki_extraction_handles_valid_cert() {
    let der = spki_from_pem(FIXTURE_CERT_PEM).expect("SPKI extract");
    // SPKI DER starts with `0x30` (SEQUENCE tag). Sanity check
    // we got something cert-shaped, not garbage.
    assert!(!der.is_empty());
    assert_eq!(der[0], 0x30, "SPKI must start with SEQUENCE tag");
}

#[test]
fn spki_extraction_rejects_non_certificate_pem() {
    // A PEM block with the wrong label must be rejected, even if
    // the body decodes as DER.
    let bogus = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDDsZ8E
-----END PRIVATE KEY-----";
    let err = spki_from_pem(bogus).expect_err("non-CERT PEM must fail");
    assert!(format!("{err}").contains("CERTIFICATE"));
}
