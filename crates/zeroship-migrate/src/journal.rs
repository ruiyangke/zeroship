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

/// How a `completed` event was RECORDED (the journaled `kind` column).
///
/// This is the migration's recorded IDENTITY-class, not anything the caller
/// supplies at apply time. The tamper guard (v3 Plan E re-critic) decides the
/// repeatable drift exemption on THIS journaled value — never on the
/// attacker-suppliable `flags.repeatable` — so a once-only migration cannot be
/// reclassified into a repeatable (or vice-versa) by flipping the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournaledKind {
    /// An ordinary once-only migration whose `up` ran (`kind='apply'`).
    Apply,
    /// An adoption baseline: the `up` was recorded NOT run (`kind='baseline'`).
    Baseline,
    /// A squash supersession (`kind='squash'`).
    Squash,
    /// A repeatable migration's re-apply (`kind='repeatable'`, v3 Plan E). The
    /// only kind whose changed checksum is a legitimate re-run signal.
    Repeatable,
}

impl JournaledKind {
    /// The wire string stored in the `kind` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Baseline => "baseline",
            Self::Squash => "squash",
            Self::Repeatable => "repeatable",
        }
    }

    /// Parse a kind from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apply" => Some(Self::Apply),
            "baseline" => Some(Self::Baseline),
            "squash" => Some(Self::Squash),
            "repeatable" => Some(Self::Repeatable),
            _ => None,
        }
    }

    /// True if this journaled kind is a REPEATABLE re-apply — the only kind whose
    /// changed checksum is a legitimate re-run rather than tamper.
    #[must_use]
    pub const fn is_repeatable(self) -> bool {
        matches!(self, Self::Repeatable)
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
    /// The journaled `kind` of the LATEST `completed` event for this version
    /// (`None` for a lone `started` inflight marker, which has no completed kind).
    /// The drift/tamper guard anchors the repeatable exemption on THIS value, not
    /// on the supplied `flags.repeatable` (v3 Plan E re-critic).
    pub kind: Option<JournaledKind>,
}

