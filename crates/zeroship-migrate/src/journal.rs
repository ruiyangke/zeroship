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

    // 1b. A single monotonic sequence shared by BOTH event tables
    //     (schema_migrations + schema_migrations_rolled_back), so the latest event
    //     per version is decided by a total order that never ties — `now()` can be
    //     equal across two events in one transaction / fast succession, but the
    //     sequence is strictly increasing. `applied()` reads net state off it.
    conn.batch_execute(&format!(
        "CREATE SEQUENCE IF NOT EXISTS {meta}.schema_migrations_event_seq"
    ))
    .await?;

    // 2. The append-only journal of record (design §2.2 columns).
    //
    //    Rollback is append-only too (Plan 5): a `completed` row is NEVER deleted
    //    on rollback — a `rolled_back` event is appended to the side table below.
    //    A rolled-back migration becomes pending again and may be RE-APPLIED,
    //    which appends a NEW `completed` row for the same version. So `version` is
    //    NOT a primary key here (multiple completed events per version are legal,
    //    across rollback↔re-apply cycles); the surrogate `event_seq` is the PK and
    //    the total order. The immutability trigger still forbids UPDATE/DELETE, so
    //    the log stays append-only.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations (
            event_seq   BIGINT PRIMARY KEY DEFAULT nextval('{meta}.schema_migrations_event_seq'),
            version     TEXT NOT NULL,
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

    // 2a. The append-only ROLLBACK event log (Plan 5). One row per `rolled_back`
    //     event, written by the ADMIN (the migrator has no grant on the meta
    //     schema — Plan 3 C1). It shares the same monotonic sequence as
    //     schema_migrations so `applied()` can order a version's completed vs
    //     rolled-back events on one total scale. Immutable too (trigger below).
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_rolled_back (
            event_seq     BIGINT PRIMARY KEY DEFAULT nextval('{meta}.schema_migrations_event_seq'),
            version       TEXT NOT NULL,
            name          TEXT NOT NULL,
            checksum      TEXT NOT NULL,
            rolled_back_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            rolled_back_by TEXT NOT NULL,
            exec_ms       BIGINT
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

    // 3. Immutability trigger function (billing-ledger pattern,
    //    0048_credit_ledger). Reject UPDATE + DELETE outright. Shared by both
    //    append-only event tables (schema_migrations + …_rolled_back).
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION {trg_fn}() RETURNS trigger AS $fn$
         BEGIN
             RAISE EXCEPTION 'migration journal is append-only (no UPDATE/DELETE)';
         END;
         $fn$ LANGUAGE plpgsql"
    ))
    .await?;

    // 4. Attach the trigger idempotently to BOTH append-only event tables (PG 16
    //    has no CREATE TRIGGER IF NOT EXISTS; guard on pg_trigger).
    for (tbl, trg) in [
        ("schema_migrations", trg_name.clone()),
        (
            "schema_migrations_rolled_back",
            format!("{}_schema_migrations_rolled_back_immutable_trg", cfg.meta_schema),
        ),
    ] {
        conn.batch_execute(&format!(
            "DO $do$ BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_trigger t
                    JOIN pg_class c ON c.oid = t.tgrelid
                    JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE t.tgname = '{trg}'
                      AND c.relname = '{tbl}'
                      AND n.nspname = '{meta_lit}'
                ) THEN
                    EXECUTE 'CREATE TRIGGER {trg}
                             BEFORE UPDATE OR DELETE ON {meta}.{tbl}
                             FOR EACH ROW EXECUTE FUNCTION {trg_fn}()';
                END IF;
            END $do$"
        ))
        .await?;
    }

    Ok(())
}

