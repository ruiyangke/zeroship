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
//! (MySQL's monotonic surrogate) replaces Postgres' `BIGINT GENERATED ALWAYS AS
//! IDENTITY`;
//! - **timestamps** — `TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6)` replaces
//! `TIMESTAMPTZ DEFAULT now`;
//! - **keyed text columns** — `VARCHAR(255)` (MySQL cannot index a bare `TEXT`
//! without a prefix length) replaces `TEXT` for `version`/`checksum`/etc.;
//! - **immutability** — `BEFORE UPDATE`/`BEFORE DELETE` triggers that
//! `SIGNAL SQLSTATE '45000'` replace the plpgsql `RAISE EXCEPTION` trigger
//! function (MySQL has no per-statement `TRUNCATE` trigger, but `TRUNCATE`
//! requires the `DROP` privilege the least-privilege migrator role lacks, and
//! the meta database is admin-owned — defense-in-depth still holds through the
//! UPDATE/DELETE triggers + privilege model);
//! - **placeholders** — every bind is the anonymous positional `?`
//! ([`PlaceholderStyle::Question`](crate::apply::backend::PlaceholderStyle::Question)),
//! never Postgres' `$N`;
//! - **net state** — a MySQL-8 window-function (`ROW_NUMBER OVER (PARTITION BY
//! version ORDER BY event_seq DESC)`) replaces Postgres' `DISTINCT ON`, and
//! `COLLATE utf8mb4_bin` replaces `COLLATE "C"` for a byte-ordered version sort;
//! - **upsert** — `INSERT IGNORE` replaces `ON CONFLICT (version) DO NOTHING`.
//!
//! The meta schema (a MySQL *database*) is admin-owned and off the migrator's
//! reach, exactly as on Postgres — the journal is unforgeable by a confined
//! creator `up`.

use crate::apply::journal::{
    AppliedEntry, CompletedRecord, EventKind, JournalError, JournaledKind, Phase,
};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;

