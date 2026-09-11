//! SQLite parity contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

#[test]
fn parity_matrix_sqlite_seed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.seed, parity::expected_seed_projection());
    });
}

#[test]
fn parity_matrix_sqlite_transaction_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.tx, parity::expected_tx_projection());
    });
}

#[test]
fn parity_matrix_sqlite_typed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.typed, parity::expected_typed_projection());
    });
}
