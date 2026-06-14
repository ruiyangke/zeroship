//! AWS SNS HTTP/HTTPS notification signature verification.
//!
//! SES → SNS → us: AWS Simple Email Service can publish bounce / complaint
//! events to an SNS topic; SNS then POSTs JSON to our `/webhooks/ses-sns`
//! endpoint. The HTTP body is **signed by SNS** so we can verify the
//! payload was actually issued by AWS (and not by an attacker probing the
//! suppression list).
//!
//! ## Algorithm (SNS signature v1/v2)
//!
//! 1. Validate `SignatureVersion` is `1` (RSA-SHA1 legacy) or `2`
//!    (RSA-SHA256).
//! 2. Validate `SigningCertURL` host is `sns.<region>.amazonaws.com`
//!    (anti-SSRF — see [`is_valid_sns_cert_url`]). Reject anything else,
//!    even other AWS hosts.
//! 3. Build the canonical string-to-sign: for each well-known field in
//!    **alphabetical order** that the payload contains, emit
//!    `<field-name>\n<value>\n`. Field sets per type:
//!    - `Notification` →
//!      `Message`, `MessageId`, `Subject`?, `Timestamp`, `TopicArn`, `Type`
//!    - `SubscriptionConfirmation` / `UnsubscribeConfirmation` →
//!      `Message`, `MessageId`, `SubscribeURL`, `Timestamp`, `Token`, `TopicArn`, `Type`
//! 4. Fetch the signing cert PEM from `SigningCertURL` (cyper, anti-SSRF
//!    already enforced). Extract its `SubjectPublicKeyInfo` DER.
//! 5. Base64-decode `Signature`. Verify against the canonical string
//!    using the cert's public key and the algorithm selected by
//!    `SignatureVersion`.
//!
//! Reference: <https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html>

