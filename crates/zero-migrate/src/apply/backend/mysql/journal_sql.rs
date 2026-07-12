//! The MySQL journal: schema (database), immutability, native `event_seq`, and
//! the net-state reads/writes for [`MysqlBackend`](super::MysqlBackend).
//!
//! This is the MySQL analogue of the Postgres [`crate::apply::journal`] module:
//! it carries the SAME logical journal shape — a SINGLE consolidated
//! `schema_migrations` events table (one row per `applied`/`rolled_back` event,
//! discriminated by `event_kind`), a `_supersedes` edge table, an inflight
//! side-table, and net-state computed over the native total order — but every
//! statement is rendered in MySQL dialect:
//!
//! - **native total order** — `event_seq BIGINT AUTO_INCREMENT PRIMARY KEY`
//!   (MySQL's monotonic surrogate) replaces Postgres' `BIGINT GENERATED ALWAYS AS
//!   IDENTITY`;
//! - **timestamps** — `TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6)` replaces
//!   `TIMESTAMPTZ DEFAULT now()`;
//! - **keyed text columns** — `VARCHAR(255)` (MySQL cannot index a bare `TEXT`
//!   without a prefix length) replaces `TEXT` for `version`/`checksum`/etc.;
//! - **immutability** — `BEFORE UPDATE`/`BEFORE DELETE` triggers that
//!   `SIGNAL SQLSTATE '45000'` replace the plpgsql `RAISE EXCEPTION` trigger
//!   function (MySQL has no per-statement `TRUNCATE` trigger, but `TRUNCATE`
//!   requires the `DROP` privilege the least-privilege migrator role lacks, and
//!   the meta database is admin-owned — defense-in-depth still holds through the
//!   UPDATE/DELETE triggers + privilege model);
//! - **placeholders** — every bind is the anonymous positional `?`
//!   ([`PlaceholderStyle::Question`](crate::apply::backend::PlaceholderStyle::Question)),
//!   never Postgres' `$N`;
//! - **net state** — a MySQL-8 window-function (`ROW_NUMBER() OVER (PARTITION BY
//!   version ORDER BY event_seq DESC)`) replaces Postgres' `DISTINCT ON`, and
//!   `COLLATE utf8mb4_bin` replaces `COLLATE "C"` for a byte-ordered version sort;
//! - **upsert** — `INSERT IGNORE` replaces `ON CONFLICT (version) DO NOTHING`.
//!
//! The meta schema (a MySQL *database*) is admin-owned and off the migrator's
//! reach, exactly as on Postgres — the journal is unforgeable by a confined
//! creator `up`.

use crate::apply::journal::{
    AppliedEntry, CompletedRecord, EventKind, JournalError, JournaledKind, Phase,
};
use crate::conn::ExecutorConfig;
use crate::seam::SqlSession;

/// The fixed, short, table-local immutability trigger names. ASCII-safe literals —
/// never embed the (hyphenated-UUID) app id (the meta database name carries it).
/// A MySQL trigger name is unique per SCHEMA (unlike Postgres' per-table), so the
/// row/table pair is distinguished by an ordinal suffix per guarded table.
const IMMUTABLE_TRG_PREFIX: &str = "zm_immutable";

/// Quote a MySQL identifier with backticks, doubling any embedded backtick, and
/// fail-closed on an empty / NUL-bearing name — the MySQL analogue of the shared
/// `quote_ident_checked` seam (which emits Postgres double-quotes). A schema /
/// table / trigger name is NEVER interpolated as raw SQL.
///
/// # Errors
/// [`JournalError::Backend`] on an empty or NUL-bearing identifier.
pub(crate) fn quote_ident_mysql(ident: &str) -> Result<String, JournalError> {
    if ident.is_empty() {
        return Err(JournalError::Backend(
            "mysql journal: refusing to quote an empty identifier".to_string(),
        ));
    }
    if ident.contains('\0') {
        return Err(JournalError::Backend(
            "mysql journal: refusing to quote a NUL-bearing identifier".to_string(),
        ));
    }
    Ok(format!("`{}`", ident.replace('`', "``")))
}

