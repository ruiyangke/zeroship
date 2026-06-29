//! `__zeroship_migrations` audit table — A3 of the @zeroship/db proposal
//! (docs/proposals/zeroship-db.md, section A3).
//!
//! Provides an append-only, per-app schema migration log: every DDL,
//! validation pass, and backfill writes a row keyed by deploy
//! identifier. The A1 `create_index_with_recovery` retry path that used
//! to log via `tracing::warn!` now writes structured audit rows here.
//!
//! ## Error contract
//!
//! Every fallible helper returns [`Result<_, DbError>`]. SQLSTATE
//! classification (`unique_violation`, `serialization_failure`,
//! `transient`, …) is preserved via [`coded_sql`] which wraps the
//! Postgres error in [`DbError`] through the `From<compio_postgres::Error>`
//! impl in `crate::error`. Callers `?`-flow these through the
//! [`Backend`](crate::backend::Backend) trait — there is no string
//! flattening at any boundary inside the crate.
//!
//! ## Divergence from proposal
//!
//! The proposal section A3 specifies a tamper-evident
//! `SECURITY DEFINER` write path mediated by an HMAC-signed
//! `__zeroship_session_ctx` PID-keyed table living in a platform-wide
//! `__zeroship_admin` schema, plus a `__zeroship_log_migration`
//! SECURITY DEFINER function that derives `applied_by_kind`/`applied_by_id`
//! from that context. The motivation for that machinery is to prevent a
//! compromised app role with raw DML access from forging audit entries.
//!
//! In the current zeroship architecture, **app code does not have raw
//! SQL access** — JS calls `zeroship.db.*` native primitives which are
//! Rust functions that build parameterised queries from inside the worker
//! process. The worker pool is the only writer; there is no path for app
//! JS to issue `SET LOCAL zeroship.actor_kind = 'operator'` or to write
//! directly to `__zeroship_migrations`. Provenance is therefore enforced
//! at the Rust call boundary, not at the SQL boundary.
//!
//! When the proposal's `SECURITY DEFINER` + `__zeroship_admin` machinery
//! lands (control-plane provisioning task), the body of
//! [`write_audit_row`] and [`update_audit_status`] can be swapped to call
//! the privileged functions without changing the public Rust API. This
//! is tracked as deferred follow-up in the dispatch report.

use compio_postgres::{Client, Pool, Row};
use serde_json::Value;

use crate::error::{first_row_or_internal, DbError};

/// Wrap a `compio_postgres::Error` in a [`DbError`] with a context
/// phrase so operators see *what* the audit layer was doing when the
/// SQL failed. The SQLSTATE classification still drives the `.code`
/// (`unique_violation`, `serialization_failure`, …) — this helper only
/// prepends `"audit: <ctx>: "` to the message body.
///
/// The variant-walking logic is shared with the per-module helpers in
/// `auth::{bootstrap,keys,session}`, `diff`, and `replication` via
/// [`crate::error::coded_sql`]. This thin wrapper just stamps the
/// `audit` module prefix onto the context phrase.
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    crate::error::coded_sql(&format!("audit: {context}"), e)
}

/// Actor categories accepted by the audit table.
///
/// Mirrors the proposal's `applied_by_kind` check
/// (`'auto' | 'user' | 'ai-builder' | 'operator'`). Workers emit rows
/// with `Auto` from the cold-start orchestrator path.
#[derive(Debug, Clone, Copy)]
pub enum ActorKind {
    /// Platform-issued (cold-start orchestrator, background sweeper).
    Auto,
}

impl ActorKind {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Auto => "auto",
        }
    }
}

/// Phase of a migration row — proposal A3 `phase` column.
#[cfg(any(test, feature = "test-helpers"))]
#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Ddl,
    #[allow(dead_code, reason = "Validation stays in the persisted audit enum even though the current release flow never constructs that phase.")]
    Validation,
    /// B1 data backfill phase — rows authored by `@zeroship/migrations`
    /// orchestrator via `zeroship.db.migration*` primitives.
    Backfill,
}

#[cfg(any(test, feature = "test-helpers"))]
impl Phase {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Ddl => "ddl",
            Self::Validation => "validation",
            Self::Backfill => "backfill",
        }
    }
}

/// Change classification — proposal A2 `change_class`.
#[derive(Debug, Clone, Copy)]
pub enum ChangeClass {
    Additive,
    Compatible,
    Destructive,
}

impl ChangeClass {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Additive => "additive",
            Self::Compatible => "compatible",
            Self::Destructive => "destructive",
        }
    }
}

