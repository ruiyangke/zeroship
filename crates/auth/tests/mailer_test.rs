//! Live-PG mailer test — suppression check + `StdoutMailer` happy path.
//!
//! Skips silently when `AUTH_DB_URL` is unset (CI without a PG fixture).
//! Mirrors the `migrations_smoke.rs` harness: `connect(...)` returns
//! `(Client, Connection)` and the connection future must be spawned + detached
//! on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
use zeroship_auth::mailer::stdout::StdoutMailer;
use zeroship_auth::mailer::{Address, Email, Mailer, SmtpConfig, SmtpMailer, SmtpTls};
use zeroship_auth::store::{suppressions};

#[compio::test]
async fn stdout_mailer_sends_when_not_suppressed() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    let mailer = StdoutMailer;
    let email = format!("not-suppressed-{}@test", uuid::Uuid::new_v4().simple());
    let msg = Email {
        to: Address {
            email: email.clone(),
            name: Some("Test".into()),
        },
        header_to: None,
        from: Address {
            email: "auth@zeroship.ai".into(),
            name: Some("zeroship".into()),
        },
        reply_to: None,
        envelope_from: None,
        subject: "Test subject".into(),
        text: "Test body".into(),
        html: None,
        headers: vec![],
        tags: vec![],
    };
    let result = mailer.send(&client, msg).await;
    assert!(
        result.is_ok(),
        "stdout mailer should not error: {result:?}"
    );
}

#[compio::test]
async fn stdout_mailer_refuses_suppressed() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    let suppressed_email = format!("suppressed-{}@test", uuid::Uuid::new_v4().simple());
    suppressions::add(&client, &suppressed_email, "hard_bounce", None)
        .await
        .expect("add suppression");

    // Sanity: re-adding the same address should not error (idempotent
    // `ON CONFLICT DO UPDATE`), and should refresh `reason`.
    suppressions::add(&client, &suppressed_email, "complaint", Some("user clicked spam"))
        .await
        .expect("idempotent re-add");

    let mailer = StdoutMailer;
    let msg = Email {
        to: Address {
            email: suppressed_email.clone(),
            name: None,
        },
        header_to: None,
        from: Address {
            email: "auth@zeroship.ai".into(),
            name: None,
        },
        reply_to: None,
        envelope_from: None,
        subject: "Test".into(),
        text: "Body".into(),
        html: None,
        headers: vec![],
        tags: vec![],
    };
    let result = mailer.send(&client, msg).await;
    assert!(
        matches!(
            result,
            Err(zeroship_auth::mailer::MailerError::Suppressed(_))
        ),
        "expected Suppressed error, got: {result:?}"
    );

    // Cleanup so the test is re-runnable.
    client
        .execute(
            "DELETE FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&suppressed_email],
        )
        .await
        .ok();
}

/// Faithful live-sink regression for the missing plaintext SMTP transport
/// (major bug): run the REAL `SmtpMailer` send path — including the relay
/// `send_raw` envelope arm — against a PLAINTEXT SMTP sink (mailpit on
/// `:1025`). Pre-fix there was no plaintext arm: `SmtpTls::Starttls` forced
/// STARTTLS ("STARTTLS is not supported") and implicit-TLS produced "corrupt
/// message of type InvalidContentType", so every send to a plaintext sink
/// FAILED. With `SmtpTls::Plaintext` the send must succeed.
///
/// Gated on `AUTH_DB_URL` (suppression check) AND `AUTH_TEST_SMTP_SINK`
/// (`host:port` of a plaintext sink, e.g. `127.0.0.1:1025`). Skips silently
/// when either is unset so CI without a sink is unaffected.
#[compio::test]
async fn smtp_plaintext_sink_delivers_relay_forward() {
    let (Ok(dsn), Ok(sink)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("AUTH_TEST_SMTP_SINK"),
    ) else {
        eprintln!("skip (need AUTH_DB_URL + AUTH_TEST_SMTP_SINK=host:port)");
        return;
    };
    let (host, port) = sink
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse::<u16>().expect("sink port")))
        .unwrap_or((sink.clone(), 1025));

    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    // Plaintext transport — the arm that did not exist pre-fix.
    let mailer = SmtpMailer::new(&SmtpConfig {
        host,
        port,
        username: None,
        password: None,
        tls: SmtpTls::Plaintext,
    })
    .expect("plaintext smtp mailer builds");

    // Exercise the relay forward `send_raw` arm (envelope_from set), the exact
    // shape the relay-forward mailer sends — proves the plaintext path delivers
    // the privacy-preserving envelope, not just a trivial transactional message.
    let real_inbox = format!("real-{}@personal.test", uuid::Uuid::new_v4().simple());
    let msg = Email {
        to: Address {
            email: real_inbox.clone(),
            name: None,
        },
        header_to: Some(Address {
            email: "abc123@relay.zeroship.localhost".into(),
            name: Some("Shop App via relay".into()),
        }),
        from: Address {
            email: "abc123@relay.zeroship.localhost".into(),
            name: Some("Shop App via relay".into()),
        },
        reply_to: Some(Address {
            email: "abc123@relay.zeroship.localhost".into(),
            name: None,
        }),
        envelope_from: Some("bounce+xyz@relay.zeroship.localhost".into()),
        subject: "Your receipt".into(),
        text: "thanks for your order".into(),
        html: None,
        headers: vec![("X-ZS-Relay".into(), "1".into())],
        tags: vec![],
    };

    let result = mailer.send(&client, msg).await;
    assert!(
        result.is_ok(),
        "plaintext SMTP sink must accept the relay forward (pre-fix this errored \
         with STARTTLS/InvalidContentType): {result:?}"
    );
}