use super::{MysqlInflightDdlMarker, MysqlInflightResolution};

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

    // 1. Meta database (MySQL's "schema").
    conn.batch(&format!("CREATE DATABASE IF NOT EXISTS {meta}"))
        .await?;

    // 2. The append-only journal of record — the SINGLE consolidated events table.
    // `event_seq BIGINT AUTO_INCREMENT PRIMARY KEY` is the native total order
    // (MySQL assigns it on INSERT; never supplied). `version` is a VARCHAR(255)
    // (indexable; MySQL cannot key a bare TEXT), NOT unique — rollback↔re-apply
    // appends multiple rows. The applied-only columns (kind/phase/outcome) are
    // NULL on a `rolled_back` row; a CHECK documents the per-event_kind shape
    // (MySQL 8.0.16+ enforces CHECK). InnoDB for transactional DDL+journal
    // atomicity on the txn apply path.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations (
            event_seq   BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            event_kind  VARCHAR(16)  NOT NULL,
            version     VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            name        VARCHAR(255) NOT NULL,
            checksum    VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            `at`        TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            `by`        VARCHAR(255) NOT NULL,
            exec_ms     BIGINT,
            down        LONGTEXT,
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

    // Additive upgrade for journals created before reverse pinning. Historical
    // applied rows remain NULL and use the explicitly-advised compatibility
    // reconstruction path; new rows carry the exact reverse SQL.
    let down_column = conn
        .query(
            "SELECT COLUMN_NAME AS column_name
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ?
                AND TABLE_NAME = 'schema_migrations'
                AND COLUMN_NAME = 'down'",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;
    if down_column.is_empty() {
        conn.batch(&format!(
            "ALTER TABLE {meta}.schema_migrations ADD COLUMN down LONGTEXT NULL"
        ))
        .await?;
    }

    // 2a. The append-only SUPERSESSION edge log (squash). One row per
    // (squash_version → superseded_version) edge; its own AUTO_INCREMENT PK.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_supersedes (
            id                 BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            squash_version     VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            superseded_version VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            recorded_at        TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY schema_migrations_supersedes_edge_uq
                (squash_version, superseded_version)
        ) ENGINE=InnoDB"
    ))
    .await?;

    // 2b. The MUTABLE inflight side-table for two-phase non-txn markers. NOT
    // guarded by the immutability triggers; the marker is deleted on successful
    // completion or by an audited repair. (MySQL DDL is auto-committing, so the
    // non-txn two-phase path is the norm for every MySQL migration; see the
    // backend's `ddl_is_transactional`.)
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_inflight (
            version     VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL PRIMARY KEY,
            name        VARCHAR(255) NOT NULL,
            checksum    VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            started_at  TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            applied_by  VARCHAR(255) NOT NULL
        ) ENGINE=InnoDB"
    ))
    .await?;

    // The rollback-side peer of the marker above. It gets its own table rather than a
    // discriminator column on that one: the two describe opposite hazards, so they
    // need different diagnosis and different repair wording, and a shared table would
    // save one CREATE while still needing the second recovery path. Keeping the
    // apply marker's meaning single also keeps the operator docs that quote it true.
    //
    // What this buys is worth stating plainly, because it is easy to over-read: it
    // CANNOT make a MySQL rollback atomic. The `down` auto-commits statement by
    // statement, and no marker changes that. It converts a silent corruption window
    // into durable ambiguity an operator can see and repair.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_rollback_inflight (
            version     VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL PRIMARY KEY,
            name        VARCHAR(255) NOT NULL,
            checksum    VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            started_at  TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            applied_by  VARCHAR(255) NOT NULL
        ) ENGINE=InnoDB"
    ))
    .await?;

    // Immutable operator recovery audit. Clearing an ambiguous marker for a
    // verified retry and reconciling a fully-landed migration are both durable,
    // append-only decisions rather than undocumented direct table edits.
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_recovery (
            id            BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            version       VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            checksum      VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            action        VARCHAR(32) NOT NULL,
            reason        TEXT NOT NULL,
            recovered_at  TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            recovered_by  VARCHAR(255) NOT NULL,
            CONSTRAINT schema_migrations_recovery_action_chk
                CHECK (action IN ('mark_applied','clear_for_retry'))
        ) ENGINE=InnoDB"
    ))
    .await?;

    // Journals created by older releases inherited the server/database default
    // collation. A case-insensitive version or checksum changes journal identity
    // (`mig_A` and `mig_a` collapse), so bootstrap verifies every identity column
    // and upgrades only the known fixed VARCHAR definitions. Converting to
    // utf8mb4 with a binary collation preserves all text while widening equality
    // to the byte-sensitive semantics used by the in-memory model.
    ensure_binary_identity_columns(conn, cfg, &meta).await?;

    // Upgrade journals created before the edge uniqueness invariant was added.
    // Absence is repaired; a same-named but malformed index fails closed. Merely
    // finding the name is not enough because a non-unique, prefixed, reordered,
    // or partial key does not enforce one exact squash edge.
    let edge_index = conn
        .query(
            "SELECT NON_UNIQUE AS non_unique,
                    SEQ_IN_INDEX AS seq_in_index,
                    COLUMN_NAME AS column_name,
                    SUB_PART AS sub_part
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ?
                AND TABLE_NAME = 'schema_migrations_supersedes'
                AND INDEX_NAME = 'schema_migrations_supersedes_edge_uq'
              ORDER BY SEQ_IN_INDEX",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;
    if edge_index.is_empty() {
        conn.batch(&format!(
            "ALTER TABLE {meta}.schema_migrations_supersedes
                 ADD UNIQUE KEY schema_migrations_supersedes_edge_uq
                     (squash_version, superseded_version)"
        ))
        .await?;
    } else if !is_exact_supersession_edge_index(&edge_index)? {
        return Err(JournalError::Backend(
            "mysql journal: index schema_migrations_supersedes_edge_uq exists but is not the exact full-column UNIQUE (squash_version, superseded_version) key; repair the index before continuing"
                .to_string(),
        ));
    }

    // 3. Immutability triggers on BOTH append-only tables (the events table +
    // _supersedes). MySQL has no `CREATE TRIGGER IF NOT EXISTS` before 8.0.29
    // and no per-statement TRUNCATE trigger, so each BEFORE UPDATE / BEFORE
    // DELETE trigger is created only when `information_schema.triggers` shows it
    // absent, and it `SIGNAL`s SQLSTATE '45000' to abort the row mutation. (A
    // `TRUNCATE TABLE` bypasses row triggers, but it needs the DROP privilege
    // the least-privilege migrator role lacks, and the meta database is
    // admin-owned — the append-only guarantee rests on triggers + privilege
    // model, matching the PG side's defense-in-depth posture.)
    for (ord, tbl) in [
        "schema_migrations",
        "schema_migrations_supersedes",
        "schema_migrations_recovery",
    ]
    .into_iter()
    .enumerate()
    {
        let tbl_q = quote_ident_mysql(tbl)?;
        for (op, verb) in [("UPDATE", "update"), ("DELETE", "delete")] {
            // Trigger names are unique per SCHEMA in MySQL (not per table), so the
            // name embeds a per-table ordinal + the op — short, fixed, ASCII (never
            // the hyphenated-UUID meta database name, which would blow MySQL's
            // 64-char identifier limit).
            let trg = format!("{IMMUTABLE_TRG_PREFIX}_{ord}_{verb}");
            let trg_q = quote_ident_mysql(&trg)?;
            // Guard on information_schema so re-bootstrap is a no-op. The whole
            // block is a single simple-query batch (no client-side statement
            // splitting of the trigger body needed — the guard + CREATE TRIGGER are
            // two batches).
            let exists: Vec<crate::driver::Row> = conn
                .query(
                    "SELECT trigger_name FROM information_schema.triggers \
                     WHERE trigger_schema = ? AND trigger_name = ?",
                    &[cfg.pg.meta_schema.as_str().into(), trg.as_str().into()],
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
        }
    }

    Ok(())
}

/// Every journal identity column that must compare byte-for-byte.
///
/// A migration version read under a case-insensitive collation could match a version
/// it is not, so each of these carries `utf8mb4_bin` and
/// `ensure_binary_identity_columns` repairs any that does not. That repair is private
/// to this module, so it is named here rather than linked.
///
/// Shared with the test that asserts the repair rather than mirrored into it: a
/// hand-copied second list is a list that silently stops covering a table somebody
/// adds here, which is how the rollback marker's own column nearly shipped unchecked.
pub const BINARY_IDENTITY_COLUMNS: [(&str, &str); 10] = [
    ("schema_migrations", "version"),
    ("schema_migrations", "checksum"),
    ("schema_migrations_supersedes", "squash_version"),
    ("schema_migrations_supersedes", "superseded_version"),
    ("schema_migrations_inflight", "version"),
    ("schema_migrations_inflight", "checksum"),
    ("schema_migrations_rollback_inflight", "version"),
    ("schema_migrations_rollback_inflight", "checksum"),
    ("schema_migrations_recovery", "version"),
    ("schema_migrations_recovery", "checksum"),
];

async fn ensure_binary_identity_columns<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    meta: &str,
) -> Result<(), JournalError> {
    const EXPECTED: [(&str, &str); 10] = BINARY_IDENTITY_COLUMNS;
    let rows = conn
        .query(
            "SELECT TABLE_NAME AS table_name,
                    COLUMN_NAME AS column_name,
                    CHARACTER_SET_NAME AS character_set_name,
                    COLLATION_NAME AS collation_name
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ?
                AND ((TABLE_NAME = 'schema_migrations'
                      AND COLUMN_NAME IN ('version', 'checksum'))
                  OR (TABLE_NAME = 'schema_migrations_supersedes'
                      AND COLUMN_NAME IN ('squash_version', 'superseded_version'))
                  OR (TABLE_NAME = 'schema_migrations_inflight'
                      AND COLUMN_NAME IN ('version', 'checksum'))
                  OR (TABLE_NAME = 'schema_migrations_rollback_inflight'
                      AND COLUMN_NAME IN ('version', 'checksum'))
                  OR (TABLE_NAME = 'schema_migrations_recovery'
                      AND COLUMN_NAME IN ('version', 'checksum')))
              ORDER BY TABLE_NAME, ORDINAL_POSITION",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;

    let mut found = std::collections::BTreeMap::new();
    for row in rows {
        let table: String = row.try_get("table_name")?;
        let column: String = row.try_get("column_name")?;
        let charset: Option<String> = row.try_get("character_set_name")?;
        let collation: Option<String> = row.try_get("collation_name")?;
        found.insert((table, column), (charset, collation));
    }

    for (table, column) in EXPECTED {
        let key = (table.to_string(), column.to_string());
        let Some((charset, collation)) = found.get(&key) else {
            return Err(JournalError::Backend(format!(
                "mysql journal: required identity column {table}.{column} is missing after bootstrap"
            )));
        };
        if charset.as_deref() == Some("utf8mb4") && collation.as_deref() == Some("utf8mb4_bin") {
            continue;
        }
        conn.batch(&format!(
            "ALTER TABLE {meta}.{} MODIFY COLUMN {} VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL",
            quote_ident_mysql(table)?,
            quote_ident_mysql(column)?,
        ))
        .await?;
    }
    Ok(())
}

fn is_exact_supersession_edge_index(rows: &[crate::driver::Row]) -> Result<bool, JournalError> {
    if rows.len() != 2 {
        return Ok(false);
    }
    for (offset, (row, expected_column)) in rows
        .iter()
        .zip(["squash_version", "superseded_version"])
        .enumerate()
    {
        let non_unique: i64 = row.try_get("non_unique")?;
        let seq: i64 = row.try_get("seq_in_index")?;
        let column: Option<String> = row.try_get("column_name")?;
        let prefix: Option<i64> = row.try_get("sub_part")?;
        if non_unique != 0
            || seq != i64::try_from(offset + 1).unwrap_or(i64::MAX)
            || column
                .as_deref()
                .is_none_or(|actual| !actual.eq_ignore_ascii_case(expected_column))
            || prefix.is_some()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Read the **net applied state** of the journal (the MySQL analogue of
/// [`crate::apply::journal::applied`]): the LATEST event per version (by the native
/// `event_seq` order) kept only where that latest event is `applied`, UNIONed with
/// the lone `started` inflight markers for versions that are not net-applied.
///
/// MySQL 8 window functions (`ROW_NUMBER OVER (PARTITION BY version ORDER BY
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
    // An inflight marker has no sequence of its own, so it reports 0. That can
    // never be mistaken for a real one: `event_seq` is `BIGINT NOT NULL
    // AUTO_INCREMENT` and starts at 1. Callers that consult apply order drop
    // started entries first, so the sentinel is never ordered against.
    let rows = conn
        .query(
            &format!(
                "WITH ranked AS (
                     SELECT version, checksum, down, event_kind, kind AS mig_kind, event_seq,
                            ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn
                       FROM {meta}.schema_migrations
                 ),
                 latest AS (
                     SELECT version, checksum, down, event_kind, mig_kind, event_seq FROM ranked WHERE rn = 1
                 ),
                 net_applied AS (
                     SELECT version, checksum, down, mig_kind, event_seq
                       FROM latest WHERE event_kind = '{applied}'
                 ),
                 union_all AS (
                     SELECT version, checksum, down, mig_kind, event_seq, 'completed' AS phase
                       FROM net_applied
                     UNION ALL
                     SELECT i.version, i.checksum, NULL AS down, NULL AS mig_kind, 0 AS event_seq, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM net_applied n WHERE n.version = i.version
                      )
                 )
                 SELECT version, checksum, down, mig_kind, event_seq, phase FROM union_all
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
        let event_seq: i64 = row.try_get("event_seq")?;
        out.push(AppliedEntry {
            version,
            checksum,
            down: row.try_get("down")?,
            phase,
            kind,
            event_seq,
        });
    }
    Ok(out)
}

/// Return versions whose latest immutable event is `rolled_back`.
pub(crate) async fn net_rolled_back_versions<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<String>, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH ranked AS (
                     SELECT version, event_kind,
                            ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn
                       FROM {meta}.schema_migrations
                 )
                 SELECT version
                   FROM ranked
                  WHERE rn = 1 AND event_kind = '{rolled_back}'
                  ORDER BY version COLLATE utf8mb4_bin",
                rolled_back = EventKind::RolledBack.as_str()
            ),
            &[],
        )
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("version").map_err(JournalError::from))
        .collect()
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