// **Schema-authority P1** — map the schema layer's classification
// (`zeroship_schema::diff::ChangeClass`) onto this audit-row enum. The
// conversion used to live as `diff::ChangeClass::as_audit()`, but the diff
// classifier was relocated into the leaf crate `zeroship-schema`, which must
// not reach into this data-plane lifecycle type. The conversion therefore
// lives here (where the audit enum is defined); call sites in
// `register_model::{validate, apply, mod}` use `ChangeClass::from(op.class)`.
impl From<crate::diff::ChangeClass> for ChangeClass {
    fn from(c: crate::diff::ChangeClass) -> Self {
        match c {
            crate::diff::ChangeClass::Additive => Self::Additive,
            crate::diff::ChangeClass::Compatible => Self::Compatible,
            crate::diff::ChangeClass::Destructive => Self::Destructive,
        }
    }
}

/// Initial status accepted on INSERT — proposal A3 state machine
/// (`pending` for asynchronous queueing, `running` for synchronous DDL
/// where the orchestrator owns the work).
///
/// `ValidationRefused` is a terminal status that may ALSO appear on
/// INSERT — the validate stage writes destructive-op audit rows that
/// land terminal at creation, eliminating the orphan-Pending window the
/// previous Failed+marker pattern carried. The audit table's status
/// CHECK accepts it as both an initial and terminal value (see
/// `ensure_audit_table_exists`).
#[derive(Debug, Clone, Copy)]
#[cfg(any(test, feature = "test-helpers"))]
pub enum InitialStatus {
    #[allow(dead_code, reason = "Pending remains part of the audit state machine for queued/test flows that are not compiled into the current release build.")]
    Pending,
    Running,
    /// Destructive op refused by the validate stage. Lands terminal at
    /// INSERT so there is no orphan-Pending window for operators to
    /// chase. Mirrors `TerminalStatus::ValidationRefused`.
    ValidationRefused,
}

#[cfg(any(test, feature = "test-helpers"))]
impl InitialStatus {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::ValidationRefused => "validation_refused",
        }
    }
}

/// Terminal status passed to [`update_audit_status`].
#[derive(Debug, Clone, Copy)]
pub enum TerminalStatus {
    Applied,
    AppliedWithDeadLetter,
    Failed,
    Cancelled,
    /// Destructive op refused by validate (proposal A2 strict mode).
    /// Distinguishes "the platform refused to run this DDL" from
    /// "the DDL ran and failed" (`Failed`). Operators can grep on
    /// `status = 'validation_refused'` without parsing `error_message`.
    /// Symmetric with `InitialStatus::ValidationRefused` so callers
    /// can express the state either as an INSERT-direct terminal or
    /// as a transition target.
    #[allow(dead_code, reason = "ValidationRefused is preserved for audit symmetry and test coverage, though the current release flow only constructs the initial-state variant.")]
    ValidationRefused,
}

impl TerminalStatus {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::AppliedWithDeadLetter => "applied_with_dead_letter",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ValidationRefused => "validation_refused",
        }
    }
}

/// Row inserted into `__zeroship_migrations`. Field names mirror the
/// proposal A3 column list; columns the worker doesn't populate (parent
/// link, dead-letter PKs, validate cursor, owner session, heartbeat) are
/// left to the B1 migrations runtime.
#[cfg(any(test, feature = "test-helpers"))]
#[derive(Debug)]
pub struct AuditRow {
    pub collection: String,
    pub phase: Phase,
    pub change_class: ChangeClass,
    pub change_kind: String,
    pub details: Value,
    pub ddl_sql: Option<String>,
    pub status: InitialStatus,
    pub deploy_id: String,
    pub schema_version: i32,
    pub actor: ActorKind,
}

