//! The relay forward + bounce `Email` builders — the privacy surgery the
//! sub-spec mandates (§5.3 / §8).
//!
//! These are PURE functions: they take the parsed inbound message + the
//! resolved relay identity and return a provider-neutral [`Email`] for the
//! dedicated relay-forward mailer ([`crate::RelayForwardMailer`]) to
//! send. All the header strip/rewrite logic and the loop-guard stamp live here
//! so they can be asserted against the built message without a live SMTP path.
//!
//! ## What never leaks (§5.3)
//!
//! The real inbox (`AliasTarget::real_inbox`) appears ONLY as the SMTP `RCPT
//! TO` — i.e. `Email.to.email`, which the driver maps to the envelope
//! recipient. It is NEVER placed in any header. The outbound header set is
//! built from SCRATCH (we do not copy the inbound `headers[]`), so the
//! leak-prone inbound headers (`Return-Path`, `Sender`, `X-Original-To`,
//! `Delivered-To`, the `Received` chain, `Authentication-Results`, `ARC-*`,
//! the broken upstream `DKIM-Signature`) are dropped by construction.

use crate::inbound::{InboundMessage, RELAY_LOOP_HEADER};
use crate::types::{Address, Email};

/// Build the forwarded message to the user's real inbox (sub-spec §5.3).
///
/// - `From:` → `"{app} via relay" <{alias}@{relay_domain}>` (privacy + DMARC
///   alignment on the relay's own DKIM).
/// - `Reply-To:` → `{alias}@{relay_domain}` (a v1 reply bounces; it never
///   reaches the real inbox or the third party).
/// - envelope-from / `Return-Path` → `bounce+<opaque>@{relay_domain}` (bounces
///   of the forward route to OUR delivery webhook, never the real inbox).
/// - `X-ZS-Relay: <hop>` is stamped for loop protection.
/// - `Subject` + body are copied; NOTHING else from the inbound is carried.
///
/// `real_inbox` is the envelope recipient ONLY (`to.email`) — by construction
/// it is in no header.
#[must_use]
pub fn build_forward(
    inbound: &InboundMessage,
    alias: &str,
    real_inbox: &str,
    app_display: &str,
    relay_domain: &str,
    hop: u32,
) -> Email {
    Email {
        // Envelope recipient = the real inbox. This is the SMTP `RCPT TO` ONLY
        // — the driver never renders it into a header (the `To:` header is taken
        // from `header_to` below). This is the §5.3 invariant: the real inbox
        // appears in NO header line, only the envelope recipient.
        to: Address {
            email: real_inbox.to_owned(),
            name: None,
        },
        // Rendered `To:` header = the relay alias, NOT the real inbox. So the
        // forwarded message (`.formatted()`) carries the real inbox in no header
        // — it lives only in the envelope `RCPT TO` (§5.3). The alias is a
        // recipient-meaningful header (it matches the rewritten `From:`/
        // `Reply-To:` and is the address the user's MUA shows).
        header_to: Some(Address {
            email: alias.to_owned(),
            name: Some(format!("{app_display} via relay")),
        }),
        // Header-From is the relay alias with the app's display name.
        from: Address {
            email: alias.to_owned(),
            name: Some(format!("{app_display} via relay")),
        },
        // Replies go back to the relay (which bounces in v1).
        reply_to: Some(Address {
            email: alias.to_owned(),
            name: None,
        }),
        // MAIL FROM / Return-Path pinned to the relay bounce mailbox, keyed by
        // the alias token so a bounce maps back to the alias for suppression.
        envelope_from: Some(bounce_mailbox(alias, relay_domain)),
        subject: inbound.subject.clone(),
        text: inbound.text_body.clone(),
        html: inbound.html_body.clone(),
        // Built from scratch — the ONLY header carried is the loop stamp.
        headers: vec![(RELAY_LOOP_HEADER.to_owned(), hop.to_string())],
        tags: vec!["relay-forward".to_owned()],
        // Relay forwards are not re-driven through a notify ledger; no dedup key.
        idempotency_key: None,
    }
}

