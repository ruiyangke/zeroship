//! Postmark **Inbound** parsed-JSON payload + the pure inspection helpers the
//! relay handler runs against it (sub-spec §4).
//!
//! This is a DIFFERENT Postmark product from the delivery-event webhook in
//! [`crate::mailer::bounce`]: inbound mail receive POSTs a parsed MIME message
//! as JSON (no MIME parser needed on our side), whereas the bounce/complaint
//! webhook posts a `RecordType`-discriminated delivery event. The two share
//! only HTTP Basic auth ([`crate::mailer::bounce::verify_basic_auth`]).
//!
//! Reference: <https://postmarkapp.com/developer/webhooks/inbound-webhook>
//!
//! Only the fields the relay acts on are deserialized; everything else Postmark
//! sends (Attachments, MessageStream, Tag, …) is ignored by serde.

use serde::Deserialize;

/// The platform loop-guard header. Stamped on every forward (sub-spec §5.3) and
/// detected on inbound (§4.3 step 3) — a re-received copy of our own forward is
/// recognised and dropped instead of forwarded again.
pub const RELAY_LOOP_HEADER: &str = "X-ZS-Relay";

/// The default SpamAssassin score at/above which an inbound is dropped
/// (sub-spec §5.4). Postmark Inbound runs SpamAssassin and surfaces the score
/// in `X-Spam-Score` / the verdict in `X-Spam-Status`.
pub const SPAM_SCORE_THRESHOLD: f64 = 5.0;

/// Max relay hops before an inbound is dropped as a loop (sub-spec §7 hop cap,
/// default N=3). Each forward stamps `X-ZS-Relay: <hop>`; an inbound whose hop
/// count is at/above this cap is dropped (a relay↔relay loop, possibly across
/// two aliases, is bounded here even if a single re-injection would otherwise
/// be re-forwarded with an incremented hop).
pub const RELAY_MAX_HOPS: u32 = 3;

/// Postmark Inbound's parsed message shape. PascalCase is Postmark's wire
/// convention (matching [`crate::mailer::bounce`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InboundMessage {
    /// The third party that sent mail to the alias. In v1 this is who a bounce
    /// is addressed to and (for v2) who a reply would route back to.
    pub from_full: Mailbox,
    #[serde(default)]
    pub to_full: Vec<Mailbox>,
    /// The literal RCPT-TO the message was delivered to — the alias to resolve.
    /// Postmark fills this even with catch-all/MailboxHash routing, and it MAY
    /// carry a `+tag`/MailboxHash suffix and mixed case, so it is run through
    /// [`normalize_alias`] before the exact-match lookup (sub-spec §4.4a).
    pub original_recipient: String,
    pub subject: String,
    pub text_body: String,
    #[serde(default)]
    pub html_body: Option<String>,
    #[serde(default)]
    pub stripped_text_reply: Option<String>,
    /// Full inbound header set (`[{ Name, Value }]`) — inspected for the
    /// loop-guard marker and the SpamAssassin / authentication verdicts. We do
    /// NOT copy these onto the forward; the outbound header set is built from
    /// scratch (sub-spec §5.3).
    #[serde(default)]
    pub headers: Vec<InboundHeader>,
    /// Stable per-message id — the idempotency/replay dedup key (sub-spec §7.1).
    #[serde(rename = "MessageID")]
    pub message_id: String,
}

/// `{ Email, Name }` mailbox. `MailboxHash` and other fields are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Mailbox {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// One `{ Name, Value }` inbound header.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InboundHeader {
    pub name: String,
    pub value: String,
}

impl InboundMessage {
    /// Case-insensitive lookup of the first header with `name`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// `true` when the inbound carries our loop-guard marker — i.e. it is a
    /// forward of a forward we already sent (sub-spec §4.3 step 3 / §7).
    #[must_use]
    pub fn has_loop_marker(&self) -> bool {
        self.header(RELAY_LOOP_HEADER).is_some()
    }