/// Create `__zeroship_migrations` in the app's schema if it doesn't
/// exist. Idempotent — safe to invoke on every cold-start; the table is
/// created without dropping existing data.
///
/// The schema name doubles as the app identifier. The caller MUST have
/// already created the schema (e.g. via `build_create_schema`).
pub async fn ensure_audit_table_exists(pool: &Pool, app_id: &str) -> Result<(), DbError> {
    validate_app_id(app_id)?;

    // Quote-escape app_id for embedding in the DDL. The validation pass
    // above restricts the character set to `[A-Za-z0-9_-]` so this is
    // belt-and-braces against an internal caller passing something weird.
    let create_sql = format!(
        r#"CREATE TABLE IF NOT EXISTS "{app_id}"."__zeroship_migrations" (
  id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  collection          TEXT NOT NULL,
  phase               TEXT NOT NULL,
  change_class        TEXT NOT NULL,
  change_kind         TEXT NOT NULL,
  details             JSONB NOT NULL,
  ddl_sql             TEXT,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  applied_at          TIMESTAMPTZ,
  applied_by_kind     TEXT NOT NULL,
  applied_by_id       TEXT,
  deploy_id           TEXT NOT NULL,
  parent_id           BIGINT REFERENCES "{app_id}"."__zeroship_migrations"(id),
  schema_version      INTEGER NOT NULL,
  status              TEXT NOT NULL,
  error               TEXT,
  duration_ms         INTEGER,
  validate_cursor     BIGINT,
  owner_session_id    TEXT,
  last_heartbeat_at   TIMESTAMPTZ,
  dead_letter_pks     JSONB,
  audit_generation    BIGINT NOT NULL DEFAULT 0,
  CONSTRAINT __zeroship_migrations_phase_chk CHECK (
    phase IN ('ddl','validation','backfill','audit')
  ),
  CONSTRAINT __zeroship_migrations_class_chk CHECK (
    change_class IN ('additive','compatible','destructive')
  ),
  CONSTRAINT __zeroship_migrations_status_chk CHECK (
    status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back','validation_refused')
  )
)"#
    );

    let empty: Vec<&str> = Vec::new();
    pool.query_text_params(&create_sql, &empty)
        .await
        .map_err(|e| coded_sql("create __zeroship_migrations", e))?;

    // Gap X: idempotent column add for tables created before this
    // commit. `audit_generation` is bumped by `exec_reset` so a
    // worker that started a run on generation `g` can detect that
    // its run got reset out from under it (generation changes →
    // commit_batch ROLLBACKs with `migration_reset_externally`).
    let add_gen = format!(
        r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
            ADD COLUMN IF NOT EXISTS audit_generation BIGINT NOT NULL DEFAULT 0"#
    );
    pool.query_text_params(&add_gen, &empty)
        .await
        .map_err(|e| coded_sql("add audit_generation column", e))?;

    // F2 (r13): widen the status CHECK on pre-existing audit tables to
    // include `'validation_refused'`. `DROP CONSTRAINT IF EXISTS` makes
    // this idempotent — on a freshly created table the constraint name
    // matches what we just emitted (`CREATE TABLE` above wired the same
    // constraint) so DROP+ADD is a no-op rewrite; on an old table from
    // before this commit the DROP succeeds (constraint exists) and the
    // ADD installs the wider list. On a really old table without the
    // named constraint at all, the IF EXISTS clause swallows the miss
    // and ADD still installs the new constraint.
    //
    // The ADD uses the same constraint name as `CREATE TABLE` so the
    // post-state is identical regardless of which branch ran.
    let drop_status_chk = format!(
        r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
            DROP CONSTRAINT IF EXISTS __zeroship_migrations_status_chk"#
    );
    pool.query_text_params(&drop_status_chk, &empty)
        .await
        .map_err(|e| coded_sql("drop status_chk", e))?;
    let add_status_chk = format!(
        r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
            ADD CONSTRAINT __zeroship_migrations_status_chk CHECK (
                status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back','validation_refused')
            )"#
    );
    pool.query_text_params(&add_status_chk, &empty)
        .await
        .map_err(|e| coded_sql("add status_chk", e))?;

    // Indexes — proposal section A3. These are plain (non-CONCURRENT)
    // because we're inside the cold-start orchestration that has the
    // advisory lock and no concurrent writers will exist before the
    // table is published.
    let idx_deploy = format!(
        r#"CREATE INDEX IF NOT EXISTS "__zeroship_migrations_deploy_idx" ON "{app_id}"."__zeroship_migrations" (deploy_id)"#
    );
    pool.query_text_params(&idx_deploy, &empty)
        .await
        .map_err(|e| coded_sql("create deploy_idx", e))?;

    let idx_updated = format!(
        r#"CREATE INDEX IF NOT EXISTS "__zeroship_migrations_updated_at_idx" ON "{app_id}"."__zeroship_migrations" (updated_at DESC)"#
    );
    pool.query_text_params(&idx_updated, &empty)
        .await
        .map_err(|e| coded_sql("create updated_at_idx", e))?;

    Ok(())
}

/// Compute the next monotonic `schema_version` for a deploy. Proposal
/// A2: `SELECT COALESCE(MAX(schema_version), 0) + 1 FROM
/// __zeroship_migrations WHERE phase='ddl' AND status='applied'`.
pub async fn next_schema_version(pool: &Pool, app_id: &str) -> Result<i32, DbError> {
    validate_app_id(app_id)?;
    let sql = format!(
        r#"SELECT COALESCE(MAX(schema_version), 0) + 1 AS v FROM "{app_id}"."__zeroship_migrations" WHERE phase = 'ddl' AND status = 'applied'"#
    );
    let empty: Vec<&str> = Vec::new();
    let rows = pool
        .query_text_params(&sql, &empty)
        .await
        .map_err(|e| coded_sql("read schema_version", e))?;
    let v: i32 = rows.first().map(|r| r.get::<_, i32>("v")).unwrap_or(1);
    Ok(v)
}

