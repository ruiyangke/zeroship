//! Resend HTTP API `Mailer` driver.
//!
//! POSTs JSON to `https://api.resend.com/emails` with Bearer auth. Pure
//! cyper — no tokio, no transitive async-runtime baggage.
//!
//! Provider docs: <https://resend.com/docs/api-reference/emails/send-email>

use async_trait::async_trait;
use compio_postgres::Client;
use serde::{Deserialize, Serialize};

use crate::mailer::{check_suppression, Address, Email, Mailer, MailerError, MessageId};

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
#[derive(Debug, Serialize)]
struct ResendRequest<'a> {
    from: String,
    to: Vec<String>,
    subject: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<ResendTag<'a>>,
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
        //    `crate::mailer::check_suppression`).
        check_suppression(db, &msg.to.email).await?;

        // 2. Build the JSON body.
        let body = ResendRequest {
            from: format_address(&msg.from),
            to: vec![format_address(&msg.to)],
            subject: &msg.subject,
            text: &msg.text,
            html: msg.html.as_deref(),
            tags: msg
                .tags
                .iter()
                .enumerate()
                .map(|(i, t)| ResendTag {
                    name: format!("zsTag{i}"),
                    value: t.as_str(),
                })
                .collect(),
        };
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