    /// The hop count carried in the `X-ZS-Relay` loop-guard header, if present
    /// (sub-spec §7). `None` ⇒ no marker (a fresh inbound). `Some(n)` ⇒ this
    /// message has already traversed the relay `n` times.
    ///
    /// A present-but-unparseable marker is treated as the cap [`RELAY_MAX_HOPS`]
    /// so a malformed loop stamp still trips the loop guard rather than being
    /// re-forwarded.
    #[must_use]
    pub fn loop_hop(&self) -> Option<u32> {
        self.header(RELAY_LOOP_HEADER)
            .map(|v| v.trim().parse::<u32>().unwrap_or(RELAY_MAX_HOPS))
    }

    /// `true` when this inbound should be dropped as a relay loop (sub-spec §7
    /// hop cap): it carries an `X-ZS-Relay` hop count at/above `max_hops`. A
    /// fresh inbound (no marker) is never a loop. The outbound forward of a
    /// below-cap inbound is stamped with `hop + 1` (see [`next_hop`]).
    #[must_use]
    pub fn is_relay_loop(&self, max_hops: u32) -> bool {
        self.loop_hop().is_some_and(|hop| hop >= max_hops)
    }

    /// The hop count to stamp on the outbound forward of THIS inbound
    /// (sub-spec §7): the inbound hop + 1, or 1 for a fresh (un-stamped)
    /// inbound. Callers MUST have already rejected loops via [`is_relay_loop`].
    #[must_use]
    pub fn next_hop(&self) -> u32 {
        self.loop_hop().map_or(1, |hop| hop.saturating_add(1))
    }

