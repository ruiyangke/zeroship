//! The `MigrationBackend` dialect seam (SQLite-parity design §2.0/§2.1, M3).
//!
//! The executor's apply/rollback **orchestration** — partition versioned vs
//! repeatable, the drift/tamper gate, squash/expand gates, `order_pending`, the
//! FIRST/SECOND pass, the repeatable phase, rollback selection + reverse-topo
//! ordering — is dialect-agnostic and stays single-sourced in
//! [`crate::executor`]. Everything the orchestration touches that is
//! **dialect-coupled** lives behind this trait:
//!
//! - **connection / session I/O** — the project lock
//!   (`pg_advisory_lock(hashtext($1))`), the GUC snapshot/restore
//!   (`current_setting`/`set_config`), the unconditional `RESET ROLE`, and
//!   transaction begin/commit/rollback;
//! - **the per-migration confined apply** — the txn path (`BEGIN; SET LOCAL …;
//!   SET LOCAL ROLE migrator; <up>; RESET ROLE; INSERT journal; COMMIT`), the
//!   non-txn two-phase path, the non-txn crash recovery, and the rollback `down`;
//! - **journal row I/O** — the shared-sequence net-state reads
//!   (`applied`/`superseded_versions`/`latest_completed_checksums`), the
//!   immutability bootstrap, and the event inserts — exposed as **dialect-neutral
//!   owned row structs** (`AppliedEntry`, …), never a `compio_postgres::Row`;
//! - **parse-time non-txn idempotency validation** — PG calls `pg_query::parse`
//!   directly; a SQLite backend rejects `transaction:false` at the dialect
//!   boundary instead, so this MUST sit behind the trait (no raw `pg_query::parse`
//!   in the generic `apply_locked` body);
//! - **drift schema introspection** — `snapshot_schema` over
//!   `information_schema`/`pg_catalog` (PG) vs `sqlite_master` + PRAGMAs (SQLite);
//!   the checksum/tamper comparison itself is dialect-agnostic and stays generic
//!   ([`crate::drift::check_checksum_drift`]).
//!
//! **P1 (this phase): Postgres is the FIRST AND ONLY impl.** [`PostgresBackend`]
//! is the regression bar — every method below moves the EXISTING executor /
//! journal / drift code behind the trait, behavior-identical. No SQLite code
//! exists yet; a `SqliteBackend` is added later (design §2.1.1) WITHOUT forking
//! the executor, because the orchestration is already generic over this trait.
//!
//! The trait is used through **static dispatch** (`<B: MigrationBackend>`), so
//! native `async fn` in trait (Rust ≥ 1.75) is used directly — no boxing, no
//! `dyn`, no `async-trait` allocation on the apply hot path.

use compio_postgres::Client;

use crate::backend_sqlite::SqliteRebuildSpec;
use crate::db::ExecutorConfig;
use crate::drift::{DriftError, SchemaSnapshot};
use crate::executor::{ApplyError, RollbackError};
use crate::journal::{self, AppliedEntry, JournalError};
use crate::migration::Migration;
use zeroship_schema::query::SqlDialect;

/// The session GUCs / settings the executor must restore on exit so its
/// per-apply settings never leak onto the pooled/long-lived connection (H2).
///
/// Dialect-neutral by construction: an owned snapshot the backend produces and
/// later consumes. The Postgres impl carries the three GUC strings
/// (`statement_timeout`/`lock_timeout`/`search_path`); a SQLite impl would carry
/// whatever its session restore needs (or nothing). The generic executor never
/// inspects the contents — it only round-trips the value back to the backend.
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    /// PG `statement_timeout` GUC text (e.g. `"60s"`). Empty for a backend that
    /// has no such setting.
    pub statement_timeout: String,
    /// PG `lock_timeout` GUC text.
    pub lock_timeout: String,
    /// PG `search_path` GUC text.
    pub search_path: String,
}

/// The dialect seam over execution I/O, journal I/O, parse-time non-txn
/// validation, and drift introspection (design §2.0/§2.1, M3).
///
/// See the module docs for the full rationale. P1 has exactly one impl,
/// [`PostgresBackend`]; the methods are the EXACT pre-seam Postgres code, moved
/// verbatim, so the existing PG suite is the regression bar.
#[allow(async_fn_in_trait)]
pub trait MigrationBackend {
    /// The SQL dialect this backend targets. Drives the dialect-boundary rejects
    /// in the generic body (e.g. `transaction:false` on SQLite, design §2.3/L3).
    fn dialect(&self) -> SqlDialect;