/// Bootstrap (idempotently) the meta database + journal table + supersedes edge
/// table + inflight side-table + immutability triggers (the MySQL analogue of
/// [`crate::apply::journal::ensure_journal`]).
///
/// Safe to call on every apply: `CREATE {DATABASE,TABLE} IF NOT EXISTS` and
/// `information_schema.triggers`-guarded `CREATE TRIGGER`s make a re-bootstrap a
/// no-op.
///
/// # Errors
/// [`JournalError::Db`] on any DDL failure.
pub(crate) async fn ensure_journal<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let meta_lit = cfg.pg.meta_schema.replace('\'', "''");

    // 1. Meta database (MySQL's "schema").
    conn.batch(&format!("CREATE DATABASE IF NOT EXISTS {meta}"))
        .await?;

    // 2. The append-only journal of record — the SINGLE consolidated events table.
    //    `event_seq BIGINT AUTO_INCREMENT PRIMARY KEY` is the native total order
    //    (MySQL assigns it on INSERT; never supplied). `version` is a VARCHAR(255)
    //    (indexable; MySQL cannot key a bare TEXT), NOT unique — rollback↔re-apply
    //    appends multiple rows. The applied-only columns (kind/phase/outcome) are
    //    NULL on a `rolled_back` row; a CHECK documents the per-event_kind shape
    //    (MySQL 8.0.16+ enforces CHECK). InnoDB for transactional DDL+journal
    //    atomicity on the txn apply path.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations (
            event_seq   BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            event_kind  VARCHAR(16)  NOT NULL,
            version     VARCHAR(255) NOT NULL,
            name        VARCHAR(255) NOT NULL,
            checksum    VARCHAR(255) NOT NULL,
            `at`        TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            `by`        VARCHAR(255) NOT NULL,
            exec_ms     BIGINT,
            phase       VARCHAR(16),
            outcome     VARCHAR(16),
            kind        VARCHAR(16),
            CONSTRAINT schema_migrations_event_kind_chk
                CHECK (event_kind IN ('applied','rolled_back')),
            CONSTRAINT schema_migrations_phase_chk
                CHECK (phase IS NULL OR phase IN ('started','completed')),
            CONSTRAINT schema_migrations_kind_chk
                CHECK (kind IS NULL OR kind IN ('apply','baseline','squash','repeatable')),
            CONSTRAINT schema_migrations_event_shape CHECK (
                (event_kind = 'applied'
                     AND kind IS NOT NULL AND phase IS NOT NULL AND outcome IS NOT NULL)
                OR
                (event_kind = 'rolled_back'
                     AND kind IS NULL AND phase IS NULL AND outcome IS NULL)
            )
        ) ENGINE=InnoDB"
    ))
    .await?;

    // 2a. The append-only SUPERSESSION edge log (squash). One row per
    //     (squash_version → superseded_version) edge; its own AUTO_INCREMENT PK.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_supersedes (
            id                 BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            squash_version     VARCHAR(255) NOT NULL,
            superseded_version VARCHAR(255) NOT NULL,
            recorded_at        TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        ) ENGINE=InnoDB"
    ))
    .await?;

    // 2b. The MUTABLE inflight side-table for two-phase non-txn markers. NOT
    //     guarded by the immutability triggers — the marker is deleted on
    //     completion / recovery. (MySQL DDL is auto-committing, so the non-txn
    //     two-phase path is the norm for every MySQL migration; see the backend's
    //     `ddl_is_transactional`.)
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_inflight (
            version     VARCHAR(255) NOT NULL PRIMARY KEY,
            name        VARCHAR(255) NOT NULL,
            checksum    VARCHAR(255) NOT NULL,
            started_at  TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            applied_by  VARCHAR(255) NOT NULL
        ) ENGINE=InnoDB"
    ))
    .await?;

    // 3. Immutability triggers on BOTH append-only tables (the events table +
    //    _supersedes). MySQL has no `CREATE TRIGGER IF NOT EXISTS` before 8.0.29
    //    and no per-statement TRUNCATE trigger, so each BEFORE UPDATE / BEFORE
    //    DELETE trigger is created only when `information_schema.triggers` shows it
    //    absent, and it `SIGNAL`s SQLSTATE '45000' to abort the row mutation. (A
    //    `TRUNCATE TABLE` bypasses row triggers, but it needs the DROP privilege
    //    the least-privilege migrator role lacks, and the meta database is
    //    admin-owned — the append-only guarantee rests on triggers + privilege
    //    model, matching the PG side's defense-in-depth posture.)
    for (ord, tbl) in ["schema_migrations", "schema_migrations_supersedes"]
        .into_iter()
        .enumerate()
    {
        let tbl_q = quote_ident_mysql(tbl)?;
        let tbl_lit = tbl.replace('\'', "''");
        for (op, verb) in [("UPDATE", "update"), ("DELETE", "delete")] {
            // Trigger names are unique per SCHEMA in MySQL (not per table), so the
            // name embeds a per-table ordinal + the op — short, fixed, ASCII (never
            // the hyphenated-UUID meta database name, which would blow MySQL's
            // 64-char identifier limit).
            let trg = format!("{IMMUTABLE_TRG_PREFIX}_{ord}_{verb}");
            let trg_q = quote_ident_mysql(&trg)?;
            let trg_lit = trg.replace('\'', "''");
            // Guard on information_schema so re-bootstrap is a no-op. The whole
            // block is a single simple-query batch (no client-side statement
            // splitting of the trigger body needed — the guard + CREATE TRIGGER are
            // two batches).
            let exists: Vec<crate::seam::Row> = conn
                .query(
                    "SELECT trigger_name FROM information_schema.triggers \
                     WHERE trigger_schema = ? AND trigger_name = ?",
                    &[meta_lit.as_str().into(), trg_lit.as_str().into()],
                )
                .await?;
            if exists.is_empty() {
                conn.batch(&format!(
                    "CREATE TRIGGER {meta}.{trg_q} BEFORE {op} ON {meta}.{tbl_q} \
                     FOR EACH ROW \
                     SIGNAL SQLSTATE '45000' \
                     SET MESSAGE_TEXT = 'migration journal is append-only (no UPDATE/DELETE)'"
                ))
                .await?;
            }
            let _ = &tbl_lit; // documented match key; the guard uses trigger name.
        }
    }

    Ok(())
}

