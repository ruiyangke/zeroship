//! Column lookup by name: exact match wins, and the fallback is deliberate.
//!
//! `RowIndex::__idx` tries an EXACT name match first and only then an ASCII
//! case-insensitive one. Both halves matter and neither was pinned:
//!
//! * **The ordering is load-bearing.** PostgreSQL lets one result set carry
//!   `x` and `"X"` at once. If the case-insensitive pass ran first, `get("X")`
//!   would return the value of `x` -- a silently wrong VALUE, not an error,
//!   which is the worst failure this crate has. Nothing tested the order.
//! * **The fallback is a deliberate divergence from libpq**, measured during
//!   the row audit on 2026-08-23: on `SELECT 7 AS "Weird"`, libpq's
//!   `PQfnumber` returns -1 for `weird`, `Weird` AND `WEIRD` (it downcases the
//!   search name, then compares exactly), while this driver resolves all
//!   three. It was left as-is on the grounds that the exact match always wins,
//!   so the fallback can only ever rescue a name that would otherwise be
//!   `Err(invalid column)` -- it can never redirect a lookup that already
//!   worked. That argument is only true while the ORDER holds, which is
//!   exactly what the first test here defends.
//!
//! The upstream `FIXME` in `row.rs` calls the ASCII-only rule "not really the
//! right thing to do". These tests pin what the code DOES, so changing it is a
//! decision someone takes on purpose rather than a silent drift.

use compio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// An exact name match beats a case-insensitive one, so two columns differing
/// only in case each resolve to themselves.
#[compio::test]
async fn an_exact_name_match_wins_over_the_case_insensitive_fallback() {
    let url = test_url();
    let client = connect_client(&url).await;

    // Column 0 is `x`, column 1 is `X`. A fallback-first lookup would answer
    // column 0 for BOTH names.
    let row = client
        .query_one("SELECT 1::int4 AS x, 2::int4 AS \"X\"", &[])
        .await
        .expect("two columns differing only in case");

    assert_eq!(
        row.get::<_, i32>("x"),
        1,
        "the lowercase name must resolve to its own column"
    );
    assert_eq!(
        row.get::<_, i32>("X"),
        2,
        "the uppercase name resolved to the WRONG column; the exact match must \
         be tried before the case-insensitive fallback"
    );
}

/// The case-insensitive fallback exists, and is a known divergence from libpq.
///
/// One variable away from the test above: only ONE column, so no exact match
/// is available and the fallback is the only way to resolve. If this stopped
/// working the crate would have quietly become libpq-compatible, which is a
/// change worth making on purpose rather than by accident.
#[compio::test]
async fn a_case_mismatched_name_still_resolves_when_nothing_matches_exactly() {
    let url = test_url();
    let client = connect_client(&url).await;

    let row = client
        .query_one("SELECT 7::int4 AS \"Weird\"", &[])
        .await
        .expect("a quoted mixed-case column");

    // libpq's PQfnumber answers -1 for every one of these.
    for name in ["weird", "Weird", "WEIRD"] {
        assert_eq!(
            row.get::<_, i32>(name),
            7,
            "{name} must resolve through the ASCII case-insensitive fallback"
        );
    }

    // A name that differs by more than case is still absent, so the fallback
    // is case folding and not fuzzy matching.
    row.try_get::<_, i32>("weirdo")
        .expect_err("an unrelated name must not resolve");
}

/// Duplicate names resolve to the FIRST column, matching libpq.
///
/// Measured during the row audit: `PQfnumber("x")` returns 0 on
/// `SELECT 1 AS x, 2 AS x`.
#[compio::test]
async fn a_duplicate_column_name_resolves_to_the_first() {
    let url = test_url();
    let client = connect_client(&url).await;

    let row = client
        .query_one("SELECT 1::int4 AS x, 2::int4 AS x", &[])
        .await
        .expect("two columns with the same name");

    assert_eq!(
        row.get::<_, i32>("x"),
        1,
        "a duplicated name must resolve to the first column, as libpq does"
    );
    // Both are still reachable positionally, so nothing is lost.
    assert_eq!(row.get::<_, i32>(1), 2);
}
