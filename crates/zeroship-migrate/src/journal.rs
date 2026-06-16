//! The migration journal — `schema_migrations` (design §2.2).
//!
//! Append-only + tamper-evident. The journal of record,
//! `<meta>.schema_migrations`, holds one row per **completed** migration
//! (version, name, checksum, timestamp, actor, exec time, phase, outcome) and
//! is guarded by an **immutability trigger** that rejects UPDATE and DELETE
//! outright — the billing-ledger pattern (`db/changelog/changesets/
//! 0048_credit_ledger.sql`): a correction is a *new* row, never an edit.
//!
//! Non-transactional migrations (`CREATE INDEX CONCURRENTLY`, …) cannot wrap
//! their DDL + journal write in one transaction, so they use a **two-phase**
//! protocol around a *separate* mutable side-table,
//! `<meta>.schema_migrations_inflight`: write a `started` marker → run the DDL
//! → insert the immutable `completed` row → drop the marker. A crash leaves a
//! lone `started` marker, which the executor's recovery path detects on the
//! next apply. The inflight table is deliberately NOT immutable (the marker
//! must be deletable on completion); only the journal of record is.
//!
//! The journal lives in a per-project **meta schema** distinct from the project
//! schema, so a creator migration confined to its own schema cannot touch its
//! own history. Bootstrap ([`ensure_journal`]) is idempotent.

use compio_postgres::Client;

use crate::db::ExecutorConfig;

/// A journal phase (design §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A non-transactional migration begun but not yet confirmed complete
    /// (a marker in the inflight side-table). A lone `Started` on re-run
    /// signals the recovery path.
    Started,
    /// A migration fully applied + recorded in the immutable journal.
    Completed,
}

impl Phase {
    /// The wire string stored in the `phase` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
        }
    }

    /// Parse a phase from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "started" => Some(Self::Started),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// One journal entry (completed) or inflight marker (started), as read back for
/// the drift check + pending computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEntry {
    /// The migration version (`mig_…`).
    pub version: String,
    /// The recorded checksum (hex SHA-256). Empty for an inflight `started`
    /// marker only if the marker predates a checksum (we always write it).
    pub checksum: String,
    /// The phase the entry is in.
    pub phase: Phase,
}

/// Error from a journal operation.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A database error.
    #[error("journal db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal row carried an unrecognized `phase` value.
    #[error("unrecognized journal phase '{0}'")]
    BadPhase(String),
}

/// Quote a SQL identifier by doubling embedded quotes and wrapping in
/// double-quotes, so a schema name is never interpolated as raw SQL.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Bootstrap (idempotently) the meta schema + journal table + inflight
/// side-table + immutability trigger (design §2.2).
///
/// Safe to call on every apply: `CREATE SCHEMA/TABLE IF NOT EXISTS`, `CREATE OR
/// REPLACE FUNCTION`, and a `pg_trigger`-guarded `CREATE TRIGGER` make a
/// re-bootstrap a no-op.
///
/// # Errors
/// [`JournalError::Db`] on any DDL failure.
pub async fn ensure_journal(conn: &Client, cfg: &ExecutorConfig) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let trg_fn = quote_ident(&format!("{}_schema_migrations_immutable", cfg.meta_schema));
    let trg_name = format!("{}_schema_migrations_immutable_trg", cfg.meta_schema);
    let meta_lit = cfg.meta_schema.replace('\'', "''");

    // 1. Meta schema.
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await?;

    // 2. The append-only journal of record (design §2.2 columns).
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations (
            version     TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            checksum    TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            applied_by  TEXT NOT NULL,
            exec_ms     BIGINT,
            phase       TEXT NOT NULL CHECK (phase IN ('started','completed')),
            outcome     TEXT NOT NULL
        )"
    ))
    .await?;

    // 2b. The MUTABLE inflight side-table for two-phase non-txn markers. NOT
    //     guarded by the immutability trigger — the marker is deleted on
    //     completion / recovery.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_inflight (
            version     TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            checksum    TEXT NOT NULL,
            started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            applied_by  TEXT NOT NULL
        )"
    ))
    .await?;

    // 3. Immutability trigger on the journal of record (billing-ledger pattern,
    //    0048_credit_ledger). Reject UPDATE + DELETE outright.
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION {trg_fn}() RETURNS trigger AS $fn$
         BEGIN
             RAISE EXCEPTION 'schema_migrations is append-only (no UPDATE/DELETE)';
         END;
         $fn$ LANGUAGE plpgsql"
    ))
    .await?;

    // 4. Attach the trigger idempotently (PG 16 has no CREATE TRIGGER IF NOT
    //    EXISTS; guard on pg_trigger).
    conn.batch_execute(&format!(
        "DO $do$ BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_trigger t
                JOIN pg_class c ON c.oid = t.tgrelid
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE t.tgname = '{trg_name}'
                  AND c.relname = 'schema_migrations'
                  AND n.nspname = '{meta_lit}'
            ) THEN
                EXECUTE 'CREATE TRIGGER {trg_name}
                         BEFORE UPDATE OR DELETE ON {meta}.schema_migrations
                         FOR EACH ROW EXECUTE FUNCTION {trg_fn}()';
            END IF;
        END $do$"
    ))
    .await?;

    Ok(())
}