/// Read the **net applied state** of the journal for the drift check + pending
/// computation, ordered by version (`UUIDv7` apply order).
///
/// The journal is append-only, including rollback (Plan 5): a `completed` row is
/// never deleted; rollback **appends** a `rolled_back` event to
/// `schema_migrations_rolled_back`, and a re-apply appends a fresh `completed`
/// row. So a version can carry several events over rollback↔re-apply cycles. The
/// NET state of a version is decided by its **latest event** on the shared
/// monotonic `event_seq` scale:
///
/// - latest event is `completed` ⇒ the version is **applied** (returned as a
///   [`Phase::Completed`] entry carrying that latest completed row's checksum, so
///   the drift check compares against the current incarnation);
/// - latest event is `rolled_back` ⇒ the version is **pending again** (NOT
///   returned as completed; it re-enters `pending = set − completed` and can be
///   re-applied);
/// - no completed row at all but a lone `started` inflight marker ⇒ returned as a
///   [`Phase::Started`] entry (the non-txn crash-recovery key), exactly as before.
///
/// # Errors
/// [`JournalError::Db`] on query failure; [`JournalError::BadPhase`] if a stored
/// phase value is unrecognized.
pub async fn applied(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<AppliedEntry>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    // Union every completed + rolled_back event onto one (event_seq, kind, …)
    // stream, take the LATEST per version with DISTINCT ON, and keep only the
    // versions whose latest event is `completed` (net-applied). Then UNION the
    // lone `started` inflight markers for versions that are NOT net-completed.
    let rows = conn
        .query(
            &format!(
                "WITH events AS (
                     SELECT version, checksum, event_seq, 'completed' AS kind
                       FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, checksum, event_seq, 'rolled_back' AS kind
                       FROM {meta}.schema_migrations_rolled_back
                 ),
                 latest AS (
                     SELECT DISTINCT ON (version) version, checksum, kind
                       FROM events
                      ORDER BY version, event_seq DESC
                 ),
                 net_completed AS (
                     SELECT version, checksum FROM latest WHERE kind = 'completed'
                 ),
                 union_all AS (
                     SELECT version, checksum, 'completed' AS phase FROM net_completed
                     UNION ALL
                     SELECT version, checksum, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM net_completed n WHERE n.version = i.version
                      )
                 )
                 SELECT version, checksum, phase FROM union_all
                 ORDER BY version COLLATE \"C\""
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

/// A net-rolled-back version: one whose **latest** event (on the shared
/// `event_seq` scale) is a `rolled_back` event. Such a version is pending again
/// and re-appliable; the status API surfaces it distinctly from net-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBackEntry {
    /// The version (`mig_…`).
    pub version: String,
    /// The migration name recorded on the rollback event.
    pub name: String,
    /// The checksum recorded on the rollback event.
    pub checksum: String,
    /// Who performed the rollback (`applied_by`-equivalent actor string).
    pub rolled_back_by: String,
    /// The rollback `down` execution time in ms (the recorded `exec_ms`).
    pub exec_ms: Option<i64>,
    /// When the rollback was recorded (RFC-3339 / ISO-8601 from `timestamptz`).
    pub at: String,
}

/// The kind of a [`HistoryEvent`] — a forward apply or a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// A `completed` apply event (from `schema_migrations`).
    Completed,
    /// A `rolled_back` event (from `schema_migrations_rolled_back`).
    RolledBack,
}

/// One event in the FULL append-only audit log (completed + rolled_back),
/// returned by [`history`] in `event_seq` order.
///
/// Unlike [`applied`] (which computes NET state and hides rolled-back history),
/// this is the raw audit trail: it shows EVERY event, including a version's
/// rollback and any subsequent re-apply, in the order they happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// The shared monotonic sequence number (the total event order).
    pub event_seq: i64,
    /// The migration version (`mig_…`).
    pub version: String,
    /// The migration name recorded on the event.
    pub name: String,
    /// Apply or rollback.
    pub kind: HistoryKind,
    /// The event timestamp (RFC-3339 / ISO-8601 from `timestamptz`).
    pub at: String,
    /// Execution time in ms (the `exec_ms` recorded on the event), if any.
    pub exec_ms: Option<i64>,
    /// The actor who performed the event (`applied_by` / `rolled_back_by`).
    pub applied_by: String,
    /// The checksum recorded on the event.
    pub checksum: String,
}