/// Write the `started` inflight marker before a non-txn `up` runs. The caller
/// holds the project lock and has already proved that no marker exists, so this
/// is a plain, exact one-row INSERT. Duplicate keys and unexpected affected-row
/// counts fail closed instead of being hidden by `INSERT IGNORE`.
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
    for (field, value) in [("name", name), ("applied_by", applied_by)] {
        let length = value.chars().count();
        if length > 255 {
            return Err(JournalError::Backend(format!(
                "mysql inflight marker {field} is {length} characters; the maximum is 255"
            )));
        }
    }
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let affected = conn
        .exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations_inflight
                 (version, name, checksum, applied_by)
             VALUES (?, ?, ?, ?)"
            ),
            &[
                version.into(),
                name.into(),
                checksum.into(),
                applied_by.into(),
            ],
        )
        .await?;
    if affected != 1 {
        return Err(JournalError::Backend(format!(
            "mysql inflight marker insert affected {affected} rows; expected exactly 1"
        )));
    }
    Ok(())
}

/// Append the immutable `completed` journal row. The caller must hold the InnoDB
/// transaction that also records any squash edges and clears the inflight marker.
/// `?` placeholders.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn append_completed<D: SqlSession>(
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
    Ok(())
}

/// Append a completed row and clear its inflight marker inside the caller's open
/// transaction. Backfill finalization uses this because its progress update,
/// journal event, and marker cleanup must commit as one unit.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn record_completed_in_transaction<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: CompletedRecord<'_>,
) -> Result<(), JournalError> {
    let version = rec.version;
    append_completed(conn, cfg, rec).await?;
    clear_inflight(conn, cfg, version).await?;
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

/// Arm the rollback marker before a `down` runs, so an interrupted unwind leaves
/// evidence instead of a silently half-reverted schema.
///
/// Written on its own and committed before the first byte of `down` reaches MySQL.
/// It cannot join the `down` in a transaction: MySQL DDL auto-commits, so the marker
/// has to be durable BEFORE the statements it describes, not alongside them.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn mark_rollback_inflight<D: SqlSession>(
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
            "INSERT INTO {meta}.schema_migrations_rollback_inflight
             (version, name, checksum, applied_by)
             VALUES (?, ?, ?, ?)"
        ),
        &[
            version.into(),
            name.into(),
            checksum.into(),
            applied_by.into(),
        ],
    )
    .await?;
    Ok(())
}