/// Error from a journal operation.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A database error.
    #[error("journal db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A **dialect-neutral** journal backend error — used by non-Postgres
    /// [`MigrationBackend`](crate::backend::MigrationBackend) impls (e.g.
    /// `SqliteBackend`) whose journal lives in SQLite, not Postgres, so they
    /// cannot produce a `compio_postgres::Error`. The payload is the backend's
    /// own error string. The Postgres journal helpers never construct this arm.
    #[error("journal backend error: {0}")]
    Backend(String),
    /// A journal row carried an unrecognized `phase` value.
    #[error("unrecognized journal phase '{0}'")]
    BadPhase(String),
    /// A `completed` journal row carried an unrecognized `kind` value — a
    /// corrupted / tampered row (the CHECK constraint forbids it on write, so
    /// seeing one means out-of-band mutation).
    #[error("unrecognized journal kind '{0}'")]
    BadKind(String),
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
    //    The `kind` column (Plan 9) distinguishes how the `completed` event was
    //    recorded, for auditing: an ordinary `apply` (the `up` actually ran), a
    //    `baseline` (the schema already existed; the `up` was recorded NOT run —
    //    adoption path), a `squash` (a supersession; the squash's `up` was
    //    recorded NOT run because `[v1..vN]` were already applied — see
    //    [`record_baseline`] / [`crate::squash`]), or a `repeatable` (v3 Plan E —
    //    a re-applied repeatable's `up` ran, but the version's IDENTITY is a
    //    repeatable, not a once-only). The `repeatable` kind is LOAD-BEARING for the
    //    tamper guard: the drift exemption anchors on the JOURNALED kind, not the
    //    attacker-suppliable `flags.repeatable`, so flipping an applied once-only to
    //    `repeatable=true` is a kind mismatch ⇒ tamper, not a re-run. It does NOT
    //    alter the append-only model — it is just a fact stamped on each immutable
    //    event row.
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
            outcome     TEXT NOT NULL,
            kind        TEXT NOT NULL DEFAULT 'apply'
                          CHECK (kind IN ('apply','baseline','squash','repeatable'))
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

    // 2a-bis. The append-only SUPERSESSION log (Plan 9 squash). One row per
    //     (squash_version → superseded_version) edge, written by the ADMIN when a
    //     squash migration `S` is journaled (whether via `apply` running its `up`
    //     on a fresh DB, or via [`crate::squash`] recording it baseline-style on a
    //     DB that already ran `[v1..vN]`). The pending computation joins this
    //     against net-applied squashes to decide that a superseded version is
    //     SATISFIED. Append-only + immutable (trigger below): a squash's
    //     supersession edges are part of history and never edited/deleted. Edges
    //     are recorded LAST (after the `completed` row), so a net-applied squash
    //     always has its full edge set; a partial edge set never exists because the
    //     squash's `completed` row + its edges are written in one transaction by the
    //     caller. (No FK to `schema_migrations` — that table allows multiple
    //     `completed` rows per version, so there is no single PK to reference; the
    //     squash_version is validated by the caller before journaling.)
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_supersedes (
            event_seq          BIGINT PRIMARY KEY DEFAULT nextval('{meta}.schema_migrations_event_seq'),
            squash_version     TEXT NOT NULL,
            superseded_version TEXT NOT NULL,
            recorded_at        TIMESTAMPTZ NOT NULL DEFAULT now()
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

    // 4. Attach the immutability triggers idempotently to ALL THREE append-only
    //    event tables (PG 16 has no CREATE TRIGGER IF NOT EXISTS; guard on
    //    pg_trigger).
    //
    //    TWO triggers per table, both calling the same RAISE function:
    //      - `BEFORE UPDATE OR DELETE ... FOR EACH ROW` — blocks row mutation.
    //      - `BEFORE TRUNCATE ... FOR EACH STATEMENT` — blocks TRUNCATE, which
    //        row-level triggers DO NOT fire on. Without the statement-level
    //        TRUNCATE trigger, `TRUNCATE {meta}.schema_migrations` would silently
    //        wipe the append-only journal. (Defense-in-depth: TRUNCATE is only
    //        reachable on the trusted-admin path — the migrator role has no grant
    //        on the meta schema — but the journal must be immutable by
    //        construction, not by least-privilege alone.)
    //
    //    Trigger names are SHORT and table-local (`zs_immutable_trg`,
    //    `zs_immutable_truncate_trg`) — a trigger name only needs to be unique
    //    per table, not per schema, so it need NOT embed the meta_schema. This is
    //    deliberate: the meta_schema under the per-app deploy model is
    //    `"<app_id>_migrations"` (a hyphenated UUID, ~37 chars). Embedding it in
    //    the trigger name overflows PostgreSQL's 63-byte NAMEDATALEN limit — the
    //    name is silently truncated, which (a) makes distinct row vs TRUNCATE
    //    names collide and (b) makes the full-name pg_trigger existence guard
    //    never match the truncated catalog name (re-bootstrap churn). Short
    //    fixed names sidestep all of that. They are still quoted as identifiers
    //    (`trg_q`) for uniformity, and the existence check compares the raw name
    //    as a string literal (`trg_lit`).
    for tbl in [
        "schema_migrations",
        "schema_migrations_rolled_back",
        "schema_migrations_supersedes",
    ] {
        for (trg, level, events) in [
            ("zs_immutable_trg", "FOR EACH ROW", "UPDATE OR DELETE"),
            (
                "zs_immutable_truncate_trg",
                "FOR EACH STATEMENT",
                "TRUNCATE",
            ),
        ] {
            let trg_lit = trg.replace('\'', "''");
            let trg_q = quote_ident(trg);
            conn.batch_execute(&format!(
                "DO $do$ BEGIN
                    IF NOT EXISTS (
                        SELECT 1 FROM pg_trigger t
                        JOIN pg_class c ON c.oid = t.tgrelid
                        JOIN pg_namespace n ON n.oid = c.relnamespace
                        WHERE t.tgname = '{trg_lit}'
                          AND c.relname = '{tbl}'
                          AND n.nspname = '{meta_lit}'
                    ) THEN
                        EXECUTE 'CREATE TRIGGER {trg_q}
                                 BEFORE {events} ON {meta}.{tbl}
                                 {level} EXECUTE FUNCTION {trg_fn}()';
                    END IF;
                END $do$"
            ))
            .await?;
        }
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
    // `event_kind` distinguishes the source table (completed vs rolled_back) for
    // the net-state decision; `mig_kind` carries the journaled `kind` column of a
    // `completed` row (NULL for a rolled_back event, which has no kind) and rides
    // through to the net-applied entry so the drift/tamper guard can read it.
    let rows = conn
        .query(
            &format!(
                "WITH events AS (
                     SELECT version, checksum, event_seq,
                            'completed' AS event_kind, kind AS mig_kind
                       FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, checksum, event_seq,
                            'rolled_back' AS event_kind, NULL AS mig_kind
                       FROM {meta}.schema_migrations_rolled_back
                 ),
                 latest AS (
                     SELECT DISTINCT ON (version) version, checksum, event_kind, mig_kind
                       FROM events
                      ORDER BY version, event_seq DESC
                 ),
                 net_completed AS (
                     SELECT version, checksum, mig_kind
                       FROM latest WHERE event_kind = 'completed'
                 ),
                 union_all AS (
                     SELECT version, checksum, mig_kind, 'completed' AS phase
                       FROM net_completed
                     UNION ALL
                     SELECT version, checksum, NULL AS mig_kind, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM net_completed n WHERE n.version = i.version
                      )
                 )
                 SELECT version, checksum, mig_kind, phase FROM union_all
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
        // The journaled kind of the latest completed event. A `started` marker
        // carries NULL; a completed row whose kind is unrecognized is a tampered /
        // corrupt journal row — surface it rather than silently treating it as a
        // benign apply.
        let kind = match row.try_get::<_, Option<String>>("mig_kind") {
            Ok(Some(s)) => Some(JournaledKind::parse(&s).ok_or(JournalError::BadKind(s))?),
            _ => None,
        };
        out.push(AppliedEntry {
            version,
            checksum,
            phase,
            kind,
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

/// The fields of a non-transactional `completed` journal event, bundled so
/// [`record_completed`] takes one descriptor (keeping the arg count in check).
#[derive(Debug, Clone, Copy)]
pub struct CompletedRecord<'a> {
    /// The migration version (`mig_…`).
    pub version: &'a str,
    /// The migration name.
    pub name: &'a str,
    /// The migration's checksum.
    pub checksum: &'a str,
    /// The actor recorded in the journal.
    pub applied_by: &'a str,
    /// Wall time the `up` took, in milliseconds.
    pub exec_ms: i64,
    /// The migration kind: `'apply'` for an ordinary migration, `'squash'` for a
    /// fresh-path squash. A fresh-path squash MUST be stamped `'squash'` so its
    /// supersession edges are honored by [`superseded_versions`] (#4 restricts to
    /// `kind = 'squash'`).
    pub kind: &'a str,
}

/// Finalize a non-transactional migration (phase 2): insert the immutable
/// `completed` journal row, then clear the inflight marker.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub async fn record_completed(
    conn: &Client,
    cfg: &ExecutorConfig,
    rec: CompletedRecord<'_>,
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
                     (version, name, checksum, applied_by, exec_ms, phase, outcome, kind)
                 VALUES ($1, $2, $3, $4, $5, 'completed', 'success', $6)"
            ),
            &[
                &rec.version,
                &rec.name,
                &rec.checksum,
                &rec.applied_by,
                &rec.exec_ms,
                &rec.kind,
            ],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_completed must insert exactly one journal row");
    conn.execute(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = $1"),
        &[&rec.version],
    )
    .await?;
    Ok(())
}

/// Count the number of versions the journal currently records as **net-applied**
/// (latest event per version is `completed`) — the first-entry test for
/// [`crate::baseline`].
///
/// Baseline is a FIRST-entry operation: you cannot baseline a DB the engine
/// already manages. This counts net-applied versions exactly as [`applied`]
/// computes them (latest event per version is `completed`), so a version that was
/// applied then rolled back does NOT count (it is pending again). A non-zero count
/// means the engine already manages real history ⇒ baseline must refuse.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub async fn applied_count(conn: &Client, cfg: &ExecutorConfig) -> Result<i64, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let row = conn
        .query_one(
            &format!(
                "WITH events AS (
                     SELECT version, event_seq, 'completed' AS kind
                       FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, event_seq, 'rolled_back' AS kind
                       FROM {meta}.schema_migrations_rolled_back
                 ),
                 latest AS (
                     SELECT DISTINCT ON (version) version, kind
                       FROM events
                      ORDER BY version, event_seq DESC
                 )
                 SELECT count(*)::bigint AS n FROM latest WHERE kind = 'completed'"
            ),
            &[],
        )
        .await?;
    Ok(row.get("n"))
}

