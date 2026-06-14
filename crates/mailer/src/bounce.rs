//! Postmark webhook payload types + auth header verification.
//!
//! Postmark authenticates webhooks via HTTP Basic Auth (configured in the
//! Postmark dashboard's webhook settings). We require the `Authorization`
//! header on every incoming webhook request.
//!
//! Postmark posts a JSON body whose top-level shape is discriminated by
//! `RecordType` — `"Bounce"`, `"SpamComplaint"`, plus several we ignore
//! today (`"Delivery"`, `"Open"`, `"Click"`, `"SubscriptionChange"`).
//! We use serde's internally-tagged enum to deserialize only the shapes we
//! act on; everything else falls into the [`PostmarkEvent::Other`] arm via
//! `#[serde(other)]`.
//!
//! SES-SNS support is deferred to Phase 6.

use serde::Deserialize;

/// Subset of Postmark's webhook payload we care about.
///
/// `RecordType` (`PascalCase` — Postmark's wire convention) is the
/// discriminator. Unknown record types fall into [`PostmarkEvent::Other`]
/// so we 200-OK them and move on (Postmark retries on non-2xx).
#[derive(Debug, Deserialize)]
#[serde(tag = "RecordType")]
pub enum PostmarkEvent {
    Bounce(BounceEvent),
    SpamComplaint(ComplaintEvent),
    /// Catch-all for shapes we don't handle (Delivery, Open, etc.).
    #[serde(other)]
    Other,
}

/// Bounce payload. `Type` distinguishes hard from soft — only permanent
/// types ([`bounce_type_is_permanent`]) cause a suppression.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BounceEvent {
    pub email: String,
    /// `"HardBounce"` | `"SoftBounce"` | `"Transient"` | `"BadEmailAddress"` |
    /// `"SpamNotification"` | `"Blocked"` | `"Unsubscribe"` | …
    pub r#type: String,
    #[serde(default)]
    pub description: Option<String>,
    // Postmark wires this field as `ID` (all-caps), not `Id` — overriding the
    // struct-level `PascalCase` rule.
    #[serde(rename = "ID", default)]
    pub id: Option<i64>,
}

/// Spam-complaint payload. Postmark uses `RecordType = "SpamComplaint"`
/// for end-user "this is spam" reports.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ComplaintEvent {
    pub email: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Whether a Postmark bounce `Type` is permanent (i.e., the recipient
/// must be suppressed).
///
/// Soft bounces (`SoftBounce`, `Transient`, `DnsError`, etc.) are
/// recoverable — we log them but do NOT suppress. Permanent bounces
/// (hard bounce, bad address, blocked, manual unsubscribe) are added to
/// `zeroship.email_suppressions`.
///
/// Reference: <https://postmarkapp.com/developer/api/bounce-api#bounce-types>
#[must_use]
pub fn bounce_type_is_permanent(bounce_type: &str) -> bool {
    matches!(
        bounce_type,
        "HardBounce" | "BadEmailAddress" | "Blocked" | "Unsubscribe"
    )
}

/// Verify HTTP Basic auth credentials from the `Authorization` header.
///
/// Returns `true` iff the header is `Basic <base64(expected_user:expected_password)>`.
/// The credential comparison is constant-time on the decoded bytes —
/// so attackers cannot mount a timing side-channel against the user
/// or password component. A missing header, malformed prefix, or invalid
/// base64 all yield `false` (no panic, no early return that leaks the
/// failure mode).
#[must_use]
pub fn verify_basic_auth(
    auth_header: Option<&str>,
    expected_user: &str,
    expected_password: &str,
) -> bool {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let Some(header) = auth_header else {
        return false;
    };
    let Some(provided) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(provided.trim()) else {
        return false;
    };
    let expected = format!("{expected_user}:{expected_password}");
    if decoded.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in decoded.iter().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    #[test]
    fn permanent_bounce_types_classified() {
        assert!(bounce_type_is_permanent("HardBounce"));
        assert!(bounce_type_is_permanent("BadEmailAddress"));
        assert!(bounce_type_is_permanent("Blocked"));
        assert!(bounce_type_is_permanent("Unsubscribe"));
        assert!(!bounce_type_is_permanent("SoftBounce"));
        assert!(!bounce_type_is_permanent("Transient"));
        assert!(!bounce_type_is_permanent("DnsError"));
        assert!(!bounce_type_is_permanent(""));
    }

    #[test]
    fn basic_auth_accepts_matching_credentials() {
        let header = format!("Basic {}", STANDARD.encode("user:pass"));
        assert!(verify_basic_auth(Some(&header), "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_wrong_password() {
        let header = format!("Basic {}", STANDARD.encode("user:wrong"));
        assert!(!verify_basic_auth(Some(&header), "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_wrong_user() {
        let header = format!("Basic {}", STANDARD.encode("eve:pass"));
        assert!(!verify_basic_auth(Some(&header), "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_missing_header() {
        assert!(!verify_basic_auth(None, "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_wrong_scheme() {
        // Bearer scheme, even with the right b64-encoded payload, must fail.
        let bearer = format!("Bearer {}", STANDARD.encode("user:pass"));
        assert!(!verify_basic_auth(Some(&bearer), "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_invalid_base64() {
        assert!(!verify_basic_auth(Some("Basic !!!"), "user", "pass"));
    }

    #[test]
    fn basic_auth_rejects_length_mismatch() {
        // base64("user:p") has different length than "user:pass" — length
        // check short-circuits before the byte loop.
        let header = format!("Basic {}", STANDARD.encode("user:p"));
        assert!(!verify_basic_auth(Some(&header), "user", "pass"));
    }

    #[test]
    fn parses_hard_bounce_payload() {
        let json = serde_json::json!({
            "RecordType": "Bounce",
            "Email": "bouncer@example.com",
            "Type": "HardBounce",
            "Description": "Recipient unknown",
            "ID": 12345,
        });
        let ev: PostmarkEvent = serde_json::from_value(json).expect("parse");
        match ev {
            PostmarkEvent::Bounce(b) => {
                assert_eq!(b.email, "bouncer@example.com");
                assert_eq!(b.r#type, "HardBounce");
                assert_eq!(b.description.as_deref(), Some("Recipient unknown"));
                assert_eq!(b.id, Some(12345));
            }
            other => panic!("expected Bounce, got {other:?}"),
        }
    }

    #[test]
    fn parses_spam_complaint_payload() {
        let json = serde_json::json!({
            "RecordType": "SpamComplaint",
            "Email": "angry@example.com",
            "Description": "User clicked This Is Spam",
        });
        let ev: PostmarkEvent = serde_json::from_value(json).expect("parse");
        assert!(matches!(ev, PostmarkEvent::SpamComplaint(_)));
    }

    #[test]
    fn unknown_record_type_falls_into_other() {
        let json = serde_json::json!({
            "RecordType": "Open",
            "MessageID": "abcd",
        });
        let ev: PostmarkEvent = serde_json::from_value(json).expect("parse");
        assert!(matches!(ev, PostmarkEvent::Other));
    }
}
