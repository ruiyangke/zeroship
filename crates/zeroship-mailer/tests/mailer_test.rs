//! Live-PG mailer test — suppression check + `StdoutMailer` happy path.
//!
//! A test database is REQUIRED. This used to skip silently without one, so a
//! run against no database reported the same green as a run against one.
//! `tests/provision_test_backends.sh` stands one up and writes its address into
//! the test overlay; `PG_TEST_URL` overrides that.
//! Mirrors the `migrations_smoke.rs` harness: `connect(...)` returns
//! `(Client, Connection)` and the connection future must be spawned + detached
//! on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
use zeroship_mailer::stdout::StdoutMailer;
use zeroship_mailer::suppressions;
use zeroship_mailer::{Address, Email, Mailer, SmtpConfig, SmtpMailer, SmtpTls};

#[compio::test]
async fn stdout_mailer_sends_when_not_suppressed() {
    let dsn = zeroship_core::config::test_database_url();
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
        idempotency_key: None,
    };
    let result = mailer.send(&client, msg).await;
    assert!(
        result.is_ok(),
        "stdout mailer should not error: {result:?}"
    );
}

#[compio::test]
async fn stdout_mailer_refuses_suppressed() {
    let dsn = zeroship_core::config::test_database_url();
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
        idempotency_key: None,
    };
    let result = mailer.send(&client, msg).await;
    assert!(
        matches!(
            result,
            Err(zeroship_mailer::MailerError::Suppressed(_))
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
/// It needs a test database for the suppression check AND a plaintext sink at
/// `AUTH_TEST_SMTP_SINK` (`host:port`). It USED TO SKIP when either was absent,
/// which is how the bug above shipped: the one test that drives the real
/// transport against a real sink was green on every machine without a sink,
/// which was every machine. Both are required now.
#[compio::test]
async fn smtp_plaintext_sink_delivers_relay_forward() {
    let dsn = zeroship_core::config::test_database_url();
    let sink = zeroship_core::test_env!("AUTH_TEST_SMTP_SINK").unwrap_or_default();
    assert!(
        !sink.trim().is_empty(),
        "A plaintext SMTP sink is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: any SMTP server that accepts a plaintext session\n\
         \x20 missing: AUTH_TEST_SMTP_SINK is unset or empty (wants host:port)\n\
         \n\
         NOTHING IN THIS REPOSITORY PROVISIONS A SINK.\n\
         `tests/provision_test_backends.sh` stands up postgres and redis only.\n\
         Run one yourself - mailpit needs no configuration:\n\
         \n\
         \x20 docker run -d --name zs-mailer-test-sink -p 1025:1025 -p 8025:8025 \\\n\
         \x20   axllent/mailpit\n\
         \n\
         Then re-run with AUTH_TEST_SMTP_SINK=127.0.0.1:1025, and read what\n\
         arrived at http://127.0.0.1:8025.\n\
         \n\
         PLAINTEXT IS THE POINT: this test exists because the SmtpTls::Plaintext\n\
         arm did not, and a sink that insists on STARTTLS reproduces the failure\n\
         rather than the fix.\n\
         \n\
         There is no environment variable that makes this a skip."
    );
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
        idempotency_key: None,
    };

    let result = mailer.send(&client, msg).await;
    assert!(
        result.is_ok(),
        "plaintext SMTP sink must accept the relay forward (pre-fix this errored \
         with STARTTLS/InvalidContentType): {result:?}"
    );
}