/// Read the set of versions **superseded by a net-applied squash** (Plan 9).
///
/// A version `v_i` is satisfied-by-supersession when some squash `S` with an edge
/// `S → v_i` in `schema_migrations_supersedes` is itself **net-applied** (its
/// latest event in `schema_migrations`/`…_rolled_back` is `completed`) AND `S`'s
/// recorded `kind` is `'squash'`. The executor unions this with the net-applied set
/// to compute `pending`, so a superseded `v_i` is never (re-)run.
///
/// Only edges of a NET-APPLIED squash count: if `S` was rolled back, its
/// supersession no longer holds and the superseded versions become pending again
/// (consistent with `S` itself being pending again).
///
/// #4 — the `kind = 'squash'` restriction is load-bearing: without it, any
/// net-applied version whose `version` collided with a corrupted/forged edge's
/// `squash_version` could over-supersede (suppress a real migration). Only a
/// genuine recorded squash may supersede.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub async fn superseded_versions(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<String>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "WITH events AS (
                     SELECT version, event_seq, 'completed' AS event_kind, kind AS mig_kind
                       FROM {meta}.schema_migrations
                     UNION ALL
                     SELECT version, event_seq, 'rolled_back' AS event_kind, NULL AS mig_kind
                       FROM {meta}.schema_migrations_rolled_back
                 ),
                 latest AS (
                     SELECT DISTINCT ON (version) version, event_kind, mig_kind
                       FROM events
                      ORDER BY version, event_seq DESC
                 ),
                 net_applied_squashes AS (
                     -- #4: only a GENUINE recorded squash can supersede. Without the
                     -- mig_kind = 'squash' filter, any net-applied version whose
                     -- `version` collided with a (corrupted/forged) edge's
                     -- `squash_version` would over-supersede — suppressing a real
                     -- migration (the C1 forgery class).
                     SELECT version FROM latest
                      WHERE event_kind = 'completed' AND mig_kind = 'squash'
                 )
                 SELECT DISTINCT s.superseded_version AS v
                   FROM {meta}.schema_migrations_supersedes s
                   JOIN net_applied_squashes n ON n.version = s.squash_version
                  ORDER BY 1"
            ),
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get::<_, String>("v")).collect())
}

