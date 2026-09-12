//! PostgreSQL parity contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

/// Postgres and the dev SQLite tier must hand `env.db` callers the same JSON.
///
/// Included in the native data suite. Missing PostgreSQL prerequisites fail
/// through `require_pg`; they never turn this comparison into a skipped leg.
#[compio::test]
async fn parity_matrix_pg_matches_sqlite_projection() {
    let (_postgres, pg_url) = require_pg().await;
    let sqlite_dir = tempfile::tempdir().expect("create sqlite parity dir");

    let app = crate::tests::fixtures::test_app_id!();

    // The SQLite leg keeps the dev app id on purpose - its tempdir isolates it,
    // and the matrix is meant to write the file a `pnpm dev` app writes. The
    // Postgres leg gets this test's own id: the two matrix tests here shared
    // schema `default` and dropped it out from under each other in parallel.
    let sqlite = parity::run_matrix(&parity::sqlite_url(&sqlite_dir), parity::DEV_APP_ID);
    let pg = parity::run_matrix(&pg_url, &app);

    assert_eq!(pg.seed, sqlite.seed);
    assert_eq!(pg.tx, sqlite.tx);

    // THE BYTES DIVERGENCE IS GONE, and it used to be pinned right here. Until
    // `crud::bytes_pass` landed, this block excluded `payload_bytes` from the
    // comparison and pinned the two OBSERVED values instead: `M3EyKzd3PT0=` on
    // Postgres and `3q2+7w==` on SQLite. The first of those is the base64 of the
    // second - the write path had no `bytes` branch, so the SDK's base64 string
    // was bound as text at a `bytea` column, Postgres parsed it in ESCAPE format
    // and stored the 8 ASCII characters, and the read path (which is correct)
    // base64'd those 8 bytes back out. Both pins were copied from what the code
    // returned, which is why neither ever went red.
    //
    // What replaces them is not another pin: `expected_typed_projection` derives
    // the expectation from `parity::TYPED_BYTES_RAW`, the four bytes the caller
    // wrote, and `bytes_column_stores_raw_bytes_on_postgres` (below) reads the
    // stored cell with a query that does not go through the SDK.
    assert_eq!(
        pg.typed, sqlite.typed,
        "every typed field must project identically on both backends"
    );
    assert_eq!(
        pg.typed,
        parity::expected_typed_projection(),
        "and both must match the independently-derived expectation"
    );
}
