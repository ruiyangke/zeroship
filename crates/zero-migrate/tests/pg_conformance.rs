//! Live-Postgres conformance for the `PgDevSession` driver.
//!
//! Proves the in-crate `PgDevSession` — the TEST-ONLY [`SqlSession`] over the blocking
//! `postgres` crate that every resurrected `*_pg` scenario drives — passes the engine's
//! `driver::conformance` suite (session-pinning, transaction-visibility, `exec_text`
//! semantics, error+SQLSTATE mapping). It is the FIRST external consumer of the seam's
//! conformance surface, so a driver that silently pools connections, swallows errors, or
//! sends the wrong param format is caught here BEFORE any scenario relies on it.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`: skips cleanly when unset so DB-free CI stays
//! green.

mod support;

use support::PgDevSession;
use zero_migrate::driver::conformance;

/// A unique scratch identifier per test so parallel runs never collide on the temp
/// table name (temp tables are session-scoped, but the name is still per-session).
fn scratch_ident(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!("zm_conf_{tag}_{n}")
}

#[compio::test]
async fn pg_dev_session_passes_seam_conformance() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let scratch = scratch_ident("pin");
    conformance::run(&session, &scratch)
        .await
        .expect("PgDevSession must pass the full driver::conformance suite");
}