/// Insert a single row into the audit table, returning its PK so callers
/// can later call [`update_audit_status`] to drive it to a terminal
/// state.
#[cfg(any(test, feature = "test-helpers"))]
pub async fn write_audit_row(pool: &Pool, app_id: &str, row: &AuditRow) -> Result<i64, DbError> {
    validate_app_id(app_id)?;

    let sql = format!(
        r#"INSERT INTO "{app_id}"."__zeroship_migrations"
            (collection, phase, change_class, change_kind, details,
             ddl_sql, status, deploy_id, applied_by_kind, schema_version)
            VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9, $10::integer)
            RETURNING id"#
    );

    let details_str = row.details.to_string();
    let schema_version_str = row.schema_version.to_string();
    let ddl_sql_str: String = row.ddl_sql.clone().unwrap_or_default();
    let params: Vec<&str> = vec![
        row.collection.as_str(),
        row.phase.as_sql(),
        row.change_class.as_sql(),
        row.change_kind.as_str(),
        details_str.as_str(),
        ddl_sql_str.as_str(),
        row.status.as_sql(),
        row.deploy_id.as_str(),
        row.actor.as_sql(),
        schema_version_str.as_str(),
    ];

    let rows = pool
        .query_text_params(&sql, &params)
        .await
        .map_err(|e| coded_sql("insert", e))?;
    let id: i64 = first_row_or_internal(&rows, "audit: INSERT")?.get::<_, i64>("id");
    Ok(id)
}

/// Drive an audit row from its current state to a terminal status. The
/// allowed transitions mirror the proposal A3 state machine:
/// `running -> applied | applied_with_dead_letter | failed | cancelled |
/// validation_refused`. Returns `Ok(true)` if the row transitioned,
/// `false` if the UPDATE matched nothing (e.g. row already terminal).
/// Note: `validation_refused` is normally written INSERT-direct from
/// `validate.rs` (since cycle-15:17 `6afab751`); this method accepts it
/// as a terminal for symmetry with `InitialStatus::ValidationRefused`.
#[cfg(any(test, feature = "test-helpers"))]
pub async fn update_audit_status(
    pool: &Pool,
    app_id: &str,
    id: i64,
    new_status: TerminalStatus,
    error: Option<&str>,
) -> Result<bool, DbError> {
    validate_app_id(app_id)?;

    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = $2,
                error = COALESCE($3, error),
                updated_at = NOW(),
                applied_at = CASE
                    WHEN $2 IN ('applied','applied_with_dead_letter') AND applied_at IS NULL THEN NOW()
                    ELSE applied_at
                END
            WHERE id = $1::bigint AND status IN ('running','pending')
            RETURNING id"#
    );

    let id_str = id.to_string();
    let err_str = error.unwrap_or("");
    let params: Vec<&str> = vec![id_str.as_str(), new_status.as_sql(), err_str];
    let rows = pool
        .query_text_params(&sql, &params)
        .await
        .map_err(|e| coded_sql("update status", e))?;
    Ok(!rows.is_empty())
}

// ─────────────────────────────────────────────────────────────────────
// B1 backfill audit-row helpers.
//
// `crate::migrations` used to inline INSERT / UPDATE / SELECT against
// `__zeroship_migrations` from ~13 distinct sites; those calls now go
// through the typed helpers below, keeping the audit table's writers
// in a single file. The wire SQL is identical to the previous inline
// statements — same column list, same WHERE-clause shape, same RETURNING
// clauses.
//
// The state machine the helpers implement (mirrors proposal A3):
//
//   [INSERT phase='backfill', status='running']
//        │
//        ▼
//   ┌────running─────┐ ──progress──▶ running (cursor / dead_letter / heartbeat)
//   │                │ ──reset─────▶ pending (audit_generation += 1)
//   │                │ ──cancel────▶ cancelled
//   │                │ ──terminal──▶ applied / applied_with_dead_letter / failed
//   └────────────────┘
//
// The helpers come in two flavours: pool-driven (used by short
// non-locking calls — status / cancel / reset / start-time bootstrap)
// and client-driven (used while the dedicated `Client` holds the
// session-scoped advisory lock so subsequent calls land on the same
// backend session).

