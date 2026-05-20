//! `__zeroship_migrations` audit table — A3 of the @zeroship/db proposal
//! (docs/proposals/zeroship-db.md, section A3).
//!
//! Provides an append-only, per-app schema migration log: every DDL,
//! validation pass, and (future) backfill writes a row keyed by deploy
//! identifier. The A1 `create_index_with_recovery` retry path that used
//! to log via `tracing::warn!` now writes structured audit rows here.
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

use compio_postgres::Pool;
use serde_json::Value;

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
#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Ddl,
    Validation,
    /// B1 data backfill phase — rows authored by `@zeroship/migrations`
    /// orchestrator via `zeroship.db.migration*` primitives.
    Backfill,
}

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

/// Initial status accepted on INSERT — proposal A3 state machine
/// (`pending` for asynchronous queueing, `running` for synchronous DDL
/// where the orchestrator owns the work).
#[derive(Debug, Clone, Copy)]
pub enum InitialStatus {
    Pending,
    Running,
}

impl InitialStatus {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
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
}

impl TerminalStatus {
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::AppliedWithDeadLetter => "applied_with_dead_letter",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Row inserted into `__zeroship_migrations`. Field names mirror the
/// proposal A3 column list; columns the worker doesn't populate (parent
/// link, dead-letter PKs, validate cursor, owner session, heartbeat) are
/// left to the B1 migrations runtime.
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
pub async fn ensure_audit_table_exists(pool: &Pool, app_id: &str) -> Result<(), String> {
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
    status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back')
  )
)"#
    );

    let empty: Vec<&str> = Vec::new();
    pool.query_text_params(&create_sql, &empty)
        .await
        .map_err(|e| format!("audit: create __zeroship_migrations failed: {e}"))?;

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
        .map_err(|e| format!("audit: add audit_generation column failed: {e}"))?;

    // Indexes — proposal section A3. These are plain (non-CONCURRENT)
    // because we're inside the cold-start orchestration that has the
    // advisory lock and no concurrent writers will exist before the
    // table is published.
    let idx_deploy = format!(
        r#"CREATE INDEX IF NOT EXISTS "__zeroship_migrations_deploy_idx" ON "{app_id}"."__zeroship_migrations" (deploy_id)"#
    );
    pool.query_text_params(&idx_deploy, &empty)
        .await
        .map_err(|e| format!("audit: create deploy_idx failed: {e}"))?;

    let idx_updated = format!(
        r#"CREATE INDEX IF NOT EXISTS "__zeroship_migrations_updated_at_idx" ON "{app_id}"."__zeroship_migrations" (updated_at DESC)"#
    );
    pool.query_text_params(&idx_updated, &empty)
        .await
        .map_err(|e| format!("audit: create updated_at_idx failed: {e}"))?;

    Ok(())
}

/// Compute the next monotonic `schema_version` for a deploy. Proposal
/// A2: `SELECT COALESCE(MAX(schema_version), 0) + 1 FROM
/// __zeroship_migrations WHERE phase='ddl' AND status='applied'`.
pub async fn next_schema_version(pool: &Pool, app_id: &str) -> Result<i32, String> {
    validate_app_id(app_id)?;
    let sql = format!(
        r#"SELECT COALESCE(MAX(schema_version), 0) + 1 AS v FROM "{app_id}"."__zeroship_migrations" WHERE phase = 'ddl' AND status = 'applied'"#
    );
    let empty: Vec<&str> = Vec::new();
    let rows = pool
        .query_text_params(&sql, &empty)
        .await
        .map_err(|e| format!("audit: read schema_version failed: {e}"))?;
    let v: i32 = rows.first().map(|r| r.get::<_, i32>("v")).unwrap_or(1);
    Ok(v)
}

/// Insert a single row into the audit table, returning its PK so callers
/// can later call [`update_audit_status`] to drive it to a terminal
/// state.
pub async fn write_audit_row(pool: &Pool, app_id: &str, row: &AuditRow) -> Result<i64, String> {
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
        .map_err(|e| format!("audit: insert failed: {e}"))?;
    let id: i64 = rows
        .first()
        .map(|r| r.get::<_, i64>("id"))
        .ok_or_else(|| "audit: INSERT returned no row".to_string())?;
    Ok(id)
}

/// Drive an audit row from its current state to a terminal status. The
/// allowed transitions mirror the proposal A3 state machine: `running ->
/// applied | failed`. Returns `Ok(true)` if the row transitioned, `false`
/// if the UPDATE matched nothing (e.g. row already terminal).
pub async fn update_audit_status(
    pool: &Pool,
    app_id: &str,
    id: i64,
    new_status: TerminalStatus,
    error: Option<&str>,
) -> Result<bool, String> {
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
        .map_err(|e| format!("audit: update status failed: {e}"))?;
    Ok(!rows.is_empty())
}

/// Validate an app_id used as a schema name — same rules as the query
/// builder's `validate_schema`. Local copy avoids exporting a private
/// function out of `query.rs`.
fn validate_app_id(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("audit: app_id cannot be empty".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("audit: invalid app_id: {name}"));
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
        assert!(validate_app_id("").is_err());
        assert!(validate_app_id("app\"; DROP TABLE x; --").is_err());
        assert!(validate_app_id("app.other").is_err());
        assert!(validate_app_id("app/other").is_err());
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
        assert_eq!(TerminalStatus::Applied.as_sql(), "applied");
        assert_eq!(TerminalStatus::AppliedWithDeadLetter.as_sql(), "applied_with_dead_letter");
        assert_eq!(TerminalStatus::Failed.as_sql(), "failed");
        assert_eq!(TerminalStatus::Cancelled.as_sql(), "cancelled");
        assert_eq!(ActorKind::Auto.as_sql(), "auto");
    }
}