/// Read the **latest `completed` checksum per version** from the journal of
/// record (v3 Plan E — repeatables).
///
/// A repeatable migration ([`MigrationFlags::repeatable`](crate::migration::MigrationFlags::repeatable))
/// has a STABLE identity (its `version`/name never changes across edits) and is
/// re-applied whenever its definition checksum changes. Each re-apply appends a
/// fresh `completed` event for the same version (append-only), so a repeatable
/// accrues several `completed` rows over its lifetime. To decide whether to
/// re-run, the executor compares the migration's current checksum against the
/// **most recent** `completed` event's checksum for that identity.
///
/// Returns a map `version → latest completed checksum`, taking the latest by the
/// shared monotonic `event_seq` (which never ties, even within one transaction).
/// Versions with no `completed` row are absent from the map (never applied).
///
/// Unlike [`applied`], this reads ONLY `schema_migrations` (the completed-event
/// table) and is INDIFFERENT to rollback: a repeatable carries `down: None` and
/// is never rolled back, so its latest event is always its newest `completed`
/// one. (Reading `…_rolled_back` here would be meaningless — there are no
/// repeatable rollbacks — and would risk masking the latest completed checksum
/// behind an unrelated event.) The drift/pending machinery still uses [`applied`]
/// for versioned migrations; this is the repeatable-specific lens.
///
/// **Kind-aware (v3 Plan E re-critic #2).** Only events whose journaled
/// `kind='repeatable'` are consulted — the re-run oracle must never read a
/// once-only `kind='apply'` (or baseline/squash) row's checksum as a repeatable's
/// "prior" value. Combined with the [`applied`]-driven kind-mismatch abort in the
/// drift check, this keeps the repeatable re-run path strictly about genuine
/// repeatable history: a version that was applied once-only (and would only reach
/// this lookup via the tamper flip, which the drift check already aborts) has no
/// `repeatable`-kind event, so it is absent from the map — never silently re-run.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub async fn latest_completed_checksums(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<std::collections::HashMap<String, String>, JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "SELECT DISTINCT ON (version) version, checksum
                   FROM {meta}.schema_migrations
                  WHERE kind = 'repeatable'
                  ORDER BY version, event_seq DESC"
            ),
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>("version"), r.get::<_, String>("checksum")))
        .collect())
}

