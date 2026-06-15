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
    address::Envelope,
    message::{
        header::{ContentType, Header, HeaderName, HeaderValue},
        Mailbox, MultiPart,
    },
    transport::smtp::{authentication::Credentials, SmtpTransport},
    Message, Transport,
};

use crate::{check_suppression, Address, Email, Mailer, MailerError, MessageId};

/// A raw `(name, value)` header lettre 0.11 has no one-call API for. lettre's
/// typed `Header` trait takes a value implementing `Header`, so the relay's
/// platform-controlled headers (`X-ZS-Relay` — closed, ASCII, never
/// caller-derived) ride through this opaque newtype.
///
/// `parse` is only used by lettre when reading a header off a parsed message;
/// the relay only ever *writes* via [`RawHeader::for_pair`], so `parse`/`name`
/// carry placeholder values and the real name/value live per-instance.
#[derive(Clone)]
struct RawHeader {
    name: HeaderName,
    value: String,
}

impl RawHeader {
    /// Build a header from a `(name, value)` pair. The name must be valid ASCII
    /// (the relay only emits `X-`-prefixed platform headers, so this never
    /// fails in practice; a bad name surfaces as `MailerError::Config`).
    fn for_pair(name: &str, value: &str) -> Result<Self, MailerError> {
        let name = HeaderName::new_from_ascii(name.to_owned())
            .map_err(|e| MailerError::Config(format!("invalid header name {name:?}: {e}")))?;
        Ok(Self {
            name,
            value: value.to_owned(),
        })
    }
}

impl Header for RawHeader {
    fn name() -> HeaderName {
        // Unused placeholder — the real name is carried per-instance and
        // emitted by `display()`. lettre only calls this associated fn for the
        // typed-header registry, which the relay does not rely on.
        HeaderName::new_from_ascii_str("X-ZS-Placeholder")
    }

    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            name: HeaderName::new_from_ascii_str("X-ZS-Relay"),
            value: s.to_owned(),
        })
    }

    fn display(&self) -> HeaderValue {
        // `HeaderValue::new` RFC-2047-encodes + line-folds the value — safe for
        // arbitrary content (no injection), correct for our ASCII X- headers.
        HeaderValue::new(self.name.clone(), self.value.clone())
    }
}

/// Transport encryption mode for an [`SmtpConfig`].
///
/// Three-way so the relay/transactional SMTP legs can target a real MTA
/// (TLS) **or** a plaintext dev/test sink (mailpit) — the two-way
/// `use_starttls: bool` had no plaintext arm, so a plaintext sink was
/// undeliverable (STARTTLS → "STARTTLS is not supported"; implicit-TLS →
/// "corrupt message of type InvalidContentType").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SmtpTls {
    /// Open plaintext then upgrade with STARTTLS (typical port 587). The
    /// default — real submission MTAs require it.
    #[default]
    Starttls,
    /// Open implicit TLS / SMTPS from the first byte (typical port 465).
    Implicit,
    /// No TLS at all — plaintext SMTP. ONLY for dev/test sinks (mailpit on
    /// `:1025`); never a real MTA. Maps to lettre's `builder_dangerous`.
    Plaintext,
}

