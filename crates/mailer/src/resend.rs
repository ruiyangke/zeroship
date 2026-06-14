//! Resend HTTP API `Mailer` driver.
//!
//! POSTs JSON to `https://api.resend.com/emails` with Bearer auth. Pure
//! cyper — no tokio, no transitive async-runtime baggage.
//!
//! Provider docs: <https://resend.com/docs/api-reference/emails/send-email>

use async_trait::async_trait;
use compio_postgres::Client;
use serde::{Deserialize, Serialize};

use crate::{check_suppression, Address, Email, Mailer, MailerError, MessageId};

const RESEND_ENDPOINT: &str = "https://api.resend.com/emails";

/// Resend driver config — just the API key. The "from" address comes
/// from the per-message [`Email`], not from per-driver config.
#[derive(Debug, Clone)]
pub struct ResendConfig {
    pub api_key: String,
}

pub struct ResendMailer {
    api_key: String,
    endpoint: String,
    http: cyper::Client,
}

impl std::fmt::Debug for ResendMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the API key. `cyper::Client` doesn't implement Debug.
        f.debug_struct("ResendMailer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl ResendMailer {
    #[must_use]
    pub fn new(cfg: ResendConfig) -> Self {
        Self {
            api_key: cfg.api_key,
            endpoint: RESEND_ENDPOINT.into(),
            http: cyper::Client::new(),
        }
    }
}

/// Resend's request body shape. Only the fields we use today.
///
/// `reply_to` and `headers` are accepted by Resend's send API
/// (<https://resend.com/docs/api-reference/emails/send-email>); the driver
/// previously dropped both. Resend's HTTP API does **not** expose a
/// per-message envelope-from override, so `Email.envelope_from` cannot be
/// honoured here — which is why the relay forward path runs on the SMTP driver
/// (sub-spec §3.2/§5.2), not Resend.
#[derive(Debug, Serialize)]
struct ResendRequest<'a> {
    from: String,
    to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    subject: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<ResendTag<'a>>,
}

impl<'a> ResendRequest<'a> {
    /// Build the wire body from a provider-neutral [`Email`].
    fn from_email(msg: &'a Email) -> Self {
        let headers = if msg.headers.is_empty() {
            None
        } else {
            Some(
                msg.headers
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            )
        };
        Self {
            from: format_address(&msg.from),
            // Honour `header_to` so the rendered `To:` never exposes the real
            // inbox (defense-in-depth — Resend is rejected for relay forwards
            // because it can't pin envelope-from, but if it ever rendered a
            // forward the `To:` must still be the alias, sub-spec §5.3).
            to: vec![format_address(msg.header_to.as_ref().unwrap_or(&msg.to))],
            reply_to: msg.reply_to.as_ref().map(format_address),
            subject: &msg.subject,
            text: &msg.text,
            html: msg.html.as_deref(),
            headers,
            tags: msg
                .tags
                .iter()
                .enumerate()
                .map(|(i, t)| ResendTag {
                    name: format!("zsTag{i}"),
                    value: t.as_str(),
                })
                .collect(),
        }
    }
}

/// Resend's tag object — `{ name, value }`. We don't have a name/value
/// split today; we map every tag string to `name=zsTag<i>, value=<tag>`.
#[derive(Debug, Serialize)]
struct ResendTag<'a> {
    name: String,
    value: &'a str,
}

/// Resend's success response — `{ id: "<message-id>" }`.
#[derive(Debug, Deserialize)]
struct ResendResponse {
    id: String,
}

#[async_trait]
impl Mailer for ResendMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        // 1. Suppression-list check (mandatory contract — see
        //    `crate::check_suppression`).
        check_suppression(db, &msg.to.email).await?;

        // 2. Build the JSON body.
        let body = ResendRequest::from_email(&msg);
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| MailerError::Transport(format!("resend encode: {e}")))?;

        // 3. POST to the API.
        let res = self
            .http
            .request(http::Method::POST, self.endpoint.clone())
            .map_err(|e| MailerError::Transport(format!("resend build request: {e}")))?
            .header("authorization", format!("Bearer {}", self.api_key))
            .map_err(|e| MailerError::Transport(format!("resend auth header: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| MailerError::Transport(format!("resend ct header: {e}")))?
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| MailerError::Transport(format!("resend POST: {e}")))?;

        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| MailerError::Transport(format!("resend read body: {e}")))?;

        if !(200..300).contains(&status) {
            return Err(MailerError::Transport(format!(
                "resend → {status}: {text}"
            )));
        }
        let parsed: ResendResponse = serde_json::from_str(&text).map_err(|e| {
            MailerError::Transport(format!("resend decode: {e}; body: {text}"))
        })?;
        Ok(MessageId(parsed.id))
    }
}