/// Read the **net applied state** of the journal (the MySQL analogue of
/// [`crate::apply::journal::applied`]): the LATEST event per version (by the native
/// `event_seq` order) kept only where that latest event is `applied`, UNIONed with
/// the lone `started` inflight markers for versions that are not net-applied.
///
/// MySQL 8 window functions (`ROW_NUMBER() OVER (PARTITION BY version ORDER BY
/// event_seq DESC)`) stand in for Postgres' `DISTINCT ON`; `COLLATE utf8mb4_bin`
/// gives the byte-ordered version sort Postgres gets from `COLLATE "C"`.
///
/// # Errors
/// [`JournalError::Db`] on query failure; [`JournalError::BadPhase`] /
/// [`JournalError::BadKind`] on an unrecognized stored value.
pub(crate) async fn applied<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<AppliedEntry>, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH ranked AS (
                     SELECT version, checksum, event_kind, kind AS mig_kind,
                            ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn
                       FROM {meta}.schema_migrations
                 ),
                 latest AS (
                     SELECT version, checksum, event_kind, mig_kind FROM ranked WHERE rn = 1
                 ),
                 net_applied AS (
                     SELECT version, checksum, mig_kind
                       FROM latest WHERE event_kind = '{applied}'
                 ),
                 union_all AS (
                     SELECT version, checksum, mig_kind, 'completed' AS phase
                       FROM net_applied
                     UNION ALL
                     SELECT i.version, i.checksum, NULL AS mig_kind, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM net_applied n WHERE n.version = i.version
                      )
                 )
                 SELECT version, checksum, mig_kind, phase FROM union_all
                 ORDER BY version COLLATE utf8mb4_bin",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let version: String = row.try_get("version")?;
        let checksum: String = row.try_get("checksum")?;
        let phase_s: String = row.try_get("phase")?;
        let phase = Phase::parse(&phase_s).ok_or(JournalError::BadPhase(phase_s))?;
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