/// Read the journal of record + inflight markers for the drift check + pending
/// computation, ordered by version (`UUIDv7` apply order).
///
/// Completed rows come from `schema_migrations`; `started` markers come from
/// `schema_migrations_inflight`. A version present in both (completed) wins —
/// but in normal operation the inflight marker is cleared on completion, so the
/// overlap only happens transiently.
///
/// # Errors
/// [`JournalError::Db`] on query failure; [`JournalError::BadPhase`] if a stored
/// phase value is unrecognized.
pub async fn applied(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<AppliedEntry>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "SELECT version, checksum, phase FROM (
                     SELECT version, checksum, phase FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, checksum, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM {meta}.schema_migrations m WHERE m.version = i.version
                      )
                 ) j ORDER BY version"
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let version: String = row.get("version");
        let checksum: String = row.get("checksum");
        let phase_s: String = row.get("phase");
        let phase = Phase::parse(&phase_s).ok_or(JournalError::BadPhase(phase_s))?;
        out.push(AppliedEntry {
            version,
            checksum,
            phase,
        });
    }
    Ok(out)
}

/// Write the `started` inflight marker for a non-transactional migration
/// (phase 1 of the two-phase protocol). On a crash this lone marker is what the
/// recovery path keys on.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
pub async fn record_started(
    conn: &Client,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    conn.execute(
        &format!(
            "INSERT INTO {meta}.schema_migrations_inflight
                 (version, name, checksum, applied_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (version) DO NOTHING"
        ),
        &[&version, &name, &checksum, &applied_by],
    )
    .await?;
    Ok(())
}

/// Finalize a non-transactional migration (phase 2): insert the immutable
/// `completed` journal row, then clear the inflight marker.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub async fn record_completed(
    conn: &Client,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    // Plain INSERT — consistent with the transactional path (M3). A `completed`
    // row is written EXACTLY once: the non-txn recovery path only ever runs when
    // there is a lone `started` marker and NO completed row, so a unique-key
    // conflict here is a genuine double-completion bug we must surface, not
    // silently swallow with `ON CONFLICT DO NOTHING`.
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (version, name, checksum, applied_by, exec_ms, phase, outcome)
                 VALUES ($1, $2, $3, $4, $5, 'completed', 'success')"
            ),
            &[&version, &name, &checksum, &applied_by, &exec_ms],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_completed must insert exactly one journal row");
    conn.execute(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = $1"),
        &[&version],
    )
    .await?;
    Ok(())
}

/// Drop a stale inflight marker (recovery path cleanup) without recording a
/// completed row — used when recovery decides the partial work was undone.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub async fn clear_inflight(
    conn: &Client,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    conn.execute(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = $1"),
        &[&version],
    )
    .await?;
    Ok(())
}
