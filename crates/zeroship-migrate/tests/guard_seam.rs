//! The `MigrationGuard` trait boundary (multi-engine abstraction P0, design
//! 2026-06-21 §2.2 L3).
//!
//! Pins the per-engine line-1 seam that replaced the three `if dialect == Sqlite`
//! guard branches (engine.rs `plan()`, executor.rs apply FIRST PASS, the
//! `SqlGuard::check` SQLite arm):
//!
//! - `PgGuard` (the Postgres line-1) still **denies** exactly the deny-list set
//!   it did before — COPY … PROGRAM (RCE) and a `CREATE EXTENSION` outside the
//!   allowlist — and still **passes** benign DDL, now through the neutral
//!   `GuardOutcome` (no PG-specific `classes` on the seam, H2).
//! - `SqliteDescriptorGuard` (the SQLite line-1) **trusts** descriptor-diff DDL
//!   (its `check` returns the empty clean outcome) — the apply/plan path on
//!   SQLite feeds it descriptor-generated DDL, which must NOT be rejected.
//! - The raw-untrusted-SQLite-SQL fail-closed survives on `SqlGuard` itself: a
//!   SQLite-keyed `SqlGuard` (the PG guard mis-handed a SQLite config) still
//!   refuses with `SqliteRawSqlRejected` rather than mis-parsing — the defensive
//!   property the engine no longer relies on (it routes SQLite through
//!   `SqliteDescriptorGuard`), kept as a backstop for the wrong caller.
//! - `guard_for` / `backend.guard()` select the right per-engine guard by
//!   dialect, with no by-name SQLite knowledge in the core.

use zeroship_migrate::guard::{
    guard_for, GuardConfig, GuardError, MigrationGuard, PgGuard, SqlGuard, SqliteDescriptorGuard,
};

/// A realistic PG project guard: project schema `project_acme`, extension
/// allowlist = `pgcrypto` + `uuid-ossp` (mirrors the `guard_security` matrix).
fn pg_guard() -> PgGuard {
    PgGuard::from_config(
        GuardConfig::confined("project_acme")
            .with_extension_allowlist(vec!["pgcrypto".to_string(), "uuid-ossp".to_string()]),
    )
}

// ---------------------------------------------------------------------------
// PgGuard — the deny-list still denies exactly what it did (byte-identical).
// ---------------------------------------------------------------------------

#[test]
fn pg_guard_denies_copy_program_rce() {
    let err = pg_guard()
        .check("COPY project_acme.t TO PROGRAM 'sh -c \"curl evil\"'")
        .expect_err("COPY … PROGRAM is RCE — must be denied through the seam");
    assert!(
        matches!(err, GuardError::Denied { .. }),
        "expected Denied, got: {err:?}"
    );
}

#[test]
fn pg_guard_denies_create_extension_outside_allowlist() {
    let err = pg_guard()
        .check("CREATE EXTENSION dblink")
        .expect_err("dblink is outside the allowlist — must be denied through the seam");
    assert!(
        matches!(err, GuardError::Denied { .. }),
        "expected Denied, got: {err:?}"
    );
}

#[test]
fn pg_guard_passes_benign_ddl_with_neutral_outcome() {
    let outcome = pg_guard()
        .check(r#"CREATE TABLE "project_acme"."users" (id text primary key)"#)
        .expect("benign in-schema DDL must pass the PG line-1");
    // Neutral seam: a non-destructive CREATE TABLE, no advisories. The PG-specific
    // `classes` are NOT on `GuardOutcome` (H2) — they stay inside `SqlGuard`.
    assert!(!outcome.destructive, "CREATE TABLE is not destructive");
}

#[test]
fn pg_guard_flags_destructive_through_seam() {
    let outcome = pg_guard()
        .check(r#"DROP TABLE "project_acme"."users""#)
        .expect("a DROP TABLE passes (flagged, not denied)");
    assert!(
        outcome.destructive,
        "DROP TABLE must be flagged destructive on the neutral seam"
    );
}

// ---------------------------------------------------------------------------
// SqliteDescriptorGuard — the descriptor-diff path is trusted (empty outcome).
// ---------------------------------------------------------------------------

#[test]
fn sqlite_descriptor_guard_passes_descriptor_create_table() {
    let guard = SqliteDescriptorGuard::new();
    // Descriptor-generated DDL is trusted by construction (author-boundary line-1 +
    // backend-authorizer line-2). The engine's apply/plan path feeds exactly this.
    let outcome = guard
        .check("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .expect("descriptor-generated SQLite DDL is trusted — empty clean outcome");
    assert!(!outcome.destructive, "trusted descriptor path: not destructive");
    assert!(
        outcome.advisories.is_empty(),
        "trusted descriptor path: no advisories"
    );
}

// ---------------------------------------------------------------------------
// The raw-untrusted-SQLite fail-closed survives on SqlGuard (the backstop).
// ---------------------------------------------------------------------------

#[test]
fn sqlite_keyed_sqlguard_rejects_raw_sql_backstop() {
    // A raw, untrusted SQLite string handed to the PG guard (a SQLite-keyed config)
    // is refused rather than mis-parsed by libpg_query — the defensive property the
    // engine no longer relies on (it routes SQLite through SqliteDescriptorGuard).
    let guard = SqlGuard::new(GuardConfig::confined_sqlite("project_acme"));
    let err = guard
        .check("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .expect_err("raw SQLite SQL handed to the PG guard must fail closed");
    assert!(
        matches!(err, GuardError::SqliteRawSqlRejected),
        "expected SqliteRawSqlRejected, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// guard_for selects the per-engine guard by dialect (no by-name SQLite in core).
// ---------------------------------------------------------------------------

#[test]
fn guard_for_pg_runs_the_deny_list() {
    let guard = guard_for(
        &GuardConfig::confined("project_acme")
            .with_extension_allowlist(vec!["pgcrypto".to_string()]),
    );
    // The PG-selected guard denies the deny-list set …
    assert!(matches!(
        guard.check("COPY project_acme.t TO PROGRAM 'id'"),
        Err(GuardError::Denied { .. })
    ));
    // … and passes benign in-schema DDL.
    assert!(guard
        .check(r#"CREATE TABLE "project_acme"."t" (id text primary key)"#)
        .is_ok());
}

#[test]
fn guard_for_sqlite_trusts_descriptor_ddl() {
    // The SQLite-selected guard trusts descriptor-diff DDL (the apply/plan path),
    // so apply is NOT broken by a raw-rejection on legitimate descriptor SQL.
    let guard = guard_for(&GuardConfig::confined_sqlite("project_acme"));
    let outcome = guard
        .check("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .expect("SQLite descriptor path trusts the engine-generated DDL");
    assert!(!outcome.destructive);
    assert!(outcome.advisories.is_empty());
}