/// Read the versions whose **latest** event is a `rolled_back` event (net
/// rolled-back), ordered by version.
///
/// Mirrors [`applied`]'s DISTINCT-ON-latest-event logic, but keeps the versions
/// whose winning event is `rolled_back` (not `completed`). Carries the rollback
/// event's detail (name, checksum, actor, exec_ms, timestamp) for the status API.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub async fn net_rolled_back(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<RolledBackEntry>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "WITH events AS (
                     SELECT version, name, checksum, applied_by AS actor, exec_ms,
                            applied_at AS at, event_seq, 'completed' AS kind
                       FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, name, checksum, rolled_back_by AS actor, exec_ms,
                            rolled_back_at AS at, event_seq, 'rolled_back' AS kind
                       FROM {meta}.schema_migrations_rolled_back
                 ),
                 latest AS (
                     SELECT DISTINCT ON (version)
                            version, name, checksum, actor, exec_ms, at, kind
                       FROM events
                      ORDER BY version, event_seq DESC
                 )
                 SELECT version, name, checksum, actor,
                        exec_ms, to_char(at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS at
                   FROM latest
                  WHERE kind = 'rolled_back'
                  ORDER BY version COLLATE \"C\""
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(RolledBackEntry {
            version: row.get("version"),
            name: row.get("name"),
            checksum: row.get("checksum"),
            rolled_back_by: row.get("actor"),
            exec_ms: row.get("exec_ms"),
            at: row.get("at"),
        });
    }
    Ok(out)
}

/// Read the FULL append-only event log (every `completed` + every `rolled_back`
/// event) in `event_seq` order — the audit trail (design §2.2, scenario 46).
///
/// This is NOT net state: a version that was applied, rolled back, and re-applied
/// appears as three events here. Read-only.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub async fn history(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<HistoryEvent>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "SELECT event_seq, version, name,
                        to_char(applied_at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS at,
                        exec_ms, applied_by AS actor, checksum, 'completed' AS kind
                   FROM {meta}.schema_migrations
                 UNION ALL
                 SELECT event_seq, version, name,
                        to_char(rolled_back_at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS at,
                        exec_ms, rolled_back_by AS actor, checksum, 'rolled_back' AS kind
                   FROM {meta}.schema_migrations_rolled_back
                 ORDER BY event_seq"
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let kind_s: String = row.get("kind");
        let kind = match kind_s.as_str() {
            "completed" => HistoryKind::Completed,
            "rolled_back" => HistoryKind::RolledBack,
            other => return Err(JournalError::BadPhase(other.to_string())),
        };
        out.push(HistoryEvent {
            event_seq: row.get("event_seq"),
            version: row.get("version"),
            name: row.get("name"),
            kind,
            at: row.get("at"),
            exec_ms: row.get("exec_ms"),
            applied_by: row.get("actor"),
            checksum: row.get("checksum"),
        });
    }
    Ok(out)
}

/// Append a `rolled_back` event for a version (Plan 5 rollback).
///
/// Run by the **ADMIN** (the migrator has no grant on the meta schema — Plan 3
/// C1): the executor brackets a rollback's `down` SQL under the migrator role,
/// then `RESET ROLE`s back to admin before this journal append, exactly mirroring
/// the up path. The append is immutable (UPDATE/DELETE forbidden by trigger); a
/// later re-apply appends a fresh `completed` row, and `applied()` reads the
/// latest event per version off the shared `event_seq`.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
pub async fn record_rolled_back(
    conn: &Client,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    rolled_back_by: &str,
    exec_ms: i64,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_rolled_back
                     (version, name, checksum, rolled_back_by, exec_ms)
                 VALUES ($1, $2, $3, $4, $5)"
            ),
            &[&version, &name, &checksum, &rolled_back_by, &exec_ms],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_rolled_back must insert exactly one event row");
    Ok(())
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
    // Plain INSERT (consistent with the transactional path, M3). `event_seq` is a
    // surrogate identity PK, so this appends a fresh `completed` event — including
    // a re-apply after a rollback (Plan 5), where a prior `completed` + a later
    // `rolled_back` already exist for this version and `applied()` made it pending
    // again. Append-only: never an UPDATE.
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
