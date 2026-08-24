//! A server NOTICE must reach the caller, with its severity intact.
//!
//! `AsyncMessage::Notice(DbError)` is a shipped feature -- `connection.rs`
//! routes every `NoticeResponse` to the async channel -- and nothing in the
//! suite asserted that it arrives. The one `RAISE NOTICE` elsewhere in `tests/`
//! (`socket_release.rs`) uses it to trigger a panic, not to observe delivery.
//! So if notices silently stopped being routed, every test would still pass:
//! this is the discarded-signal shape, and the signal here is one PostgreSQL
//! uses for things a caller genuinely wants -- "there is no transaction in
//! progress", constraint and cast warnings, `RAISE` from PL/pgSQL.
//!
//! The severity assertions matter for a second reason. A `NoticeResponse`
//! carries severity twice: a localized `S` field and a non-localized `V` field
//! added in PostgreSQL 9.6. Earlier today an unrecognised `V` was made
//! non-fatal (it used to discard the whole message); these tests walk the
//! ordinary path through that same code, so the common case stays covered
//! rather than only the hostile one.
//!
//! WHAT THE MUTATION ESTABLISHED, and what it did not. Flattening `WARNING`
//! into `Severity::Notice` in the severity table turns
//! `a_warning_is_not_reported_as_a_notice` red while the NOTICE case stays
//! green, so the severity assertions discriminate and the second test is a
//! real control rather than a duplicate.
//!
//! The DELIVERY half is mutation-proven too, closed later the same day once
//! `connection.rs` was no longer being edited by another agent (mutating a
//! file underneath someone else's work gives a result neither of us can
//! trust). Replacing the `AsyncMessage::Notice` send with the `log::info!`
//! arm -- so a notice is logged and dropped rather than routed -- turns ALL
//! THREE tests below red on the delivery timeout, and restoring it makes them
//! green. So a notice that stops reaching the caller now fails here.

use compio_postgres::error::Severity;
use compio_postgres::{AsyncMessage, NoTls};
use futures_util::StreamExt;
use std::time::Duration;

#[allow(dead_code)]
mod common;

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

/// Raise `level` with `message`, and return the first async message that
/// arrives.
///
/// The async sink has to be taken before `run()` is spawned, so each case
/// builds its own connection rather than sharing one.
async fn raise_and_collect(url: &str, level: &str, message: &str) -> AsyncMessage {
    let (client, mut connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    let mut messages = connection.notifications();
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();

    client
        .batch_execute(&format!("DO $$ BEGIN RAISE {level} '{message}'; END $$"))
        .await
        .expect("the DO block itself must succeed");

    compio::time::timeout(DELIVERY_TIMEOUT, messages.next())
        .await
        .expect("no async message arrived within the timeout")
        .expect("the async channel closed instead of delivering")
}

/// A NOTICE arrives, carrying its message and its parsed severity.
#[compio::test]
async fn a_server_notice_reaches_the_caller() {
    let url = test_url();

    match raise_and_collect(&url, "NOTICE", "hello-from-a-notice").await {
        AsyncMessage::Notice(notice) => {
            assert_eq!(notice.message(), "hello-from-a-notice");
            assert_eq!(
                notice.severity(),
                "NOTICE",
                "the raw S field carries the level"
            );
            assert_eq!(
                notice.parsed_severity(),
                Some(Severity::Notice),
                "the non-localized V field must parse to the matching variant"
            );
        }
        other => panic!("expected a Notice, got {other:?}"),
    }
}

/// One variable away: the SAME path at a different level must report that
/// level.
///
/// Without this, both severity assertions above could be satisfied by a
/// decoder that hardcoded NOTICE, which is the level a reader would most
/// plausibly assume for a message called a notice.
#[compio::test]
async fn a_warning_is_not_reported_as_a_notice() {
    let url = test_url();

    match raise_and_collect(&url, "WARNING", "hello-from-a-warning").await {
        AsyncMessage::Notice(notice) => {
            assert_eq!(notice.message(), "hello-from-a-warning");
            assert_eq!(notice.severity(), "WARNING");
            assert_eq!(
                notice.parsed_severity(),
                Some(Severity::Warning),
                "a WARNING must not be flattened into NOTICE"
            );
        }
        other => panic!("expected a Notice, got {other:?}"),
    }
}

/// A notice carries the SQLSTATE the server chose, not a placeholder.
///
/// `RAISE ... USING ERRCODE` lets the server pick it, so this goes through the
/// same field parsing an error would, on a message that is explicitly not an
/// error.
#[compio::test]
async fn a_notice_carries_its_sqlstate() {
    let url = test_url();
    let (client, mut connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    let mut messages = connection.notifications();
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();

    client
        .batch_execute("DO $$ BEGIN RAISE NOTICE 'coded' USING ERRCODE = '01000'; END $$")
        .await
        .expect("the DO block must succeed");

    let received = compio::time::timeout(DELIVERY_TIMEOUT, messages.next())
        .await
        .expect("no async message arrived")
        .expect("channel closed");

    match received {
        AsyncMessage::Notice(notice) => {
            assert_eq!(notice.message(), "coded");
            assert_eq!(
                notice.code().code(),
                "01000",
                "the notice must carry the SQLSTATE the server was told to use"
            );
        }
        other => panic!("expected a Notice, got {other:?}"),
    }
}
