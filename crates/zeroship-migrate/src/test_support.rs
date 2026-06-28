//! Test-support helpers for integration suites.
//!
//! These helpers intentionally live in the crate rather than a parallel
//! `tests/support` island so every integration test can use the same public seam.

use std::sync::{Mutex, MutexGuard, OnceLock};

use compio_postgres::Client;

use crate::apply::drift::{diff_snapshots, snapshot_schema};
use crate::apply::executor::{apply, rollback, RollbackRequest, RollbackTarget};
use crate::conn::ExecutorConfig;
use crate::model::migration::Migration;
use crate::Approval;

/// One fixed advisory-lock key for tests that intentionally exercise the real
/// platform schemas (`zeroship` / `oauth_hydra`) and fixed cluster roles
/// (`zeroship_*` / `oauth_hydra`).
///
/// PostgreSQL advisory lock tags include the database OID, so every caller must
/// acquire this on the same maintenance DB connection, even when the test's
/// actual work happens in a throwaway database.
pub const GLOBAL_PLATFORM_RESOURCE_LOCK_KEY: i64 = 0x005A_534D_4947_0001;

/// Process-local guard for tests that arm the crate-global fault-injection seam.
///
/// The fault registry is intentionally in-process test state, so it needs only a
/// process-local mutex. Tests that arm faults, and peer tests in the same binary
/// that cross faulted executor boundaries, hold this guard for the whole test.
#[must_use = "hold this guard while a test depends on the fault registry being isolated"]
pub struct FaultInjectionTestLock {
    _guard: MutexGuard<'static, ()>,
}

impl std::fmt::Debug for FaultInjectionTestLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultInjectionTestLock").finish_non_exhaustive()
    }
}

/// Serialize access to the process-global fault registry and start with it clear.
pub fn acquire_fault_injection_test_lock() -> FaultInjectionTestLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::fault::disarm_all();
    FaultInjectionTestLock { _guard: guard }
}

impl Drop for FaultInjectionTestLock {
    fn drop(&mut self) {
        crate::fault::disarm_all();
    }
}

/// RAII guard holding the global platform-resource test lock on a dedicated PG
/// session. The dedicated session keeps the lock independent from any working
/// connection a test resets, drops, or points at a throwaway database.
#[must_use = "hold this guard for the whole test that touches global platform resources"]
pub struct GlobalPlatformResourceLock {
    client: Option<Client>,
    key: i64,
}

impl std::fmt::Debug for GlobalPlatformResourceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobalPlatformResourceLock")
            .field("key", &self.key)
            .field("held", &self.client.is_some())
            .finish()
    }
}

impl GlobalPlatformResourceLock {
    /// Release the advisory lock explicitly on the owning session. Dropping the
    /// guard without calling this still closes the session, which also releases
    /// the session-level advisory lock.
    pub async fn release(mut self) {
        if let Some(client) = self.client.take() {
            let _ = client
                .execute("SELECT pg_advisory_unlock($1)", &[&self.key])
                .await;
        }
    }
}

/// Acquire the cross-process gate for tests that touch real platform schemas or
/// fixed cluster roles.
#[allow(clippy::future_not_send)]
pub async fn acquire_global_platform_resource_lock(
    database_url: &str,
) -> GlobalPlatformResourceLock {
    let (client, conn) = compio_postgres::connect(database_url, compio_postgres::NoTls)
        .await
        .expect("connect dedicated global platform test lock session");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();

    client
        .execute("SELECT pg_advisory_lock($1)", &[&GLOBAL_PLATFORM_RESOURCE_LOCK_KEY])
        .await
        .expect("acquire global platform test advisory lock");

    GlobalPlatformResourceLock {
        client: Some(client),
        key: GLOBAL_PLATFORM_RESOURCE_LOCK_KEY,
    }
}

impl Drop for GlobalPlatformResourceLock {
    fn drop(&mut self) {
        // Closing the dedicated backend session releases the session-level lock.
        // Tests may call `release().await` when they want an explicit
        // `pg_advisory_unlock`; Drop is the panic-safe fallback.
        let _ = self.client.take();
    }
}

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