/// Clear the rollback marker. Called inside the caller's transaction, paired with
/// the `rolled_back` append so the two commit as one unit.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn clear_rollback_inflight<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!("DELETE FROM {meta}.schema_migrations_rollback_inflight WHERE version = ?"),
        &[version.into()],
    )
    .await?;
    Ok(())
}

/// Whether a version carries an unmatched rollback marker: a `down` that started and
/// whose outcome was never recorded.
///
/// # Errors
/// [`JournalError::Db`] on failure.
pub(crate) async fn has_rollback_inflight<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<bool, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT version FROM {meta}.schema_migrations_rollback_inflight
                  WHERE version = ? LIMIT 1"
            ),
            &[version.into()],
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Lock and read one recovery marker inside the caller's transaction. The row is
/// decoded into a public, driver-neutral value so operator tooling never needs
/// to query the journal tables directly.
pub(crate) async fn inflight_for_update<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<Option<MysqlInflightDdlMarker>, JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT version, name, checksum, applied_by,
                        CAST(started_at AS CHAR) AS started_at
                   FROM {meta}.schema_migrations_inflight
                  WHERE version = ?
                  FOR UPDATE"
            ),
            &[version.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(MysqlInflightDdlMarker {
        version: row.try_get("version")?,
        name: row.try_get("name")?,
        checksum: row.try_get("checksum")?,
        applied_by: row.try_get("applied_by")?,
        started_at: row.try_get("started_at")?,
    }))
}

/// Append the immutable operator decision that resolves one ambiguous DDL
/// marker. The caller commits this row atomically with either completion or
/// marker clearance.
pub(crate) async fn append_recovery_audit<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    marker: &MysqlInflightDdlMarker,
    resolution: MysqlInflightResolution,
    recovered_by: &str,
    reason: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!(
            "INSERT INTO {meta}.schema_migrations_recovery
                 (version, checksum, action, reason, recovered_by)
             VALUES (?, ?, ?, ?, ?)"
        ),
        &[
            marker.version.as_str().into(),
            marker.checksum.as_str().into(),
            resolution.as_str().into(),
            reason.into(),
            recovered_by.into(),
        ],
    )
    .await?;
    Ok(())
}