    /// The **per-engine line-1 guard** (multi-engine abstraction P0, design
    /// §2.2 L3). The generic apply path runs `backend.guard().check(up)` over
    /// every pending migration in the FIRST PASS — no `if dialect == Sqlite`, no
    /// inline `SqlGuard::new`. Postgres returns a [`PgGuard`](crate::guard::PgGuard)
    /// (libpg_query deny-list); SQLite returns a
    /// [`SqliteDescriptorGuard`](crate::guard::SqliteDescriptorGuard) (the trusted
    /// descriptor-diff path). The returned [`GuardOutcome`](crate::guard::GuardOutcome)
    /// / [`GuardError`](crate::guard::GuardError) are dialect-neutral.
    fn guard(&self) -> &dyn crate::guard::MigrationGuard;

    // -- connection / session I/O -------------------------------------------

    /// Acquire the project apply-serialization lock (PG `pg_advisory_lock`).
    async fn acquire_project_lock(&self, project_id: &str) -> Result<(), ApplyError>;

    /// Release the project apply-serialization lock (PG `pg_advisory_unlock`).
    async fn release_project_lock(&self, project_id: &str) -> Result<(), ApplyError>;

    /// Snapshot the session settings the apply will override, for restore on exit.
    async fn snapshot_session(&self) -> Result<SessionSnapshot, ApplyError>;

    /// Restore the settings captured by [`snapshot_session`](Self::snapshot_session).
    /// Includes the leading unconditional `RESET ROLE` (PG).
    async fn restore_session(&self, snap: &SessionSnapshot) -> Result<(), ApplyError>;

    /// Drop any per-apply privilege confinement back to the connecting role
    /// (PG `RESET ROLE`), unconditionally. Best-effort: logs on failure, never
    /// fails the apply (mirrors the pre-seam `apply` L1 backstop).
    async fn reset_role_best_effort(&self);

    // -- per-migration confined apply ---------------------------------------

    /// Apply ONE migration transactionally (design §2.3): `BEGIN; SET LOCAL …;
    /// SET LOCAL ROLE migrator; <up>; RESET ROLE; INSERT journal; (edges); COMMIT`.
    /// DDL + journal row commit atomically. Used for the default path AND the
    /// repeatable re-apply (`kind="repeatable"`, `supersedes` empty).
    async fn apply_up_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<(), ApplyError>;

    /// Configure the session for the **non-txn** path (no transaction to scope
    /// `SET LOCAL` within): pin `search_path` + the mandatory timeouts. Restored
    /// on exit by [`restore_session`](Self::restore_session).
    async fn configure_session_non_txn(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<(), ApplyError>;

    /// Apply ONE migration non-transactionally (design §2.4): two-phase
    /// `started` marker → run the confined `<up>` → immutable `completed` row +
    /// clear marker, with idempotent crash recovery when `had_inflight`.
    /// Returns `true` iff this was a recovery.
    async fn apply_up_non_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        had_inflight: bool,
        supersedes: &[&str],
    ) -> Result<bool, ApplyError>;

    /// Roll back ONE migration transactionally (design §5): `BEGIN; SET LOCAL …;
    /// SET LOCAL ROLE migrator; <down>; RESET ROLE; INSERT rolled_back; COMMIT`.
    /// `down` + the `rolled_back` append commit atomically.
    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError>;

    // -- parse-time validation ----------------------------------------------

    /// Validate that a **non-transactional** migration's `up` is idempotent
    /// (re-runnable by crash recovery). PG parses with `pg_query` and enforces
    /// `IF NOT EXISTS` on `CREATE INDEX CONCURRENTLY` / `ALTER TYPE … ADD VALUE`
    /// and forbids bare DML; a SQLite backend rejects `transaction:false`
    /// outright (no non-txn DDL exists on SQLite, design §2.3/L3).
    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError>;

    // -- journal row I/O (dialect-neutral owned rows) -----------------------

    /// Bootstrap the journal + immutability constructs (idempotent).
    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError>;

    /// The net-applied + lone-`started` journal entries (the drift/pending input).
    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError>;

    /// The versions covered by a net-applied squash (the supersession net-state).
    async fn superseded_versions(&self, cfg: &ExecutorConfig)
        -> Result<Vec<String>, JournalError>;

