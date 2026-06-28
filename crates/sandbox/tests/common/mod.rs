//! Shared pg fixture for the sandbox DB-gated integration tests.
//!
//! The sandbox controller no longer self-migrates: the unified
//! `zeroship` schema (sandbox tables included) is owned by the
//! zeroship-migrate engine (the compose `migrate` service /
//! `ops/db-migrate.sh` applies `db/migrations/`). To keep these
//! `#[ignore]`-by-default integration tests self-contained against a
//! manually-provisioned Postgres, [`reset_and_migrate`] applies the
//! *same* migration SQL the engine does — the source-of-truth DDL files
//! under `db/migrations/` — directly, without requiring the engine.
//!
//! It drops the sandbox tables first (NOT the whole `zeroship` schema,
//! which also holds the auth/control tables) so each test starts from a
//! clean sandbox sub-schema.

use compio_postgres::{Pool, PoolConfig};

/// The platform bootstrap migration (creates the `zeroship` schema +
/// citext) followed by the 14 sandbox migrations (V0011..V0024). These
/// are the verbatim engine migration files; their bodies use ordinary
/// `--` SQL comments, so `batch_execute` runs the DDL and ignores them.
const CHANGESETS: &[&str] = &[
    // Schema + extensions bootstrap (idempotent: CREATE SCHEMA/EXTENSION
    // IF NOT EXISTS), so re-running against an already-migrated DB is a
    // no-op.
    include_str!("../../../../db/migrations/V0001__extensions_schemas.sql"),
    include_str!("../../../../db/migrations/V0011__sandbox_initial.sql"),
    include_str!("../../../../db/migrations/V0012__sandbox_status_unreachable.sql"),
    include_str!("../../../../db/migrations/V0013__sandbox_share_token_id_alphabet.sql"),
    include_str!("../../../../db/migrations/V0014__sandbox_role_split_phase3.sql"),
    include_str!("../../../../db/migrations/V0015__sandbox_events_sandbox_id_nullable.sql"),
    include_str!("../../../../db/migrations/V0016__sandbox_status_snapshot.sql"),
    include_str!("../../../../db/migrations/V0017__sandbox_snapshot_columns.sql"),
    include_str!("../../../../db/migrations/V0018__sandbox_relax_hosts_region_regex.sql"),
    include_str!("../../../../db/migrations/V0019__sandbox_wake_jobs.sql"),
    include_str!("../../../../db/migrations/V0020__sandbox_wake_jobs_hardening.sql"),
    include_str!("../../../../db/migrations/V0021__sandbox_wake_jobs_unique.sql"),
    include_str!("../../../../db/migrations/V0022__sandbox_wake_jobs_aborted_code.sql"),
    include_str!(
        "../../../../db/migrations/V0023__sandbox_wake_jobs_staging_path_missing_code.sql"
    ),
    include_str!(
        "../../../../db/migrations/V0024__sandbox_wake_jobs_agent_version_mismatch_code.sql"
    ),
];

/// Drop the sandbox tables (CASCADE pulls the `sandbox_events` partitions,
/// indexes, and FKs) so each test starts clean, then re-apply the
/// sandbox changesets into the `zeroship` schema. The `zeroship` schema
/// itself is preserved — it also holds the auth/control tables, which
/// these tests must not touch.
///
/// Idempotent: the DROPs are `IF EXISTS`, the schema/extension/role
/// creation in the migrations is guarded, and the migration DDL is the
/// same the zeroship-migrate engine applies in production.
pub async fn reset_and_migrate(url: &str) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg)
        .await
        .expect("connect for reset");
    let client = pool.get().await.expect("acquire for reset");

    // Ensure the schema exists before the targeted table drops (a fresh
    // DB may not have it yet); the changesets also CREATE IF NOT EXISTS.
    client
        .batch_execute("CREATE SCHEMA IF NOT EXISTS zeroship")
        .await
        .expect("create zeroship schema");

    // Drop only the sandbox tables. CASCADE removes the sandbox_events
    // partitions and every dependent index / FK.
    client
        .batch_execute(
            "DROP TABLE IF EXISTS \
                 zeroship.wake_jobs, \
                 zeroship.deleted_sandboxes, \
                 zeroship.sandbox_events, \
                 zeroship.shares, \
                 zeroship.sandboxes, \
                 zeroship.hosts \
             CASCADE",
        )
        .await
        .expect("drop sandbox tables");

    for sql in CHANGESETS {
        client
            .batch_execute(sql)
            .await
            .expect("apply sandbox changeset");
    }
}