/// The fields of a baseline/squash `completed` event recorded WITHOUT running its
/// `up` (Plan 9), bundled so [`record_baseline`] takes one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct BaselineRecord<'a> {
    /// The migration version (`mig_…`).
    pub version: &'a str,
    /// The migration name.
    pub name: &'a str,
    /// The migration's checksum (so the drift check compares correctly later).
    pub checksum: &'a str,
    /// The actor recorded in the journal (operator / admin).
    pub applied_by: &'a str,
    /// The event `kind`: `'baseline'` (adoption) or `'squash'` (supersession).
    pub kind: &'a str,
    /// The versions this event supersedes (empty for a baseline; `[v1..vN]` for a
    /// squash recorded on an existing DB).
    pub supersedes: &'a [&'a str],
}

/// Journal a baseline/squash `completed` event WITHOUT running its `up` (Plan 9).
///
/// The event carries an explicit `kind` (`'baseline'` or `'squash'`) and an
/// optional supersession edge set. Run by the ADMIN (the migrator has no
/// meta-schema grant), exactly like [`record_completed`]. `exec_ms` is recorded as
/// 0 (no SQL ran).
///
/// This is the journal-without-running primitive shared by [`crate::baseline`]
/// (the adoption path: the schema already physically exists, so the `up` is
/// recorded not run) and [`crate::squash`]'s existing-DB path (a supersession: the
/// effect of `[v1..vN]` is already present, so the squash's `up` is recorded not
/// run). #3 fix: the `completed` row + every supersession edge are inserted in ONE
/// transaction THIS function brackets (`BEGIN … COMMIT`, ROLLBACK on any error), so
/// a net-applied squash always carries its full edge set (no partial-edge window) —
/// a crash between the row and the edges can no longer leave `S` net-applied with
/// partial/empty edges (the advisory lock the callers hold gives mutual exclusion,
/// NOT atomicity). Append-only: never an UPDATE/DELETE.
///
/// # Errors
/// [`JournalError::Db`] on insert failure (the partial work is rolled back).
pub async fn record_baseline(
    conn: &Client,
    cfg: &ExecutorConfig,
    rec: BaselineRecord<'_>,
) -> Result<(), JournalError> {
    conn.batch_execute("BEGIN").await?;
    let result = record_baseline_inner(conn, cfg, rec).await;
    if let Err(e) = result {
        // Roll back the partial row/edges; surface the original error.
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %rec.version, "zeroship-migrate: ROLLBACK failed after a record_baseline error (#3)");
        }
        return Err(e);
    }
    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// The row + edge INSERTs of [`record_baseline`], run INSIDE its `BEGIN … COMMIT`
/// (#3). Split out so the caller can ROLLBACK on the first failure, making the
/// completed row and its full edge set atomic.
async fn record_baseline_inner(
    conn: &Client,
    cfg: &ExecutorConfig,
    rec: BaselineRecord<'_>,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (version, name, checksum, applied_by, exec_ms, phase, outcome, kind)
                 VALUES ($1, $2, $3, $4, 0, 'completed', 'success', $5)"
            ),
            &[&rec.version, &rec.name, &rec.checksum, &rec.applied_by, &rec.kind],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_baseline must insert exactly one journal row");
    for sup in rec.supersedes {
        conn.execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_supersedes
                     (squash_version, superseded_version)
                 VALUES ($1, $2)"
            ),
            &[&rec.version, sup],
        )
        .await?;
    }
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
