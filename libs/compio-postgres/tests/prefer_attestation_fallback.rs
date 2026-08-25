// This file chooses its own transport and cannot run under
// `--features suite-over-tls`, which forces every helper onto the encrypted
// server: `serialized_loop` reaches its loop through a deliberately
// unsplittable PLAINTEXT socket, and the sslmode files assert what happens
// when a connector and a config disagree. Running them in that mode would
// measure the mode, not the claim.
#![cfg(not(feature = "suite-over-tls"))]

//! `sslmode=prefer` falls back to plaintext when the supplied connector cannot
//! attest to the TLS parameters the config asks for.
//!
//! WHAT BROKE. `sslrootcert` escalates even the weak modes to chain
//! verification (`ServerVerification::select`), and the default
//! `TlsConnect::can_honor_server_verification` answers true only for
//! `ServerVerification::None`. `Encryption::first_for(Prefer)` is `Tls`, so the
//! attestation gate in `connect_raw` runs on the first leg and refused --
//! turning a DSN that had always connected into a hard error the moment
//! `sslrootcert` was added to it.
//!
//! WHY FALLING BACK IS RIGHT, and the case against it. A connector that cannot
//! honour the demanded TLS parameters means TLS-as-configured is not available,
//! and "not available" is precisely the condition `prefer` exists to handle.
//! The argument the other way is that someone who sets `sslrootcert` wants
//! verification and would rather see an error than silent plaintext -- but under
//! `prefer` they already get plaintext whenever the server does not offer TLS,
//! so they can never rely on it. A guarantee requires `verify-ca` / `verify-full`,
//! and those still refuse, which the third test below pins.
//!
//! This is a RESTORATION, not a new policy: before the attestation gate existed
//! these DSNs fell back, which is why it reads as a regression rather than a
//! tightening.

#[allow(dead_code)]
mod common;

fn test_url() -> String {
    common::test_url()
}

fn with_query(base: &str, query: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}{query}")
}

/// A root cert is named, the connector cannot attest, and `prefer` connects
/// anyway -- over plaintext, which is what the mode promises.
#[compio::test]
async fn prefer_with_a_root_cert_falls_back_when_the_connector_cannot_attest() {
    let base = test_url();
    let dsn = with_query(
        &base,
        "sslmode=prefer&sslrootcert=/etc/ssl/certs/ca-certificates.crt",
    );

    let (client, connection) = compio_postgres::connect(&dsn, common::suite_tls())
        .await
        .expect(
            "sslmode=prefer must fall back to plaintext when the connector cannot attest to the \
         verification sslrootcert asks for; refusing here breaks a DSN that connected before the \
         attestation gate existed",
        );
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the fallback connection must be usable");
    assert_eq!(row.get::<_, i32>(0), 1);
}

/// THE CONTROL, differing in one variable: the same DSN without `sslrootcert`.
///
/// It connects both before and after the fix, so it must stay green through any
/// mutation of the fallback arm. If it ever goes red alongside the test above,
/// the change broke `prefer` generally rather than the attestation case.
#[compio::test]
async fn prefer_without_a_root_cert_still_connects() {
    let base = test_url();
    let dsn = with_query(&base, "sslmode=prefer");

    let (client, connection) = compio_postgres::connect(&dsn, common::suite_tls())
        .await
        .expect("sslmode=prefer with no root cert has always connected");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let row = client
        .query_one("SELECT 2::int4", &[])
        .await
        .expect("the control connection must be usable");
    assert_eq!(row.get::<_, i32>(0), 2);
}

/// The modes that actually promise verification must still REFUSE an
/// unattesting connector. This is what keeps the fallback above from being a
/// silent downgrade of a real guarantee.
///
/// `verify-full` has no plaintext leg, so there is nothing to fall back to and
/// the refusal is the only correct outcome.
#[compio::test]
async fn verify_full_still_refuses_a_connector_that_cannot_attest() {
    let base = test_url();
    let dsn = with_query(
        &base,
        "sslmode=verify-full&sslrootcert=/etc/ssl/certs/ca-certificates.crt",
    );

    let error = compio_postgres::connect(&dsn, common::suite_tls())
        .await
        .err()
        .expect("verify-full must not connect through a connector that verifies nothing");
    let chain = common::error_chain(&error);
    assert!(
        chain.contains("verify-full"),
        "the refusal must name the mode that demanded verification: {chain}"
    );
}