    /// The latest journaled `completed` checksum per identity — the repeatable
    /// re-run oracle.
    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError>;

    // -- DB-coupled validation / introspection ------------------------------

    /// The checksum/tamper drift report over the journal (dialect-agnostic
    /// comparison; the journal read underneath is dialect-coupled, hence here).
    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<crate::drift::ChecksumDriftReport, DriftError>;

    /// Introspect the live schema for the structural drift surface (PG
    /// `information_schema`/`pg_catalog`; SQLite `sqlite_master` + PRAGMAs).
    async fn snapshot_schema(&self, cfg: &ExecutorConfig)
        -> Result<SchemaSnapshot, DriftError>;

    // -- preconditions (DB-coupled; full seam is post-P1) -------------------

    /// Evaluate a migration's preconditions read-only under the apply lock.
    /// Behind the trait so the generic body never holds a concrete connection;
    /// the PG impl delegates to [`crate::precondition::evaluate`].
    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::executor::PreconditionVerdict, ApplyError>;

    // -- declarative-only structured ops (P6a) ------------------------------

    /// Apply ONE structured SQLite 12-step table REBUILD atomically with
    /// confinement + journal it (design §2.4), the dialect-coupled drive behind the
    /// generic declarative apply path. A rebuild is NOT a plain `up` statement — it
    /// is an engine-mode structured operation (drop-stale-temp / CREATE new / copy /
    /// drop old / rename / replay captured dependents) with `foreign_keys` toggles
    /// straddling the transaction — so it cannot flow through
    /// [`apply_up_transactional`](Self::apply_up_transactional); the engine drives it
    /// here, after the destructive/approval gate it already runs for the rebuild's
    /// `destructive`-flagged journal migration.
    ///
    /// Rebuilds exist ONLY on the SQLite dialect: the SQLite differ emits them for
    /// the existing-table changes SQLite has no native `ALTER` for, and `plan.renames`
    /// is empty on SQLite so this is reached only with a SQLite-produced spec. The
    /// Postgres impl rejects it ([`ApplyError::Backend`]): PG never produces a
    /// `SqliteRebuild` (its differ uses native `ALTER` / expand-contract), so a rebuild
    /// reaching the PG backend is a routing bug, surfaced as a clear error rather than
    /// a silent pass.
    ///
    /// # Errors
    /// The dialect-neutral [`ApplyError::Backend`] on a rebuild failure (FK-check
    /// abort, confinement denial, DDL failure, or a poisoned connection — the SQLite
    /// transaction is rolled back, leaving the original table intact), or — on the PG
    /// backend — the unreachable-routing reject.
    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), ApplyError>;

    /// The Postgres connection for the **not-yet-abstracted** online expand-contract
    /// rename drive (`run_expand` / `run_backfill`, PG `pg_advisory_xact_lock` +
    /// paged `UPDATE`; deferred to P6c). Returns `Some` for the Postgres backend,
    /// `None` for SQLite.
    ///
    /// This is the single escape hatch the generic declarative apply path uses to
    /// reach the PG-shaped `run_expand_with_lock`. It is only ever consulted to drive
    /// `plan.renames`, which the SQLite differ keeps EMPTY (a SQLite rename is routed
    /// to a [`rebuild_one`](Self::rebuild_one), never expand-contract) — so the
    /// `None` arm is never reached with a non-empty rename set, and `run_expand` is
    /// never called on a SQLite backend (design §3.3 / P6a CONDITION ii / H1).
    fn expand_conn(&self) -> Option<&Client>;
}

/// The Postgres [`MigrationBackend`] — P1's first and only impl.
///
/// Holds a borrowed [`Client`]; every method is the EXACT pre-seam executor /
/// journal / drift code, moved here verbatim. This is the regression bar: the
/// entire existing PG suite must pass unchanged.
#[derive(Debug)]
pub struct PostgresBackend<'a> {
    conn: &'a Client,
    /// The per-engine line-1 guard returned by [`guard`](MigrationBackend::guard).
    /// This is the **dialect contract** guard (it answers "what is PG's line-1?")
    /// and carries a [`PgGuard`](crate::guard::PgGuard).
    ///
    /// NOTE: the executor's per-apply FIRST PASS does NOT consult this stored
    /// guard for its denials — the apply config (the project schema + the
    /// `Confined`/`Platform`/`Trusted` trust profile) lives on the per-call
    /// [`ExecutorConfig`], not the backend, so the executor selects the
    /// config-correct guard with
    /// [`guard_for(cfg.guard_config())`](crate::guard::guard_for) (the same
    /// `MigrationGuard` seam, dialect-selected). This field is the default
    /// project-agnostic Confined PG guard so `guard()` has a concrete value to
    /// borrow; the trust/project that actually gates an apply always comes from
    /// `cfg`.
    guard: crate::guard::PgGuard,
}