/// Build the explicit bounce we emit to the ORIGINAL sender (sub-spec §8) when
/// an inbound is known-but-unforwardable (revoked/unknown alias, or a v1 reply).
/// Emitted via the relay-forward mailer from the relay bounce mailbox; itself
/// suppression-gated by the mailer contract so we never bounce-loop.
///
/// The bounce carries NO X-ZS-Relay stamp (it is terminal, not a forward) and
/// of course no real-inbox reference.
#[must_use]
pub fn build_bounce(
    original_sender: &str,
    reason: BounceReason,
    relay_domain: &str,
) -> Email {
    let (subject, body) = reason.copy();
    Email {
        to: Address {
            email: original_sender.to_owned(),
            name: None,
        },
        // The bounce goes to the original third-party sender — there is no real
        // inbox to hide, so the rendered `To:` is the envelope recipient.
        header_to: None,
        from: Address {
            email: format!("bounce@{relay_domain}"),
            name: Some("zeroship relay".to_owned()),
        },
        reply_to: None,
        envelope_from: Some(format!("bounce@{relay_domain}")),
        subject: subject.to_owned(),
        text: body.to_owned(),
        html: None,
        headers: vec![],
        tags: vec!["relay-bounce".to_owned()],
        idempotency_key: None,
    }
}

/// Why we are bouncing back to the original sender (sub-spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BounceReason {
    /// The alias is revoked or was never minted — `revoked_at IS NULL` miss.
    AddressInactive,
    /// A user reply to a relayed message — v1 is app→user one-way.
    RepliesUnsupported,
}

impl BounceReason {
    /// `(subject, body)` copy for the bounce email.
    #[must_use]
    pub fn copy(self) -> (&'static str, &'static str) {
        match self {
            Self::AddressInactive => (
                "Delivery failed: address no longer active",
                "This address no longer forwards. The mailbox you sent to has been \
                 deactivated and your message was not delivered.",
            ),
            Self::RepliesUnsupported => (
                "Delivery failed: replies aren't supported",
                "Replies to this address aren't supported yet. Your message was not \
                 delivered. Please use the app's own contact channel.",
            ),
        }
    }
}

