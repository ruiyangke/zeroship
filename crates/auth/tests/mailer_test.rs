//! Live-PG mailer test — suppression check + `StdoutMailer` happy path.
//!
//! Skips silently when `AUTH_DB_URL` is unset (CI without a PG fixture).
//! Mirrors the `migrations_smoke.rs` harness: `connect(...)` returns
//! `(Client, Connection)` and the connection future must be spawned + detached
//! on the compio runtime or queries hang.

use compio_postgres::{connect, NoTls};
use zeroship_auth::mailer::stdout::StdoutMailer;
use zeroship_auth::mailer::{Address, Email, Mailer};
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
            "DELETE FROM auth.email_suppressions WHERE email = $1::citext",
            &[&suppressed_email],
        )
        .await
        .ok();
}