/// `RFC 5322` mailbox-style string: `"Display Name" <addr@host>` or
/// just `addr@host` when no display name is set. Resend accepts both.
fn format_address(a: &Address) -> String {
    match &a.name {
        Some(n) if !n.is_empty() => format!("{n} <{}>", a.email),
        _ => a.email.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `format_address` emits a bare `addr@host` when no display name
    /// is set, and a `"Name" <addr>` form when one is — guards the
    /// branch that Resend's JSON `from` field depends on.
    #[test]
    fn format_address_with_and_without_name() {
        assert_eq!(
            format_address(&Address {
                email: "x@y.test".into(),
                name: None,
            }),
            "x@y.test"
        );
        assert_eq!(
            format_address(&Address {
                email: "x@y.test".into(),
                name: Some(String::new()),
            }),
            "x@y.test",
        );
        assert_eq!(
            format_address(&Address {
                email: "x@y.test".into(),
                name: Some("Ada".into()),
            }),
            "Ada <x@y.test>",
        );
    }

    /// §3.4 regression: a `ResendRequest` built from an `Email` carrying a
    /// `reply_to` and a `headers: [("X-ZS-Relay","1")]` serializes BOTH fields.
    /// Before the contract extension the driver dropped `headers` entirely and
    /// had no `reply_to` field, so this assertion fails pre-fix.
    #[test]
    fn resend_request_serializes_reply_to_and_headers() {
        let msg = Email {
            to: Address {
                email: "real@inbox.test".into(),
                name: None,
            },
            header_to: None,
            from: Address {
                email: "alias@relay.zeroship.ai".into(),
                name: Some("App via relay".into()),
            },
            reply_to: Some(Address {
                email: "alias@relay.zeroship.ai".into(),
                name: None,
            }),
            envelope_from: None,
            subject: "Receipt".into(),
            text: "your receipt".into(),
            html: None,
            headers: vec![("X-ZS-Relay".into(), "1".into())],
            tags: vec![],
        };
        let body = ResendRequest::from_email(&msg);
        let json = serde_json::to_value(&body).expect("serialize");

        assert_eq!(
            json["reply_to"], "alias@relay.zeroship.ai",
            "reply_to must serialize: {json}"
        );
        assert_eq!(
            json["headers"]["X-ZS-Relay"], "1",
            "X-ZS-Relay header must serialize: {json}"
        );
    }

    /// Without a `reply_to` / `headers`, both keys are omitted (the
    /// `skip_serializing_if` guards) so transactional Resend mail is unchanged.
    #[test]
    fn resend_request_omits_empty_reply_to_and_headers() {
        let msg = Email {
            to: Address {
                email: "u@test".into(),
                name: None,
            },
            header_to: None,
            from: Address {
                email: "auth@zeroship.ai".into(),
                name: None,
            },
            reply_to: None,
            envelope_from: None,
            subject: "hi".into(),
            text: "body".into(),
            html: None,
            headers: vec![],
            tags: vec![],
        };
        let json = serde_json::to_value(ResendRequest::from_email(&msg)).expect("serialize");
        assert!(json.get("reply_to").is_none(), "reply_to omitted: {json}");
        assert!(json.get("headers").is_none(), "headers omitted: {json}");
    }

    /// The Debug impl never reveals the API key — guards the
    /// "don't leak secrets in tracing/panic output" contract.
    #[test]
    fn debug_does_not_leak_api_key() {
        let m = ResendMailer::new(ResendConfig {
            api_key: "super-secret-key-do-not-leak".into(),
        });
        let s = format!("{m:?}");
        assert!(!s.contains("super-secret"), "Debug leaks key: {s}");
        assert!(s.contains("ResendMailer"), "Debug shape: {s}");
    }
}