/// The versions covered by a net-applied squash (the MySQL analogue of
/// [`crate::apply::journal::superseded_versions`]). Only a GENUINE recorded squash
/// (latest event `applied` AND `kind='squash'`) can supersede.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub(crate) async fn superseded_versions<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<String>, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH ranked AS (
                     SELECT version, event_kind, kind AS mig_kind,
                            ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn
                       FROM {meta}.schema_migrations
                 ),
                 net_applied_squashes AS (
                     SELECT version FROM ranked
                      WHERE rn = 1 AND event_kind = '{applied}' AND mig_kind = 'squash'
                 )
                 SELECT DISTINCT s.superseded_version AS v
                   FROM {meta}.schema_migrations_supersedes s
                   JOIN net_applied_squashes n ON n.version = s.squash_version
                  ORDER BY v",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    rows.iter()
        .map(|r| r.try_get::<_, String>("v").map_err(JournalError::from))
        .collect()
}

/// The latest `completed` checksum per **repeatable** version (the MySQL analogue
/// of [`crate::apply::journal::latest_completed_checksums`]) — the repeatable
/// re-run oracle. Only `event_kind='applied' AND kind='repeatable'` rows count.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
pub(crate) async fn latest_completed_checksums<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<std::collections::HashMap<String, String>, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH ranked AS (
                     SELECT version, checksum,
                            ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn
                       FROM {meta}.schema_migrations
                      WHERE event_kind = '{applied}' AND kind = 'repeatable'
                 )
                 SELECT version, checksum FROM ranked WHERE rn = 1",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get::<_, String>("version")?,
                r.try_get::<_, String>("checksum")?,
            ))
        })
        .collect::<Result<std::collections::HashMap<String, String>, JournalError>>()
}

/// Write (or no-op) the `started` inflight marker before a non-txn `up` runs — the
/// MySQL analogue of [`crate::apply::journal::record_started`]. `INSERT IGNORE`
/// stands in for Postgres' `ON CONFLICT (version) DO NOTHING`, so a re-arm on the
/// recovery path is idempotent. `?` placeholders throughout.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn record_started<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!(
            "INSERT IGNORE INTO {meta}.schema_migrations_inflight
                 (version, name, checksum, applied_by)
             VALUES (?, ?, ?, ?)"
        ),
        &[version.into(), name.into(), checksum.into(), applied_by.into()],
    )
    .await?;
    Ok(())
}

/// Finalize a non-txn migration (phase 2): append the immutable `completed`
/// journal row, then clear the inflight marker — the MySQL analogue of
/// [`crate::apply::journal::record_completed`]. `?` placeholders.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn record_completed<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: CompletedRecord<'_>,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!(
            "INSERT INTO {meta}.schema_migrations
                 (event_kind, version, name, checksum, `by`, exec_ms, phase, outcome, kind)
             VALUES ('{applied}', ?, ?, ?, ?, ?, 'completed', 'success', ?)",
            applied = EventKind::Applied.as_str()
        ),
        &[
            rec.version.into(),
            rec.name.into(),
            rec.checksum.into(),
            rec.applied_by.into(),
            rec.exec_ms.into(),
            rec.kind.into(),
        ],
    )
    .await?;
    conn.exec(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = ?"),
        &[rec.version.into()],
    )
    .await?;
    Ok(())
}

/// Append an immutable `rolled_back` event — the MySQL analogue of the PG rollback
/// journal INSERT (see [`crate::apply::journal::record_rolled_back`]). `?`
/// placeholders. The applied-only columns stay NULL (the CHECK enforces the
/// `rolled_back` shape).
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn record_rolled_back<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!(
            "INSERT INTO {meta}.schema_migrations
                 (event_kind, version, name, checksum, `by`, exec_ms)
             VALUES ('{rolled_back}', ?, ?, ?, ?, ?)",
            rolled_back = EventKind::RolledBack.as_str()
        ),
        &[
            version.into(),
            name.into(),
            checksum.into(),
            applied_by.into(),
            exec_ms.into(),
        ],
    )
    .await?;
    Ok(())
}

/// Clear the inflight `started` marker for a version (the MySQL analogue of
/// [`crate::apply::journal::clear_inflight`]). `?` placeholder.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn clear_inflight<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = ?"),
        &[version.into()],
    )
    .await?;
    Ok(())
}