/// Driver-specific config; parsed from `AUTH_SMTP_*` env vars by the auth
/// binary's config layer (`zeroship-auth`'s `config` module).
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Transport encryption mode (STARTTLS / implicit-TLS / plaintext).
    pub tls: SmtpTls,
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
        // `starttls_relay`/`relay` build a rustls connector (can fail on a bad
        // server-name); `builder_dangerous` is infallible — it opens a
        // plaintext socket with no TLS, the only path that can reach a
        // plaintext dev/test sink (mailpit).
        let builder = match cfg.tls {
            SmtpTls::Starttls => SmtpTransport::starttls_relay(&cfg.host)
                .map_err(|e| MailerError::Config(format!("smtp build({}): {e}", cfg.host)))?,
            SmtpTls::Implicit => SmtpTransport::relay(&cfg.host)
                .map_err(|e| MailerError::Config(format!("smtp build({}): {e}", cfg.host)))?,
            SmtpTls::Plaintext => SmtpTransport::builder_dangerous(&cfg.host),
        }
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
        //    `crate::check_suppression` doc).
        check_suppression(db, &msg.to.email).await?;

        // 2. Translate our provider-neutral `Email` to a lettre `Message`.
        let message = build_lettre_message(&msg)?;

        // 3. Send on the blocking pool. `spawn_blocking` returns a
        //    `JoinHandle<Result<T, Box<dyn Any + Send>>>` — outer error
        //    is a join failure (panic in the blocking closure), inner
        //    is lettre's SMTP error.
        //
        //    When `envelope_from` is set (relay forwards), we take the
        //    explicit-envelope `send_raw` arm so MAIL FROM / Return-Path is
        //    pinned to the relay bounce mailbox, independent of the `From:`
        //    header (sub-spec §3.3/§5.3). Transactional auth mail leaves
        //    `envelope_from` unset and uses `send(&message)`, whose envelope is
        //    derived from `From:` exactly as before — byte-for-byte unchanged.
        let transport = self.transport.clone();
        let send_result = if msg.envelope_from.is_some() {
            let envelope =
                build_envelope(msg.envelope_from.as_deref(), &msg.from.email, &msg.to.email)?;
            let raw = message.formatted();
            compio::runtime::spawn_blocking(move || transport.send_raw(&envelope, &raw)).await
        } else {
            compio::runtime::spawn_blocking(move || transport.send(&message)).await
        };

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
/// `msg.reply_to` (when set) becomes a `Reply-To:` header, and `msg.headers`
/// are emitted verbatim via the [`RawHeader`] escape hatch — in practice the
/// relay's closed, ASCII `X-ZS-Relay` loop marker (sub-spec §5.3/§7).
/// Transactional auth mail sets neither and is byte-for-byte unchanged.
fn build_lettre_message(msg: &Email) -> Result<Message, MailerError> {
    let from_mbox = format_mailbox(&msg.from)?;
    // The rendered `To:` header comes from `header_to` when set (relay forwards
    // set it to the alias so the real inbox is NEVER rendered in a header —
    // sub-spec §5.3); otherwise it is the envelope recipient (transactional mail
    // — byte-for-byte unchanged). The envelope `RCPT TO` is always `msg.to`
    // (built in `build_envelope`), independent of this header.
    let to_mbox = format_mailbox(msg.header_to.as_ref().unwrap_or(&msg.to))?;

    let mut builder = Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(msg.subject.clone());

    if let Some(reply_to) = &msg.reply_to {
        builder = builder.reply_to(format_mailbox(reply_to)?);
    }
    for (name, value) in &msg.headers {
        builder = builder.header(RawHeader::for_pair(name, value)?);
    }

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

/// Build the SMTP envelope (MAIL FROM + RCPT TO) for the explicit-envelope
/// `send_raw` path. `envelope_from`, when `Some`, pins MAIL FROM /
/// `Return-Path` to the relay bounce mailbox — **independent of the message's
/// `From:` header** — so forwarded-mail bounces route to the relay's own bounce
/// handler, never to the real inbox or the original sender (sub-spec §5.3).
/// When `None`, MAIL FROM falls back to the header-from, matching lettre's
/// default `Message::envelope()` behaviour.
fn build_envelope(
    envelope_from: Option<&str>,
    header_from: &str,
    rcpt: &str,
) -> Result<Envelope, MailerError> {
    let mail_from: lettre::Address = envelope_from
        .unwrap_or(header_from)
        .parse()
        .map_err(|e| MailerError::Config(format!("invalid envelope-from: {e}")))?;
    let to: lettre::Address = rcpt
        .parse()
        .map_err(|e| MailerError::Config(format!("invalid envelope rcpt {rcpt:?}: {e}")))?;
    Envelope::new(Some(mail_from), vec![to])
        .map_err(|e| MailerError::Config(format!("envelope build: {e}")))
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
            tls: SmtpTls::Starttls,
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
            tls: SmtpTls::Implicit,
        });
        assert!(m.is_ok(), "implicit tls relay build: {:?}", m.err());
    }

    /// Regression for the missing plaintext transport arm (major bug): a
    /// plaintext dev/test sink (mailpit on `:1025`) MUST build a no-TLS
    /// transport. Pre-fix there was no `SmtpTls::Plaintext` and both arms
    /// forced TLS, so a plaintext sink was undeliverable. `builder_dangerous`
    /// is infallible, so the only thing under test here is that the plaintext
    /// arm exists and constructs a mailer.
    #[test]
    fn smtp_mailer_builds_for_plaintext_sink() {
        let m = SmtpMailer::new(&SmtpConfig {
            host: "127.0.0.1".into(),
            port: 1025,
            username: None,
            password: None,
            tls: SmtpTls::Plaintext,
        });
        assert!(m.is_ok(), "plaintext sink build: {:?}", m.err());
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
            header_to: None,
            from: Address {
                email: "from@zeroship.test".into(),
                name: Some("From".into()),
            },
            reply_to: None,
            envelope_from: None,
            subject: "hi".into(),
            text: "body".into(),
            html: None,
            headers: vec![],
            tags: vec![],
            idempotency_key: None,
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
            header_to: None,
            from: Address {
                email: "from@zeroship.test".into(),
                name: None,
            },
            reply_to: None,
            envelope_from: None,
            subject: "hi".into(),
            text: "plain".into(),
            html: Some("<p>html</p>".into()),
            headers: vec![],
            tags: vec![],
            idempotency_key: None,
        };
        let m = build_lettre_message(&msg).expect("build");
        let raw = String::from_utf8_lossy(&m.formatted()).to_string();
        assert!(
            raw.contains("multipart/alternative"),
            "expected multipart, raw: {raw}"
        );
    }

    /// §3.4 regression (SMTP test 1): `build_lettre_message` emits a
    /// `Reply-To:` line and the `X-ZS-Relay:` header from `msg.reply_to` /
    /// `msg.headers`. Before the contract extension lettre dropped both
    /// (`reply_to` did not exist; `headers` were "intentionally ignored"), so
    /// this assertion fails pre-fix.
    #[test]
    fn build_lettre_message_emits_reply_to_and_relay_header() {
        let msg = Email {
            to: Address {
                email: "real@inbox.test".into(),
                name: None,
            },
            // Relay forwards render `To:` from `header_to` (the alias), so the
            // real inbox lives only in the envelope `RCPT TO`.
            header_to: Some(Address {
                email: "alias@relay.zeroship.ai".into(),
                name: Some("App via relay".into()),
            }),
            from: Address {
                email: "alias@relay.zeroship.ai".into(),
                name: Some("App via relay".into()),
            },
            reply_to: Some(Address {
                email: "alias@relay.zeroship.ai".into(),
                name: None,
            }),
            envelope_from: Some("bounce+abc@relay.zeroship.ai".into()),
            subject: "Receipt".into(),
            text: "your receipt".into(),
            html: None,
            headers: vec![("X-ZS-Relay".into(), "1".into())],
            tags: vec![],
            idempotency_key: None,
        };
        let m = build_lettre_message(&msg).expect("build");
        let raw = String::from_utf8_lossy(&m.formatted()).to_string();
        assert!(
            raw.contains("Reply-To:") && raw.contains("alias@relay.zeroship.ai"),
            "expected Reply-To header, raw: {raw}"
        );
        assert!(
            raw.contains("X-ZS-Relay:") && raw.contains("X-ZS-Relay: 1"),
            "expected X-ZS-Relay header, raw: {raw}"
        );
    }

    /// §5.3 / §10 regression (the privacy invariant the round was missing):
    /// the rendered SMTP message of a relay forward contains the real inbox in
    /// **no** header line — it lives only in the envelope `RCPT TO`. Pre-fix,
    /// `build_lettre_message` rendered `To: real@inbox.test` from `msg.to`, so
    /// `.formatted()` carried the real inbox in a header line and this assertion
    /// failed. With `header_to` set to the alias, the rendered `To:` is the
    /// alias and the real inbox appears nowhere in the headers.
    #[test]
    fn relay_forward_real_inbox_absent_from_all_rendered_headers() {
        let real = "real.user@personal.test";
        let inbound = crate::inbound::InboundMessage {
            from_full: crate::inbound::Mailbox {
                email: "newsletter@shop.test".into(),
                name: Some("Shop".into()),
            },
            to_full: vec![],
            original_recipient: "abc123@relay.zeroship.ai".into(),
            subject: "Your receipt".into(),
            text_body: "thanks for your order".into(),
            html_body: None,
            stripped_text_reply: None,
            headers: vec![],
            message_id: "mid-1".into(),
        };
        // Build the forward the REAL way (the path the handler runs), then
        // render it the REAL way (the path the SMTP driver runs).
        let fwd = crate::forward::build_forward(
            &inbound,
            "abc123@relay.zeroship.ai",
            real,
            "Shop App",
            "relay.zeroship.ai",
            1,
        );
        let m = build_lettre_message(&fwd).expect("build");
        let raw = String::from_utf8_lossy(&m.formatted()).to_string();

        // The whole rendered message (headers + body) must not contain the real
        // inbox. (The body is app content; the real inbox is never in it either.)
        assert!(
            !raw.contains(real),
            "real inbox leaked into rendered forward: {raw}"
        );
        // And specifically the `To:` header is the alias, NOT the real inbox —
        // while the envelope RCPT TO (asserted via build_envelope below) IS the
        // real inbox.
        assert!(
            raw.contains("To:") && raw.contains("abc123@relay.zeroship.ai"),
            "To: header must render the alias, raw: {raw}"
        );
        // The envelope (what send_raw actually uses) keeps the real inbox as the
        // RCPT TO — the one and only place it is allowed to appear.
        let env = build_envelope(
            fwd.envelope_from.as_deref(),
            &fwd.from.email,
            &fwd.to.email,
        )
        .expect("envelope");
        assert_eq!(
            env.to().len(),
            1,
            "exactly one RCPT TO"
        );
        assert_eq!(
            env.to()[0].to_string(),
            real,
            "the real inbox is the envelope RCPT TO"
        );
    }

    /// §3.4 regression (SMTP test 2): `build_envelope` with an explicit
    /// `envelope_from` returns an `Envelope` whose `from()` is the relay bounce
    /// mailbox — **not** the `From:` alias and **not** the real inbox. This is
    /// the proof that `send_raw`'s envelope pins MAIL FROM / Return-Path
    /// independent of the header-From (sub-spec §5.3 Return-Path guarantee).
    #[test]
    fn build_envelope_pins_bounce_mailbox_as_mail_from() {
        let env = build_envelope(
            Some("bounce+x@relay.zeroship.ai"),
            "alias@relay.zeroship.ai",
            "real@inbox.test",
        )
        .expect("envelope");
        let from = env.from().expect("envelope has a from").to_string();
        assert_eq!(
            from, "bounce+x@relay.zeroship.ai",
            "MAIL FROM must be the bounce mailbox, got {from}"
        );
        assert_ne!(from, "alias@relay.zeroship.ai", "must not be the From alias");
        assert_ne!(from, "real@inbox.test", "must NEVER be the real inbox");
    }

    /// When `envelope_from` is `None`, `build_envelope` falls back to the
    /// header-from, matching lettre's default `Message::envelope()` behaviour —
    /// transactional mail's envelope is unchanged.
    #[test]
    fn build_envelope_falls_back_to_header_from() {
        let env = build_envelope(None, "auth@zeroship.ai", "u@test").expect("envelope");
        assert_eq!(
            env.from().expect("from").to_string(),
            "auth@zeroship.ai",
            "envelope-from defaults to header-from when unset"
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
