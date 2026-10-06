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

    // Every typed field, `payload_bytes` included, is compared across both
    // backends and against `parity::expected_typed_projection()`, which derives
    // the expectation from `parity::TYPED_BYTES_RAW`, the bytes the caller wrote;
    // `bytes_column_stores_raw_bytes_on_postgres` (below) reads the stored cell
    // with a query that does not go through the SDK.
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