/// Snapshot of a backfill row identified by (collection, change_kind).
/// Returned by [`find_latest_backfill_row`] for callers that need to
/// inspect prior state at `migration.start(...)` time.
#[derive(Debug)]
pub struct BackfillLookup {
    pub id: i64,
    pub status: String,
    pub cursor: i64,
    pub processed: i64,
    pub dead_letter_pks: Value,
    pub audit_generation: i64,
    pub error: Option<String>,
    pub is_done: bool,
}

/// Snapshot returned by [`lock_audit_row_for_update`] — the columns the
/// commit-batch path needs to validate generation + cancellation before
/// applying row updates.
#[derive(Debug)]
pub struct LockedAuditRow {
    pub status: String,
    pub audit_generation: i64,
}

/// Compatibility shim — the legacy `Pool` and `Client` types both expose
/// `query_text_params(sql, &[&str])`, so the helpers below accept either
/// via this trait. Keeps the audit-table SQL in one file without forcing
/// callers to thread connection ownership through wrapper types.
///
/// Uses `async_fn_in_trait` (single-thread compio invariant — same
/// posture as [`crate::backend::Backend`]) so callers don't pay a
/// `Box<dyn Future>` allocation per call. Both impls (`Pool`,
/// `Client`) are sized types — there is no `dyn AuditExecutor` use
/// site in the crate, so `?Sized` is unnecessary.
pub(crate) trait AuditExecutor {
    #[allow(async_fn_in_trait)]
    async fn query_text(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Row>, compio_postgres::Error>;
}

impl AuditExecutor for Pool {
    async fn query_text(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Row>, compio_postgres::Error> {
        self.query_text_params(sql, params).await
    }
}

impl AuditExecutor for Client {
    async fn query_text(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Row>, compio_postgres::Error> {
        self.query_text_params(sql, params).await
    }
}

/// Decode `details.processed` from a backfill audit row. The audit
/// table's `details` column is `jsonb`; `Row::raw_value` returns the
/// binary jsonb wire format (1-byte version prefix + JSON text).
pub fn read_processed_from_audit_row(row: &Row) -> i64 {
    let bytes = row.raw_value("details");
    let Some(bytes) = bytes else { return 0 };
    if bytes.len() < 2 {
        return 0;
    }
    let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
    let parsed: Value = serde_json::from_str(json_str).unwrap_or(Value::Null);
    parsed
        .get("processed")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Decode `dead_letter_pks` from a backfill audit row.
pub fn read_dead_letter_pks_from_audit_row(row: &Row) -> Value {
    let bytes = row.raw_value("dead_letter_pks");
    let Some(bytes) = bytes else { return Value::Array(vec![]) };
    if bytes.len() < 2 {
        return Value::Array(vec![]);
    }
    let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
    serde_json::from_str(json_str).unwrap_or(Value::Array(vec![]))
}

/// SELECT the latest backfill row for `(collection, change_kind=name)`.
/// Returns `Ok(None)` if the row hasn't been inserted yet.
pub(crate) async fn find_latest_backfill_row<E: AuditExecutor>(
    exec: &E,
    app_id: &str,
    collection: &str,
    name: &str,
) -> Result<Option<BackfillLookup>, DbError> {
    let sql = format!(
        r#"SELECT id, status, validate_cursor, dead_letter_pks, details, error, audit_generation
            FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let rows = exec
        .query_text(&sql, &[collection, name])
        .await
        .map_err(|e| coded_sql("find_latest_backfill_row", e))?;
    let Some(row) = rows.first() else { return Ok(None) };
    let id: i64 = row.get("id");
    let status: String = row.get("status");
    let cursor: i64 = row.try_get::<_, i64>("validate_cursor").unwrap_or(0);
    let processed = read_processed_from_audit_row(row);
    let dead_letter_pks = read_dead_letter_pks_from_audit_row(row);
    let audit_generation: i64 = row.try_get::<_, i64>("audit_generation").unwrap_or(0);
    // SQL NULL and empty-string both mean "no error". The migrations
    // SDK's parseNative treats any string in `error` as a thrown
    // exception, so an empty string would surface as a zero-message
    // failure on the caller side.
    let error: Option<String> = row
        .try_get::<_, String>("error")
        .ok()
        .filter(|s| !s.is_empty());
    let is_done = matches!(
        status.as_str(),
        "applied" | "applied_with_dead_letter" | "failed" | "cancelled"
    );
    Ok(Some(BackfillLookup {
        id,
        status,
        cursor,
        processed,
        dead_letter_pks,
        audit_generation,
        error,
        is_done,
    }))
}

/// `UPDATE … SET status='running'` — used at `migration.start(...)` time
/// when an existing backfill row is being resumed by this worker.
/// Refreshes `owner_session_id`/`last_heartbeat_at` to the current
/// backend so operators can see who's running.
pub async fn set_backfill_running(
    client: &Client,
    app_id: &str,
    id: i64,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = 'running',
                owner_session_id = pg_backend_pid()::text,
                last_heartbeat_at = NOW(),
                updated_at = NOW(),
                error = NULL
            WHERE id = $1::bigint"#
    );
    let id_s = id.to_string();
    client
        .query_text_params(&sql, &[id_s.as_str()])
        .await
        .map_err(|e| coded_sql("set_backfill_running", e))?;
    Ok(())
}

/// `INSERT … status='running'` — first-time insert at
/// `migration.start(...)`. Stamps `owner_session_id` to the connection's
/// `pg_backend_pid()` and seeds `details.processed = 0`. Audit
/// generation defaults to 0 from the column DEFAULT.
#[allow(clippy::too_many_arguments)]
pub async fn insert_backfill_running(
    client: &Client,
    app_id: &str,
    collection: &str,
    name: &str,
    dry_run: bool,
    deploy_id: &str,
    schema_version: i32,
) -> Result<i64, DbError> {
    let sql = format!(
        r#"INSERT INTO "{app_id}"."__zeroship_migrations"
            (collection, phase, change_class, change_kind, details,
             ddl_sql, status, deploy_id, applied_by_kind, schema_version,
             owner_session_id, last_heartbeat_at, validate_cursor)
            VALUES ($1, 'backfill', $2, $3, $4::jsonb,
                    NULL, 'running', $5, $6, $7::integer,
                    pg_backend_pid()::text, NOW(), 0)
            RETURNING id"#
    );
    let details = serde_json::json!({
        "processed": 0,
        "dryRun": dry_run,
    });
    let details_s = details.to_string();
    let sv_s = schema_version.to_string();
    let rows = client
        .query_text_params(
            &sql,
            &[
                collection,
                ChangeClass::Additive.as_sql(),
                name,
                details_s.as_str(),
                deploy_id,
                ActorKind::Auto.as_sql(),
                sv_s.as_str(),
            ],
        )
        .await
        .map_err(|e| coded_sql("insert_backfill_running", e))?;
    // Defensive: an empty RETURNING set used to silently produce `id =
    // 0` (via `.unwrap_or_default()`), which then aliased every
    // downstream `WHERE id = $1::bigint` write to a no-op. The
    // regression test in this module locks in the `DbError::Internal`
    // path so RLS bypass / trigger interception surfaces loudly. The
    // predicate now lives in `crate::error::first_row_or_internal` so
    // every empty-RETURNING site emits the same message shape.
    let id: i64 =
        first_row_or_internal(&rows, "audit: insert_backfill_running")?.get::<_, i64>("id");
    Ok(id)
}

/// `UPDATE … audit_generation = audit_generation + 1` — bumps the
/// generation counter so any in-flight worker holding a stale snapshot
/// will detect the reset on its next commit. Also zeroes the cursor,
/// dead-letter PKs, and processed counter so a fresh run starts from
/// the top.
pub(crate) async fn reset_backfill_row<E: AuditExecutor>(
    exec: &E,
    app_id: &str,
    collection: &str,
    name: &str,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = 'pending',
                validate_cursor = NULL,
                dead_letter_pks = NULL,
                error = NULL,
                applied_at = NULL,
                updated_at = NOW(),
                audit_generation = audit_generation + 1,
                details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', '0'::jsonb)
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2"#
    );
    exec.query_text(&sql, &[collection, name])
        .await
        .map_err(|e| coded_sql("reset_backfill_row", e))?;
    Ok(())
}

/// `SELECT status FROM … ORDER BY id DESC LIMIT 1` — fast-path peek
/// used by `migration.fetchBatch(...)` to short-circuit if the operator
/// cancelled between batches.
pub(crate) async fn peek_latest_backfill_status<E: AuditExecutor>(
    exec: &E,
    app_id: &str,
    collection: &str,
    name: &str,
) -> Result<Option<String>, DbError> {
    let sql = format!(
        r#"SELECT status FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let rows = exec
        .query_text(&sql, &[collection, name])
        .await
        .map_err(|e| coded_sql("peek_latest_backfill_status", e))?;
    Ok(rows.first().map(|r| r.get::<_, String>("status")))
}

/// `UPDATE … SET last_heartbeat_at = NOW()` — best-effort write the
/// fetch-batch path issues so operators can see the worker is alive.
pub(crate) async fn heartbeat_backfill<E: AuditExecutor>(
    exec: &E,
    app_id: &str,
    collection: &str,
    name: &str,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET last_heartbeat_at = NOW()
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2 AND status = 'running'"#
    );
    exec.query_text(&sql, &[collection, name])
        .await
        .map_err(|e| coded_sql("heartbeat_backfill", e))?;
    Ok(())
}

/// `SELECT status, audit_generation … FOR UPDATE` — acquires the row
/// lock inside the current transaction so a concurrent
/// `migrations.cancel(...)` on another connection serialises against
/// the commit. Caller must already be inside a `BEGIN`.
///
/// Returns `Ok(None)` if the row was missing (shouldn't happen after
/// a successful `exec_begin`, but kept honest).
pub async fn lock_audit_row_for_update(
    client: &Client,
    app_id: &str,
    id: i64,
) -> Result<Option<LockedAuditRow>, DbError> {
    let sql = format!(
        r#"SELECT status, audit_generation FROM "{app_id}"."__zeroship_migrations"
            WHERE id = $1::bigint FOR UPDATE"#
    );
    let id_s = id.to_string();
    let rows = client
        .query_text_params(&sql, &[id_s.as_str()])
        .await
        .map_err(|e| coded_sql("lock_audit_row_for_update", e))?;
    let Some(row) = rows.first() else { return Ok(None) };
    let status: String = row.get("status");
    let audit_generation: i64 = row.try_get::<_, i64>("audit_generation").unwrap_or(0);
    Ok(Some(LockedAuditRow {
        status,
        audit_generation,
    }))
}

/// `UPDATE … validate_cursor / dead_letter_pks / processed` — advances
/// the row's progress columns after a successful batch. Must run on the
/// same connection that holds the row lock from
/// [`lock_audit_row_for_update`].
pub async fn update_backfill_progress(
    client: &Client,
    app_id: &str,
    id: i64,
    next_cursor: i64,
    dead_letter_pks: &Value,
    processed_total: i64,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET validate_cursor = $2::bigint,
                dead_letter_pks = $3::jsonb,
                details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', to_jsonb($4::bigint)),
                last_heartbeat_at = NOW(),
                updated_at = NOW()
            WHERE id = $1::bigint"#
    );
    let dlp_s = dead_letter_pks.to_string();
    let id_s = id.to_string();
    let nc_s = next_cursor.to_string();
    let pt_s = processed_total.to_string();
    client
        .query_text_params(
            &sql,
            &[id_s.as_str(), nc_s.as_str(), dlp_s.as_str(), pt_s.as_str()],
        )
        .await
        .map_err(|e| coded_sql("update_backfill_progress", e))?;
    Ok(())
}

/// `UPDATE … status=$terminal, owner_session_id = NULL` — terminal
/// transition for a backfill row. Idempotent: only flips rows still in
/// `running` / `pending`, so a doubled call is harmless. Used by
/// `exec_commit_batch` when `isDone=true`.
pub async fn finalise_backfill(
    client: &Client,
    app_id: &str,
    id: i64,
    terminal: TerminalStatus,
    error_message: Option<&str>,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = $2,
                error = COALESCE($3, error),
                updated_at = NOW(),
                applied_at = CASE
                    WHEN $2 IN ('applied','applied_with_dead_letter') AND applied_at IS NULL THEN NOW()
                    ELSE applied_at
                END,
                owner_session_id = NULL
            WHERE id = $1::bigint AND status IN ('running','pending')"#
    );
    let id_s = id.to_string();
    let err_s = error_message.unwrap_or("").to_string();
    client
        .query_text_params(&sql, &[id_s.as_str(), terminal.as_sql(), err_s.as_str()])
        .await
        .map_err(|e| coded_sql("finalise_backfill", e))?;
    Ok(())
}

/// `UPDATE … status='cancelled'` — operator-driven cancel via
/// `migrations.cancel(...)`. Runs on a pool client (no lock) — the
/// row's FOR UPDATE in any concurrent `exec_commit_batch` serialises
/// against this update.
pub(crate) async fn cancel_backfill_row<E: AuditExecutor>(
    exec: &E,
    app_id: &str,
    id: i64,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = 'cancelled',
                updated_at = NOW(),
                owner_session_id = NULL,
                error = COALESCE(error, 'cancelled by operator')
            WHERE id = $1::bigint AND status IN ('pending','running')"#
    );
    let id_s = id.to_string();
    exec.query_text(&sql, &[id_s.as_str()])
        .await
        .map_err(|e| coded_sql("cancel_backfill_row", e))?;
    Ok(())
}

