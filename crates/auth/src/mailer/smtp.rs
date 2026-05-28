//! SMTP `Mailer` driver via the BLOCKING `lettre::SmtpTransport`.
//!
//! The whole `transport.send(...)` call is wrapped in
//! [`compio::runtime::spawn_blocking`] so the `compio/io_uring` event loop
//! isn't blocked during the SMTP handshake + write. This is the canonical
//! zero-tokio way to use lettre in our stack — `lettre`'s native async
//! transports are tied to either tokio or async-std, neither of which
//! belongs in the zeroship event loop.
//!
//! ## Features
//!
//! We enable `smtp-transport + rustls-tls + builder + pool + hostname`
//! and crucially DROP `tokio1*` — without `tokio1`, lettre's optional
//! `tokio1_crate` dependency stays out of the build graph.
//!
//! `pool` keeps a small connection pool so successive sends don't
//! reopen the SMTP/STARTTLS handshake; the pool itself is internal to
//! the `SmtpTransport` value and is `Send + Sync + Clone`.

use std::sync::Arc;

use async_trait::async_trait;
use compio_postgres::Client;
use lettre::{
    message::{header::ContentType, Mailbox, MultiPart},
    transport::smtp::{authentication::Credentials, SmtpTransport},
    Message, Transport,
};

use crate::mailer::{check_suppression, Address, Email, Mailer, MailerError, MessageId};

/// Driver-specific config; parsed from `AUTH_SMTP_*` env vars in [`crate::config`].
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `true` ⇒ open plaintext then upgrade via STARTTLS (typical port 587).
    /// `false` ⇒ open implicit-TLS / SMTPS (typical port 465).
    pub use_starttls: bool,
}

/// SMTP-backed `Mailer`. The wrapped `SmtpTransport` is internally
/// `Clone + Send + Sync` (it holds an `Arc<Pool>`); the outer `Arc`
/// keeps the build cheap to share across handler tasks.
pub struct SmtpMailer {
    transport: Arc<SmtpTransport>,
}

impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer").finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// Build a transport from the parsed config.
    ///
    /// # Errors
    ///
    /// [`MailerError::Config`] if lettre rejects the relay host (e.g.
    /// the rustls connector can't construct a server-name from `host`).
    pub fn new(cfg: &SmtpConfig) -> Result<Self, MailerError> {
        let builder = if cfg.use_starttls {
            SmtpTransport::starttls_relay(&cfg.host)
        } else {
            SmtpTransport::relay(&cfg.host)
        }
        .map_err(|e| MailerError::Config(format!("smtp build({}): {e}", cfg.host)))?
        .port(cfg.port);

        let builder = match (&cfg.username, &cfg.password) {
            (Some(u), Some(p)) => builder.credentials(Credentials::new(u.clone(), p.clone())),
            _ => builder,
        };

        Ok(Self {
            transport: Arc::new(builder.build()),
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        // 1. Suppression-list check (every Mailer impl MUST do this — see
        //    `crate::mailer::check_suppression` doc).
        check_suppression(db, &msg.to.email).await?;

        // 2. Translate our provider-neutral `Email` to a lettre `Message`.
        let message = build_lettre_message(&msg)?;

        // 3. Send on the blocking pool. `spawn_blocking` returns a
        //    `JoinHandle<Result<T, Box<dyn Any + Send>>>` — outer error
        //    is a join failure (panic in the blocking closure), inner
        //    is lettre's SMTP error.
        let transport = self.transport.clone();
        let send_result = compio::runtime::spawn_blocking(move || transport.send(&message)).await;

        let response = match send_result {
            Ok(inner) => inner.map_err(|e| MailerError::Transport(format!("smtp send: {e}")))?,
            Err(_panic) => {
                return Err(MailerError::Transport(
                    "smtp send: blocking task panicked".into(),
                ));
            }
        };
        // lettre's `Response::code()` is `Copy`; the resulting message id is
        // opaque to callers (only used in logs / delivery-webhook correlation).
        Ok(MessageId(format!("smtp-{:?}", response.code())))
    }
}

/// Translate a provider-neutral [`Email`] into a `lettre::Message`.
///
/// Arbitrary `msg.headers` are intentionally ignored in U2 — lettre's
/// typed `Header` trait requires per-header `Display`/`FromStr` impls,
/// so smuggling caller-supplied strings is awkward. The two callers
/// today (magic-link / verification / reset) don't need custom headers;
/// we'll wire them through once a real consumer asks for it.
fn build_lettre_message(msg: &Email) -> Result<Message, MailerError> {
    let from_mbox = format_mailbox(&msg.from)?;
    let to_mbox = format_mailbox(&msg.to)?;

    let builder = Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(msg.subject.clone());

    if let Some(html) = &msg.html {
        builder
            .multipart(MultiPart::alternative_plain_html(
                msg.text.clone(),
                html.clone(),
            ))
            .map_err(|e| MailerError::Transport(format!("smtp build message: {e}")))
    } else {
        builder
            .header(ContentType::TEXT_PLAIN)
            .body(msg.text.clone())
            .map_err(|e| MailerError::Transport(format!("smtp build message: {e}")))
    }
}

/// Parse our [`Address`] into a lettre `Mailbox`. lettre's `AddressError`
/// type isn't part of the surface we want to expose, so we wrap it as
/// `MailerError::Config` (an invalid recipient address is a config-shape
/// failure — bad `from` in the env, or a caller bug).
fn format_mailbox(a: &Address) -> Result<Mailbox, MailerError> {
    let parsed: lettre::Address = a
        .email
        .parse()
        .map_err(|e| MailerError::Config(format!("invalid address {:?}: {e}", a.email)))?;
    Ok(Mailbox::new(a.name.clone(), parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SmtpMailer::new` builds without error against a plausible
    /// STARTTLS relay config — no SMTP server is contacted, this only
    /// exercises the lettre relay-builder + rustls server-name parser
    /// path. Catches the "wrong feature set" regression where
    /// `rustls-tls` is dropped (then `relay()` returns an Err here).
    #[test]
    fn smtp_mailer_builds_for_starttls_relay() {
        let m = SmtpMailer::new(&SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: Some("u".into()),
            password: Some("p".into()),
            use_starttls: true,
        });
        assert!(m.is_ok(), "starttls relay build: {:?}", m.err());
    }

    #[test]
    fn smtp_mailer_builds_for_implicit_tls() {
        let m = SmtpMailer::new(&SmtpConfig {
            host: "smtp.example.com".into(),
            port: 465,
            username: None,
            password: None,
            use_starttls: false,
        });
        assert!(m.is_ok(), "implicit tls relay build: {:?}", m.err());
    }

    /// `build_lettre_message` translates text-only `Email` cleanly
    /// (no html alternative) — guards the single-part branch.
    #[test]
    fn build_lettre_message_text_only() {
        let msg = Email {
            to: Address {
                email: "to@zeroship.test".into(),
                name: Some("To".into()),
            },
            from: Address {
                email: "from@zeroship.test".into(),
                name: Some("From".into()),
            },
            subject: "hi".into(),
            text: "body".into(),
            html: None,
            headers: vec![],
            tags: vec![],
        };
        let m = build_lettre_message(&msg).expect("build");
        let raw = String::from_utf8_lossy(&m.formatted()).to_string();
        assert!(raw.contains("Subject: hi"), "raw: {raw}");
        assert!(raw.contains("body"), "raw: {raw}");
    }

    /// `build_lettre_message` emits a `multipart/alternative` when an
    /// HTML body is provided — guards the html branch.
    #[test]
    fn build_lettre_message_with_html() {
        let msg = Email {
            to: Address {
                email: "to@zeroship.test".into(),
                name: None,
            },
            from: Address {
                email: "from@zeroship.test".into(),
                name: None,
            },
            subject: "hi".into(),
            text: "plain".into(),
            html: Some("<p>html</p>".into()),
            headers: vec![],
            tags: vec![],
        };
        let m = build_lettre_message(&msg).expect("build");
        let raw = String::from_utf8_lossy(&m.formatted()).to_string();
        assert!(
            raw.contains("multipart/alternative"),
            "expected multipart, raw: {raw}"
        );
    }

    /// Invalid email addresses surface as `MailerError::Config` — guards
    /// the lettre `AddressError` → `MailerError` translation.
    #[test]
    fn invalid_address_maps_to_config_error() {
        let bad = Address {
            email: "not-an-email".into(),
            name: None,
        };
        let err = format_mailbox(&bad).unwrap_err();
        assert!(
            matches!(err, MailerError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }
}