use aws_lc_rs::signature::{
    self, UnparsedPublicKey, RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY,
    RSA_PKCS1_2048_8192_SHA256,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use x509_parser::prelude::*;

/// Outer SNS envelope.
///
/// Fields match the JSON shape AWS posts; missing fields per `Type` are
/// modeled as `Option`. We never read the signature bytes directly
/// through serde — it's base64 in the payload and we decode it during
/// verify.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)] // SNS field names are PascalCase and load-bearing
pub struct SnsEnvelope {
    #[serde(rename = "Type")]
    pub r#type: String,
    #[serde(rename = "MessageId")]
    pub message_id: String,
    #[serde(rename = "TopicArn")]
    pub topic_arn: String,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Timestamp")]
    pub timestamp: String,
    #[serde(rename = "Signature")]
    pub signature: String,
    #[serde(rename = "SignatureVersion")]
    pub signature_version: String,
    #[serde(rename = "SigningCertURL")]
    pub signing_cert_url: String,

    // Notification only.
    #[serde(rename = "Subject", default)]
    pub subject: Option<String>,
    #[serde(rename = "UnsubscribeURL", default)]
    pub unsubscribe_url: Option<String>,

    // SubscriptionConfirmation / UnsubscribeConfirmation only.
    #[serde(rename = "Token", default)]
    pub token: Option<String>,
    #[serde(rename = "SubscribeURL", default)]
    pub subscribe_url: Option<String>,
}

/// SES inner payload (the JSON-STRING inside `SnsEnvelope.message`).
///
/// `#[serde(tag = "notificationType")]` discriminates the bounce/complaint
/// variants; anything else falls into `Other` so we accept the envelope
/// and drop the inner event without crashing on unknown types.
#[derive(Debug, Deserialize)]
#[serde(tag = "notificationType")]
pub enum SesEvent {
    Bounce {
        bounce: Bounce,
    },
    Complaint {
        complaint: Complaint,
    },
    /// Delivery / `DeliveryDelay` / Send / Open / Click — we don't
    /// suppress on these. The `#[serde(other)]` catch-all keeps unknown
    /// future `notificationType` values from breaking the parse.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Bounce {
    #[serde(rename = "bounceType")]
    pub bounce_type: String,
    #[serde(rename = "bouncedRecipients", default)]
    pub bounced_recipients: Vec<Recipient>,
}

#[derive(Debug, Deserialize)]
pub struct Complaint {
    #[serde(rename = "complainedRecipients", default)]
    pub complained_recipients: Vec<Recipient>,
}

#[derive(Debug, Deserialize)]
pub struct Recipient {
    #[serde(rename = "emailAddress")]
    pub email_address: String,
}

/// Errors returned by [`verify`]. Kept as a `String` newtype because the
/// caller only ever logs or returns 401; we don't dispatch on the variant.
#[derive(Debug, thiserror::Error)]
#[error("sns verify: {0}")]
pub struct SnsError(pub String);

impl From<&str> for SnsError {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for SnsError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Anti-SSRF allowlist for `SigningCertURL`.
///
/// We accept ONLY hosts of the form `sns.<region>.amazonaws.com` (https,
/// no port override). The substring/suffix tricks (`sns.evil.com`,
/// `attacker.amazonaws.com.fake`) are explicitly rejected by anchoring
/// on the URL's authority component.
#[must_use]
pub fn is_valid_sns_cert_url(url: &str) -> bool {
    // Manual parse — `url::Url` is heavy and we only need scheme + host.
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // Authority ends at the first '/' (or end of string for hostless URLs).
    let host = rest.split('/').next().unwrap_or("");
    // Reject userinfo (`user@host`) and explicit ports (`host:443`) —
    // AWS never publishes either; treat both as suspect.
    if host.contains('@') || host.contains(':') {
        return false;
    }
    // Must be exactly `sns.<region>.amazonaws.com` — i.e. start with
    // `sns.`, end with `.amazonaws.com`, and have a non-empty region
    // segment in between with no further subdomains.
    let Some(after_sns) = host.strip_prefix("sns.") else {
        return false;
    };
    let Some(region) = after_sns.strip_suffix(".amazonaws.com") else {
        return false;
    };
    !region.is_empty() && !region.contains('.')
}

/// Build SNS v1 canonical string-to-sign.
///
/// Field order is alphabetical over the well-known names; only fields
/// PRESENT in the envelope are included. Format: `<key>\n<value>\n`
/// repeated, no trailing separator beyond the last `\n`.
///
/// Two field sets, gated on `Type`:
/// - `Notification` includes `Subject` if present, never `SubscribeURL` / `Token`.
/// - `SubscriptionConfirmation` / `UnsubscribeConfirmation` include
///   `SubscribeURL` and `Token`, never `Subject`.
#[must_use]
pub fn canonical_string(env: &SnsEnvelope) -> String {
    let is_subscription = matches!(
        env.r#type.as_str(),
        "SubscriptionConfirmation" | "UnsubscribeConfirmation"
    );
    let mut out = String::with_capacity(512);
    push_field(&mut out, "Message", &env.message);
    push_field(&mut out, "MessageId", &env.message_id);
    if is_subscription {
        if let Some(url) = &env.subscribe_url {
            push_field(&mut out, "SubscribeURL", url);
        }
    } else if let Some(s) = &env.subject {
        push_field(&mut out, "Subject", s);
    }
    push_field(&mut out, "Timestamp", &env.timestamp);
    if is_subscription {
        if let Some(t) = &env.token {
            push_field(&mut out, "Token", t);
        }
    }
    push_field(&mut out, "TopicArn", &env.topic_arn);
    push_field(&mut out, "Type", &env.r#type);
    out
}

fn push_field(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('\n');
    out.push_str(value);
    out.push('\n');
}

/// Verify the SNS envelope signature.
///
/// Steps: fetch the signing cert PEM → parse it → extract the
/// `SubjectPublicKeyInfo` DER → RSA verify against the canonical
/// string-to-sign. Pre-conditions (signature version + cert URL host)
/// are validated by the handler before this is called.
///
/// # Errors
///
/// [`SnsError`] on any of:
/// - HTTP fetch failure / non-2xx response
/// - PEM/X.509 parse failure
/// - Non-RSA public key in cert (SES-SNS only signs with RSA)
/// - Base64 decode failure on the `Signature` field
/// - Signature verification rejection
pub async fn verify(env: &SnsEnvelope) -> Result<(), SnsError> {
    let cert_pem = fetch_cert(&env.signing_cert_url).await?;
    verify_with_cert(env, &cert_pem)
}

/// Cert-already-fetched verification — split out from [`verify`] so unit
/// tests can exercise the crypto path without a live HTTP server. The
/// production handler always goes through [`verify`].
///
/// # Errors
///
/// [`SnsError`] on unsupported signature version, PEM/X.509 parse
/// failure, non-RSA key, base64 decode failure on the `Signature`
/// field, or signature rejection.
pub fn verify_with_cert(env: &SnsEnvelope, cert_pem: &str) -> Result<(), SnsError> {
    let spki_der = spki_from_pem(cert_pem)?;
    let sig_bytes = STANDARD
        .decode(env.signature.as_bytes())
        .map_err(|e| SnsError(format!("base64 decode Signature: {e}")))?;
    let canon = canonical_string(env);
    let (algorithm, label): (&dyn signature::VerificationAlgorithm, &str) =
        match env.signature_version.as_str() {
            "1" => (&RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY, "RSA-SHA1"),
            "2" => (&RSA_PKCS1_2048_8192_SHA256, "RSA-SHA256"),
            other => {
                return Err(SnsError(format!(
                    "unsupported SignatureVersion {other}; expected 1 or 2"
                )));
            }
        };
    // aws-lc-rs accepts both RFC 8017 (RSAPublicKey: modulus+exponent)
    // and RFC 5280 (SPKI) inputs for RSA verification; we feed SPKI
    // straight from the cert.
    let pk = UnparsedPublicKey::new(algorithm, spki_der);
    pk.verify(canon.as_bytes(), &sig_bytes)
        .map_err(|_| SnsError(format!("{label} verify rejected signature")))?;
    Ok(())
}

/// Fetch the signing certificate PEM. Caller MUST have validated the
/// URL host via [`is_valid_sns_cert_url`] before calling this. We pin
/// to cyper (no tokio).
async fn fetch_cert(url: &str) -> Result<String, SnsError> {
    let http = cyper::Client::new();
    let res = http
        .request(http::Method::GET, url.to_owned())
        .map_err(|e| SnsError(format!("cert GET build: {e}")))?
        .send()
        .await
        .map_err(|e| SnsError(format!("cert GET send: {e}")))?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SnsError(format!("cert GET → {status}")));
    }
    res.text()
        .await
        .map_err(|e| SnsError(format!("cert read body: {e}")))
}

/// Parse the PEM-encoded certificate and return its
/// `SubjectPublicKeyInfo` as a DER-encoded byte vector. Rejects
/// non-CERTIFICATE PEM blocks and non-RSA public keys.
///
/// # Errors
///
/// [`SnsError`] when the PEM is malformed, the block label is not
/// `CERTIFICATE`, the X.509 DER doesn't parse, or the embedded
/// `SubjectPublicKeyInfo` declares a non-RSA algorithm OID.
pub fn spki_from_pem(pem_text: &str) -> Result<Vec<u8>, SnsError> {
    // `parse_x509_pem` returns the first PEM block; AWS publishes
    // single-cert PEMs at the SigningCertURL.
    let (_, pem) = parse_x509_pem(pem_text.as_bytes())
        .map_err(|e| SnsError(format!("PEM parse: {e}")))?;
    if pem.label != "CERTIFICATE" {
        return Err(SnsError(format!("expected CERTIFICATE PEM, got {}", pem.label)));
    }
    let (_, cert) = X509Certificate::from_der(&pem.contents)
        .map_err(|e| SnsError(format!("X.509 parse: {e}")))?;
    // Validate it's RSA (we only signal SES-SNS, and AWS uses RSA).
    let spki = cert.tbs_certificate.subject_pki.clone();
    // OID 1.2.840.113549.1.1.1 = rsaEncryption (RFC 8017 §A.1).
    let alg_oid = spki.algorithm.algorithm.to_id_string();
    if alg_oid != "1.2.840.113549.1.1.1" {
        return Err(SnsError(format!(
            "expected RSA public key OID, got {alg_oid}"
        )));
    }
    // `raw` is the full SPKI DER (SEQUENCE { AlgorithmIdentifier,
    // BIT STRING }) — exactly what aws-lc-rs's RFC 5280 path wants.
    Ok(spki.raw.to_vec())
}

/// Auto-confirm an SNS subscription.
///
/// Issues a GET to the `SubscribeURL`. The handler MUST verify the SNS
/// signature BEFORE calling this — otherwise an attacker could trigger
/// arbitrary GETs via our service.
///
/// SNS's `SubscribeURL` is always on `sns.<region>.amazonaws.com`; we
/// re-check via [`is_valid_sns_cert_url`] before the GET as a
/// defense-in-depth measure.
///
/// # Errors
///
/// [`SnsError`] on non-2xx response or transport failure.
pub async fn confirm_subscription(url: &str) -> Result<(), SnsError> {
    if !is_valid_sns_cert_url(url) {
        return Err(SnsError(format!(
            "subscribe URL rejected by allowlist: {url}"
        )));
    }
    let http = cyper::Client::new();
    let res = http
        .request(http::Method::GET, url.to_owned())
        .map_err(|e| SnsError(format!("subscribe GET build: {e}")))?
        .send()
        .await
        .map_err(|e| SnsError(format!("subscribe GET send: {e}")))?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SnsError(format!("subscribe GET → {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_allowlist_accepts_canonical_sns_hosts() {
        assert!(is_valid_sns_cert_url(
            "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem"
        ));
        assert!(is_valid_sns_cert_url(
            "https://sns.eu-west-3.amazonaws.com/anything"
        ));
        assert!(is_valid_sns_cert_url("https://sns.us-east-1.amazonaws.com"));
    }

    #[test]
    fn ssrf_allowlist_rejects_attacker_variants() {
        // Wrong scheme.
        assert!(!is_valid_sns_cert_url(
            "http://sns.us-east-1.amazonaws.com/x"
        ));
        // Substring tricks.
        assert!(!is_valid_sns_cert_url("https://sns.evil.com/x"));
        assert!(!is_valid_sns_cert_url(
            "https://attacker.amazonaws.com.fake.com/x"
        ));
        // No region segment.
        assert!(!is_valid_sns_cert_url("https://sns..amazonaws.com/x"));
        assert!(!is_valid_sns_cert_url("https://sns.amazonaws.com/x"));
        // Multi-segment region (subdomain smuggling).
        assert!(!is_valid_sns_cert_url(
            "https://sns.evil.us-east-1.amazonaws.com/x"
        ));
        // Userinfo / port-override smuggling.
        assert!(!is_valid_sns_cert_url(
            "https://attacker@sns.us-east-1.amazonaws.com/x"
        ));
        assert!(!is_valid_sns_cert_url(
            "https://sns.us-east-1.amazonaws.com:8080/x"
        ));
        // Adjacent AWS services.
        assert!(!is_valid_sns_cert_url(
            "https://s3.us-east-1.amazonaws.com/x"
        ));
    }

    #[test]
    fn canonical_string_notification_alphabetical_with_subject() {
        let env = SnsEnvelope {
            r#type: "Notification".into(),
            message_id: "mid".into(),
            topic_arn: "arn:aws:sns:us-east-1:1:t".into(),
            message: "hello".into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            signature: String::new(),
            signature_version: "1".into(),
            signing_cert_url: String::new(),
            subject: Some("subj".into()),
            unsubscribe_url: None,
            token: None,
            subscribe_url: None,
        };
        let c = canonical_string(&env);
        assert_eq!(
            c,
            "Message\nhello\n\
             MessageId\nmid\n\
             Subject\nsubj\n\
             Timestamp\n2026-01-01T00:00:00.000Z\n\
             TopicArn\narn:aws:sns:us-east-1:1:t\n\
             Type\nNotification\n"
        );
    }

    #[test]
    fn canonical_string_notification_omits_absent_subject() {
        let env = SnsEnvelope {
            r#type: "Notification".into(),
            message_id: "mid".into(),
            topic_arn: "arn".into(),
            message: "m".into(),
            timestamp: "ts".into(),
            signature: String::new(),
            signature_version: "1".into(),
            signing_cert_url: String::new(),
            subject: None,
            unsubscribe_url: None,
            token: None,
            subscribe_url: None,
        };
        let c = canonical_string(&env);
        // No `Subject\n` line.
        assert!(!c.contains("Subject\n"), "absent subject must be omitted: {c}");
        assert_eq!(
            c,
            "Message\nm\nMessageId\nmid\nTimestamp\nts\nTopicArn\narn\nType\nNotification\n"
        );
    }

    #[test]
    fn canonical_string_subscription_confirmation_has_token_and_url() {
        let env = SnsEnvelope {
            r#type: "SubscriptionConfirmation".into(),
            message_id: "mid".into(),
            topic_arn: "arn".into(),
            message: "Please confirm".into(),
            timestamp: "ts".into(),
            signature: String::new(),
            signature_version: "1".into(),
            signing_cert_url: String::new(),
            subject: None,
            unsubscribe_url: None,
            token: Some("tok".into()),
            subscribe_url: Some("https://sns.us-east-1.amazonaws.com/?Action=Confirm".into()),
        };
        let c = canonical_string(&env);
        assert_eq!(
            c,
            "Message\nPlease confirm\n\
             MessageId\nmid\n\
             SubscribeURL\nhttps://sns.us-east-1.amazonaws.com/?Action=Confirm\n\
             Timestamp\nts\n\
             Token\ntok\n\
             TopicArn\narn\n\
             Type\nSubscriptionConfirmation\n"
        );
    }
}
