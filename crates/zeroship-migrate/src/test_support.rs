//! Test-support helpers for integration suites.
//!
//! These helpers intentionally live in the crate rather than a parallel
//! `tests/support` island so every integration test can use the same public seam.

use compio_postgres::Client;

use crate::apply::drift::{diff_snapshots, snapshot_schema};
use crate::apply::executor::{apply, rollback, RollbackRequest, RollbackTarget};
use crate::conn::ExecutorConfig;
use crate::model::migration::Migration;
use crate::Approval;

/// Apply a reversible migration set, roll it back, and apply it again, asserting
/// structural equality at the two fixed points:
///
/// 1. baseline `S0`
/// 2. after apply `S1`
/// 3. after rollback `S2 == S0`
/// 4. after re-apply `S3 == S1`
///
/// This proves rollback returns the project schema to baseline and that replaying
/// the same set recreates the same schema shape. It deliberately requires real
/// `down` SQL; irreversible migrations are out of scope and should fail through
/// the canonical rollback path rather than getting a fabricated inverse here.
#[allow(clippy::future_not_send)]
pub async fn assert_reversible_replay_pg(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) {
    let s0 = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("snapshot baseline schema");

    apply(conn, cfg, migrations, Approval::Approved, "round-trip-apply")
        .await
        .expect("apply migration set");
    let s1 = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("snapshot after apply");

    rollback(
        conn,
        cfg,
        migrations,
        RollbackRequest::new(RollbackTarget::All),
        Approval::Approved,
        "round-trip-rollback",
    )
    .await
    .expect("rollback migration set");
    let s2 = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("snapshot after rollback");
    let rollback_drift = diff_snapshots(&s0, &s2);
    assert!(
        rollback_drift.is_clean(),
        "rollback did not return to baseline; structural drift: {rollback_drift:?}"
    );

    apply(conn, cfg, migrations, Approval::Approved, "round-trip-reapply")
        .await
        .expect("re-apply migration set");
    let s3 = snapshot_schema(conn, &cfg.project_schema)
        .await
        .expect("snapshot after re-apply");
    let replay_drift = diff_snapshots(&s1, &s3);
    assert!(
        replay_drift.is_clean(),
        "re-apply did not recreate the applied schema; structural drift: {replay_drift:?}"
    );
}