/// The per-alias bounce mailbox (`bounce+<token>@{relay_domain}`). Keying the
/// `+tag` to the alias token lets the delivery webhook map a forward-bounce
/// back to the alias for suppression, while keeping a single bounce mailbox.
fn bounce_mailbox(alias: &str, relay_domain: &str) -> String {
    let token = alias.split('@').next().unwrap_or(alias);
    format!("bounce+{token}@{relay_domain}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{InboundHeader, Mailbox};

    fn sample_inbound(real_inbox: &str) -> InboundMessage {
        InboundMessage {
            from_full: Mailbox {
                email: "newsletter@shop.test".into(),
                name: Some("Shop".into()),
            },
            to_full: vec![],
            original_recipient: "abc123@relay.zeroship.localhost".into(),
            subject: "Your receipt".into(),
            text_body: "thanks for your order".into(),
            html_body: Some("<p>thanks</p>".into()),
            stripped_text_reply: None,
            // Leak-prone inbound headers that MUST NOT survive onto the forward.
            headers: vec![
                InboundHeader {
                    name: "Delivered-To".into(),
                    value: real_inbox.into(),
                },
                InboundHeader {
                    name: "X-Original-To".into(),
                    value: real_inbox.into(),
                },
                InboundHeader {
                    name: "Received".into(),
                    value: format!("from mx by host for <{real_inbox}>"),
                },
                InboundHeader {
                    name: "Return-Path".into(),
                    value: format!("<{real_inbox}>"),
                },
            ],
            message_id: "mid-1".into(),
        }
    }

    #[test]
    fn forward_rewrites_from_reply_to_and_envelope() {
        let real = "real.user@personal.test";
        let inbound = sample_inbound(real);
        let fwd = build_forward(
            &inbound,
            "abc123@relay.zeroship.localhost",
            real,
            "Shop App",
            "relay.zeroship.localhost",
            1,
        );

        // From is the relay alias with "via relay" branding — NOT the third
        // party, NOT the real inbox.
        assert_eq!(fwd.from.email, "abc123@relay.zeroship.localhost");
        assert_eq!(fwd.from.name.as_deref(), Some("Shop App via relay"));
        // Reply-To routes back to the relay alias.
        assert_eq!(
            fwd.reply_to.as_ref().map(|a| a.email.as_str()),
            Some("abc123@relay.zeroship.localhost")
        );
        // Envelope-from / Return-Path pinned to the relay bounce mailbox.
        assert_eq!(
            fwd.envelope_from.as_deref(),
            Some("bounce+abc123@relay.zeroship.localhost")
        );
        // The real inbox is ONLY the envelope recipient (`to`), NEVER the
        // rendered `To:` header (`header_to`, which is the alias).
        assert_eq!(fwd.to.email, real);
        assert_eq!(
            fwd.header_to.as_ref().map(|a| a.email.as_str()),
            Some("abc123@relay.zeroship.localhost"),
            "rendered To: must be the alias, not the real inbox"
        );
        // The loop stamp rides on headers.
        assert!(fwd
            .headers
            .iter()
            .any(|(k, v)| k == "X-ZS-Relay" && v == "1"));
        // Content copied.
        assert_eq!(fwd.subject, "Your receipt");
        assert_eq!(fwd.text, "thanks for your order");
    }

    /// The privacy guarantee: NO inbound header survives onto the forward —
    /// the real inbox appears in no header line, only the envelope recipient.
    #[test]
    fn forward_strips_all_leaking_inbound_headers() {
        let real = "real.user@personal.test";
        let inbound = sample_inbound(real);
        let fwd = build_forward(
            &inbound,
            "abc123@relay.zeroship.localhost",
            real,
            "Shop App",
            "relay.zeroship.localhost",
            1,
        );

        // The ONLY header is the loop stamp. None of the leak-prone inbound
        // headers were copied, so the real inbox cannot appear in any header.
        assert_eq!(fwd.headers.len(), 1, "only X-ZS-Relay survives: {:?}", fwd.headers);
        for (name, value) in &fwd.headers {
            assert!(
                !value.contains(real),
                "real inbox leaked in header {name}: {value}"
            );
            assert!(
                !matches!(
                    name.as_str(),
                    "Delivered-To" | "X-Original-To" | "Received" | "Return-Path" | "Sender"
                ),
                "leak-prone header {name} survived"
            );
        }
        // The rendered SMTP message (the real path) contains the real inbox
        // ONLY as the envelope recipient — never in a header. We assert the
        // built struct's header surfaces here (`from`, `reply_to`, `header_to`,
        // and the `headers` vec); `smtp.rs`'s
        // `relay_forward_real_inbox_absent_from_all_rendered_headers` asserts the
        // same on the actual `.formatted()` bytes (§5.3).
        let header_blob = format!(
            "{:?}{}{}{}",
            fwd.headers,
            fwd.from.email,
            fwd.reply_to.as_ref().map(|a| a.email.clone()).unwrap_or_default(),
            fwd.header_to.as_ref().map(|a| a.email.clone()).unwrap_or_default(),
        );
        assert!(
            !header_blob.contains(real),
            "real inbox must not appear in any header surface: {header_blob}"
        );
    }

    #[test]
    fn bounce_addresses_original_sender_from_relay_bounce_mailbox() {
        let b = build_bounce(
            "newsletter@shop.test",
            BounceReason::AddressInactive,
            "relay.zeroship.localhost",
        );
        assert_eq!(b.to.email, "newsletter@shop.test");
        assert_eq!(b.from.email, "bounce@relay.zeroship.localhost");
        assert_eq!(b.envelope_from.as_deref(), Some("bounce@relay.zeroship.localhost"));
        assert!(b.subject.contains("address no longer active"));
        // A bounce never carries the loop stamp (it is terminal, not a forward).
        assert!(b.headers.is_empty());
    }

    #[test]
    fn reply_bounce_copy_says_replies_unsupported() {
        let b = build_bounce(
            "user@personal.test",
            BounceReason::RepliesUnsupported,
            "relay.zeroship.localhost",
        );
        assert!(b.subject.contains("replies aren't supported"));
        assert!(b.text.contains("aren't supported yet"));
    }
}