    /// `true` when this inbound is a user **reply** to a relayed message rather
    /// than a fresh app→user message (sub-spec §8 / O7). v1 is one-way, so a
    /// reply is bounced with "replies not yet supported".
    ///
    /// Detection mirrors the sub-spec mechanism: a reply carries `In-Reply-To`
    /// or `References` threading headers (set by the user's MUA when they hit
    /// reply on our forward), OR Postmark extracted a `StrippedTextReply` (its
    /// reply-quote stripper only fires on detected replies).
    #[must_use]
    pub fn is_reply(&self) -> bool {
        self.header("In-Reply-To").is_some()
            || self.header("References").is_some()
            || self
                .stripped_text_reply
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    /// `true` when the inbound should be dropped as spam / sender-authentication
    /// failure BEFORE forwarding (sub-spec §5.4): re-originating it under the
    /// relay's own DKIM would vouch for spam with the relay's reputation.
    ///
    /// Triggers on:
    /// - `X-Spam-Status: Yes` (Postmark's SpamAssassin verdict), or
    /// - `X-Spam-Score` ≥ `threshold`, or
    /// - the original-sender `Authentication-Results` showing `dmarc=fail`
    ///   (a domain disowning its own message — we will not re-sign it).
    #[must_use]
    pub fn is_spam_or_unauthenticated(&self, threshold: f64) -> bool {
        if let Some(status) = self.header("X-Spam-Status") {
            // SpamAssassin emits `Yes, score=…` / `No, score=…`; match the
            // leading verdict token case-insensitively.
            if status
                .trim_start()
                .split([',', ' '])
                .next()
                .is_some_and(|tok| tok.eq_ignore_ascii_case("yes"))
            {
                return true;
            }
        }
        if let Some(score) = self.header("X-Spam-Score") {
            if score
                .trim()
                .parse::<f64>()
                .is_ok_and(|v| v >= threshold)
            {
                return true;
            }
        }
        if let Some(authres) = self.header("Authentication-Results") {
            if authres.to_ascii_lowercase().contains("dmarc=fail") {
                return true;
            }
        }
        false
    }
}

/// Normalize a literal RCPT-TO (`OriginalRecipient`) into the canonical alias
/// key before the exact-match lookup (sub-spec §4.4a):
///
/// - lowercase the whole address (local + domain),
/// - strip any `+tag` / MailboxHash subaddress suffix from the local part.
///
/// Returns `None` for an address with no `@`. Because alias tokens are minted
/// from lowercase base36 (sub-spec §4.4a), lowercasing the local part is never
/// lossy. `relay_email` is always stored lowercased so this matches it exactly.
#[must_use]
pub fn normalize_alias(original_recipient: &str) -> Option<String> {
    let (local, domain) = original_recipient.trim().rsplit_once('@')?;
    let local = local.split('+').next().unwrap_or(local);
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    Some(format!(
        "{}@{}",
        local.to_ascii_lowercase(),
        domain.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_with_headers(headers: Vec<(&str, &str)>) -> InboundMessage {
        InboundMessage {
            from_full: Mailbox {
                email: "sender@third.party".into(),
                name: None,
            },
            to_full: vec![],
            original_recipient: "abc123@relay.zeroship.localhost".into(),
            subject: "Receipt".into(),
            text_body: "hi".into(),
            html_body: None,
            stripped_text_reply: None,
            headers: headers
                .into_iter()
                .map(|(n, v)| InboundHeader {
                    name: n.into(),
                    value: v.into(),
                })
                .collect(),
            message_id: "mid-1".into(),
        }
    }

    #[test]
    fn parses_postmark_inbound_payload() {
        let json = serde_json::json!({
            "FromFull": { "Email": "newsletter@shop.test", "Name": "Shop" },
            "ToFull": [{ "Email": "abc123@relay.zeroship.localhost" }],
            "OriginalRecipient": "abc123@relay.zeroship.localhost",
            "Subject": "Your receipt",
            "TextBody": "thanks for your order",
            "HtmlBody": "<p>thanks</p>",
            "Headers": [{ "Name": "X-Spam-Status", "Value": "No" }],
            "MessageID": "11111111-2222-3333-4444-555555555555",
            "Attachments": [],
        });
        let m: InboundMessage = serde_json::from_value(json).expect("parse");
        assert_eq!(m.from_full.email, "newsletter@shop.test");
        assert_eq!(m.original_recipient, "abc123@relay.zeroship.localhost");
        assert_eq!(m.subject, "Your receipt");
        assert_eq!(m.message_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(m.header("X-Spam-Status"), Some("No"));
    }

    #[test]
    fn normalize_lowercases_and_strips_subaddress() {
        // Mixed case + +tag both fold to the canonical stored alias.
        assert_eq!(
            normalize_alias("Token+SampleHash@Relay.Zeroship.AI").as_deref(),
            Some("token@relay.zeroship.ai")
        );
        assert_eq!(
            normalize_alias("abc123@relay.zeroship.localhost").as_deref(),
            Some("abc123@relay.zeroship.localhost")
        );
        // No @ ⇒ None; empty local/domain ⇒ None.
        assert_eq!(normalize_alias("not-an-email"), None);
        assert_eq!(normalize_alias("@relay.test"), None);
        assert_eq!(normalize_alias("token@"), None);
    }

    #[test]
    fn loop_marker_detected_case_insensitively() {
        assert!(msg_with_headers(vec![("X-ZS-Relay", "1")]).has_loop_marker());
        assert!(msg_with_headers(vec![("x-zs-relay", "3")]).has_loop_marker());
        assert!(!msg_with_headers(vec![("X-Spam-Status", "No")]).has_loop_marker());
    }

    /// §7 hop cap: a fresh inbound (no marker) is never a loop and stamps hop 1;
    /// a below-cap hop forwards with hop+1; a hop at/above the cap is a loop and
    /// is dropped. This is the regression for the finding that the hop count was
    /// hardcoded to a constant `1` and never incremented/compared.
    #[test]
    fn hop_count_increments_and_caps() {
        // Fresh inbound: no marker ⇒ not a loop, next forward is hop 1.
        let fresh = msg_with_headers(vec![("X-Spam-Status", "No")]);
        assert_eq!(fresh.loop_hop(), None);
        assert!(!fresh.is_relay_loop(RELAY_MAX_HOPS));
        assert_eq!(fresh.next_hop(), 1);

        // Below cap: hop 1 ⇒ not a loop, next forward is hop 2 (NOT a constant).
        let hop1 = msg_with_headers(vec![("X-ZS-Relay", "1")]);
        assert_eq!(hop1.loop_hop(), Some(1));
        assert!(!hop1.is_relay_loop(RELAY_MAX_HOPS));
        assert_eq!(hop1.next_hop(), 2);

        // Below cap: hop 2 ⇒ next forward is hop 3.
        let hop2 = msg_with_headers(vec![("X-ZS-Relay", "2")]);
        assert_eq!(hop2.next_hop(), 3);
        assert!(!hop2.is_relay_loop(RELAY_MAX_HOPS));

        // At the cap (default 3): a loop ⇒ dropped, never re-forwarded.
        let hop3 = msg_with_headers(vec![("X-ZS-Relay", "3")]);
        assert_eq!(hop3.loop_hop(), Some(3));
        assert!(hop3.is_relay_loop(RELAY_MAX_HOPS));

        // Above the cap is also a loop.
        assert!(msg_with_headers(vec![("X-ZS-Relay", "9")]).is_relay_loop(RELAY_MAX_HOPS));

        // A present-but-unparseable marker is treated as the cap (trips the
        // guard) so a malformed stamp can't be re-forwarded forever.
        let garbage = msg_with_headers(vec![("X-ZS-Relay", "not-a-number")]);
        assert_eq!(garbage.loop_hop(), Some(RELAY_MAX_HOPS));
        assert!(garbage.is_relay_loop(RELAY_MAX_HOPS));
    }

    #[test]
    fn reply_detected_via_threading_headers_or_stripped_reply() {
        assert!(msg_with_headers(vec![("In-Reply-To", "<abc@relay>")]).is_reply());
        assert!(msg_with_headers(vec![("References", "<abc@relay>")]).is_reply());
        let mut m = msg_with_headers(vec![]);
        m.stripped_text_reply = Some("my reply text".into());
        assert!(m.is_reply());
        // A blank stripped reply does not count.
        let mut blank = msg_with_headers(vec![]);
        blank.stripped_text_reply = Some("   ".into());
        assert!(!blank.is_reply());
        // A fresh app→user message is not a reply.
        assert!(!msg_with_headers(vec![("X-Spam-Status", "No")]).is_reply());
    }

    #[test]
    fn spam_status_yes_is_flagged() {
        assert!(msg_with_headers(vec![("X-Spam-Status", "Yes")])
            .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
        // SpamAssassin's `Yes, score=9.1` long form.
        assert!(msg_with_headers(vec![("X-Spam-Status", "Yes, score=9.1")])
            .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
        assert!(!msg_with_headers(vec![("X-Spam-Status", "No")])
            .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
    }

    #[test]
    fn high_spam_score_is_flagged() {
        assert!(msg_with_headers(vec![("X-Spam-Score", "7.4")])
            .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
        assert!(!msg_with_headers(vec![("X-Spam-Score", "1.2")])
            .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
    }

    #[test]
    fn dmarc_fail_is_flagged() {
        assert!(msg_with_headers(vec![(
            "Authentication-Results",
            "mx.relay.test; spf=fail smtp.mailfrom=x; dkim=fail; dmarc=fail",
        )])
        .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
        assert!(!msg_with_headers(vec![(
            "Authentication-Results",
            "mx.relay.test; spf=pass; dkim=pass; dmarc=pass",
        )])
        .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
    }

    #[test]
    fn clean_message_is_not_flagged() {
        assert!(!msg_with_headers(vec![
            ("X-Spam-Status", "No"),
            ("X-Spam-Score", "0.4"),
            ("Authentication-Results", "mx; dmarc=pass"),
        ])
        .is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD));
    }
}