/// Validate an app_id used as a schema name — same rules as the query
/// builder's `validate_schema`. Local copy avoids exporting a private
/// function out of `query.rs`.
///
/// Refusals stamp `invalid_app_id` so the SDK can branch on
/// `err.code === 'invalid_app_id'` rather than substring-matching the
/// message. This is a defensive guardrail — every caller in this
/// crate threads through a worker-controlled `app_id`, but the audit
/// helpers are the only file in `plugin-db` that string-interpolates
/// the app id directly into DDL, so we belt-and-braces the input
/// here.
fn validate_app_id(name: &str) -> Result<(), DbError> {
    if name.is_empty() {
        return Err(DbError::validation(
            "invalid_app_id",
            "audit: app_id cannot be empty",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        // Name the allowed alphabet inline so SDK-facing error messages
        // tell creators what's permitted. Mirrors the `validate_field_name`
        // shape landed at `403b3891` (cycle 11:17 [I12]) and the twin
        // alphabet-naming pattern in `replication.rs:91-97`. Closes the
        // 5-cycle error-ux carry on audit.rs:818.
        return Err(DbError::validation(
            "invalid_app_id",
            format!(
                "audit: invalid app_id: {name} (allowed: ASCII alphanumeric + underscore + hyphen)"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_app_id_accepts_valid_names() {
        assert!(validate_app_id("app_abc123").is_ok());
        assert!(validate_app_id("app-with-hyphen").is_ok());
        assert!(validate_app_id("a").is_ok());
    }

    #[test]
    fn validate_app_id_rejects_injection() {
        for bad in ["", "app\"; DROP TABLE x; --", "app.other", "app/other"] {
            let err = validate_app_id(bad).expect_err("must reject");
            // Refusal must stamp the static `invalid_app_id` code so the
            // SDK can branch on it programmatically — the guard predates
            // the typed-error rail but is now the only `audit::*` path
            // that surfaces a non-SQLSTATE-classified DbError.
            match err {
                DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "invalid_app_id");
                }
                other => panic!("expected ValidationFailed, got {other:?}"),
            }
        }
    }

    /// Regression: before the fix, `insert_backfill_running` called
    /// `.unwrap_or_default()` on the RETURNING rows, silently returning
    /// `id = 0` when the INSERT returned no row (RLS bypass, trigger
    /// interception, or a missing RETURNING clause). A `0` id then
    /// propagated into `WHERE id = 0` queries, silently no-oping every
    /// downstream progress write.  The fix mirrors `write_audit_row`
    /// and returns `DbError::Internal` instead.
    ///
    /// This test cannot drive a real Client, but it directly exercises
    /// the `first_row_or_internal` helper on an empty slice to lock in
    /// the intended behaviour — the same predicate the production
    /// site now calls.
    #[test]
    fn insert_backfill_running_empty_returning_is_internal_error() {
        let rows: Vec<()> = vec![];
        let result = first_row_or_internal(&rows, "audit: insert_backfill_running");
        match result {
            Err(DbError::Internal { message }) => {
                assert_eq!(message, "audit: insert_backfill_running: returned no row");
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }

    #[test]
    fn enum_sql_strings_match_proposal_check_constraints() {
        // These strings must match the proposal A3 CHECK constraints
        // verbatim, or INSERTs will fail with SQLSTATE 23514.
        assert_eq!(Phase::Ddl.as_sql(), "ddl");
        assert_eq!(Phase::Validation.as_sql(), "validation");
        assert_eq!(Phase::Backfill.as_sql(), "backfill");
        assert_eq!(ChangeClass::Additive.as_sql(), "additive");
        assert_eq!(ChangeClass::Compatible.as_sql(), "compatible");
        assert_eq!(ChangeClass::Destructive.as_sql(), "destructive");
        assert_eq!(InitialStatus::Pending.as_sql(), "pending");
        assert_eq!(InitialStatus::Running.as_sql(), "running");
        assert_eq!(InitialStatus::ValidationRefused.as_sql(), "validation_refused");
        assert_eq!(TerminalStatus::Applied.as_sql(), "applied");
        assert_eq!(TerminalStatus::AppliedWithDeadLetter.as_sql(), "applied_with_dead_letter");
        assert_eq!(TerminalStatus::Failed.as_sql(), "failed");
        assert_eq!(TerminalStatus::Cancelled.as_sql(), "cancelled");
        assert_eq!(TerminalStatus::ValidationRefused.as_sql(), "validation_refused");
        assert_eq!(ActorKind::Auto.as_sql(), "auto");
    }
}
