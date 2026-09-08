//! Live-PG mailer test — suppression check + `StdoutMailer` happy path, plus
//! the real SMTP transport against a real sink.
//!
//! A test database is REQUIRED, and so is the sink. Both used to skip silently
//! when absent, so a run against neither reported the same green as a run
//! against both. `tests/provision_test_backends.sh` stands up all of them and
//! writes the DSN into the test overlay; `PG_TEST_URL` and
//! `AUTH_TEST_SMTP_SINK` REDIRECT those two, they do not enable them.
//! Mirrors the `migrations_smoke.rs` harness: `connect(...)` returns
//! `(Client, Connection)` and the connection future must be spawned + detached
//! on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
use zeroship_mailer::stdout::StdoutMailer;
use zeroship_mailer::suppressions;
use zeroship_mailer::{Address, Email, Mailer, SmtpConfig, SmtpMailer, SmtpTls};

/// Where the provisioned sink listens, and the same relationship to
/// `AUTH_TEST_SMTP_SINK` that the compiled Postgres and Redis defaults in
/// `libs/compio-postgres/tests/common/mod.rs` and
/// `libs/compio-redis/tests/common/mod.rs` have to their own override
/// variables: the provisioner's address is the default, the variable redirects.
///
/// `tests/provision_test_backends.sh` publishes this port. If it moves there,
/// move it here in the same commit - that script's header names this file for
/// exactly that reason.
const DEFAULT_SMTP_SINK: &str = "127.0.0.1:1025";

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
/// It needs a test database for the suppression check AND a plaintext sink. It
/// USED TO SKIP when either was absent, which is how the bug above shipped: the
/// one test that drives the real transport against a real sink was green on
/// every machine without a sink, which was every machine. Both are required now,
/// and both are provisioned by `tests/provision_test_backends.sh` - "fail if
/// missing" is only honest when the thing is obtainable from the documented
/// setup.
#[compio::test]
async fn smtp_plaintext_sink_delivers_relay_forward() {
    let dsn = zeroship_core::config::test_database_url();
    let sink = zeroship_core::test_env!("AUTH_TEST_SMTP_SINK")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SMTP_SINK.to_string());
    let (host, port) = sink
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse::<u16>().expect("sink port")))
        .unwrap_or((sink.clone(), 1025));

    // Dial before building the mailer, so an absent sink names itself instead of
    // arriving as lettre's connection error inside a `spawn_blocking`.
    assert!(
        std::net::TcpStream::connect((host.as_str(), port)).is_ok(),
        "A plaintext SMTP sink is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: any SMTP server that accepts a plaintext session\n\
         \x20 dialled: {sink}\n\
         \n\
         Provision one, along with every other backend this workspace's tests\n\
         require:\n\
         \n\
         \x20 tests/provision_test_backends.sh\n\
         \n\
         It runs mailpit at the address above and prints the web inbox, which is\n\
         where you read what a send actually delivered. AUTH_TEST_SMTP_SINK\n\
         (host:port) redirects this test at a sink of your own; it does not\n\
         enable it.\n\
         \n\
         PLAINTEXT IS THE POINT: this test exists because the SmtpTls::Plaintext\n\
         arm did not, and a sink that insists on STARTTLS reproduces the failure\n\
         rather than the fix.\n\
         \n\
         There is no environment variable that makes this a skip."
    );

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