impl<'a> PostgresBackend<'a> {
    /// Wrap a migrator connection as the Postgres backend.
    #[must_use]
    pub fn new(conn: &'a Client) -> Self {
        Self {
            conn,
            guard: crate::guard::PgGuard::from_config(crate::guard::GuardConfig::confined(
                String::new(),
            )),
        }
    }

    /// The borrowed connection — for the (few) PG-only call sites that still take
    /// a raw `&Client` and are out of P1's seam scope (the shadow-DB dry-run and
    /// the standalone lock helpers re-exported for `submit`/`engine`).
    #[must_use]
    pub fn conn(&self) -> &'a Client {
        self.conn
    }
}

impl MigrationBackend for PostgresBackend<'_> {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn guard(&self) -> &dyn crate::guard::MigrationGuard {
        &self.guard
    }

    async fn acquire_project_lock(&self, project_id: &str) -> Result<(), ApplyError> {
        crate::executor::pg::acquire_project_lock(self.conn, project_id).await
    }

    async fn release_project_lock(&self, project_id: &str) -> Result<(), ApplyError> {
        crate::executor::pg::release_project_lock(self.conn, project_id).await
    }

    async fn snapshot_session(&self) -> Result<SessionSnapshot, ApplyError> {
        crate::executor::pg::snapshot_session(self.conn).await
    }

    async fn restore_session(&self, snap: &SessionSnapshot) -> Result<(), ApplyError> {
        crate::executor::pg::restore_session(self.conn, snap).await
    }

    async fn reset_role_best_effort(&self) {
        if let Err(e) = self.conn.batch_execute("RESET ROLE").await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to RESET ROLE after apply (L1)");
        }
    }

    async fn apply_up_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<(), ApplyError> {
        crate::executor::pg::apply_transactional(self.conn, cfg, m, applied_by, supersedes, kind)
            .await
    }

    async fn configure_session_non_txn(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<(), ApplyError> {
        crate::executor::pg::configure_session_non_txn(self.conn, cfg, m).await
    }

    async fn apply_up_non_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        had_inflight: bool,
        supersedes: &[&str],
    ) -> Result<bool, ApplyError> {
        crate::executor::pg::apply_non_transactional(
            self.conn, cfg, m, applied_by, had_inflight, supersedes,
        )
        .await
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        crate::executor::pg::rollback_one_transactional(self.conn, cfg, m, applied_by).await
    }

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        crate::executor::pg::validate_non_txn_idempotent(m)
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal::ensure_journal(self.conn, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal::applied(self.conn, cfg).await
    }

    async fn superseded_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal::superseded_versions(self.conn, cfg).await
    }

    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError> {
        journal::latest_completed_checksums(self.conn, cfg).await
    }

    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<crate::drift::ChecksumDriftReport, DriftError> {
        crate::drift::check_checksum_drift(self.conn, cfg, migrations).await
    }

    async fn snapshot_schema(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<SchemaSnapshot, DriftError> {
        crate::drift::snapshot_schema(self.conn, &cfg.project_schema).await
    }

    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::executor::PreconditionVerdict, ApplyError> {
        crate::executor::pg::evaluate_preconditions(self.conn, cfg, m).await
    }

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        _m: &Migration,
        _applied_by: &str,
    ) -> Result<(), ApplyError> {
        // PG never produces a `SqliteRebuild` (its declarative differ uses native
        // `ALTER` / expand-contract, not the 12-step rebuild). A rebuild reaching the
        // Postgres backend is a routing bug — fail closed with a clear error rather
        // than silently passing.
        Err(ApplyError::Backend(format!(
            "postgres backend: SQLite table rebuild requested for '{}' — the PG differ \
             never produces rebuilds (routing bug)",
            spec.table
        )))
    }

    fn expand_conn(&self) -> Option<&Client> {
        Some(self.conn)
    }
}
