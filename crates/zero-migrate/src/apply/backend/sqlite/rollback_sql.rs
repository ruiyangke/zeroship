//! Additive transactional rollback on SQLite.
//!
//! Reverses ONE applied migration via its `down` SQL inside a single
//! `BEGIN IMMEDIATE` transaction, then appends an immutable `rolled_back` event to
//! the consolidated `schema_migrations` table (event_seq is native AUTOINCREMENT)
//! — the SAME atomic single-connection model as the apply path:
//!
//! ```text
//! BEGIN IMMEDIATE (EngineJournal — the engine owns txn boundaries)
//! CreatorUp: run the creator `down` (confined from `_mig`)
//! EngineJournal: INSERT schema_migrations (event_kind='rolled_back')
//! COMMIT (down + rolled_back event commit atomically)
//! ```
//!
//! # Additive-only this phase
//!
//! SQLite ≥ 3.35 reverses these NATIVELY, no table rebuild:
//! - `DROP TABLE` (reverses a `CREATE TABLE`)
//! - `DROP COLUMN` (reverses an `ADD COLUMN`; SQLite ≥ 3.35)
//! - `DROP INDEX` (reverses a `CREATE INDEX`)
//! - `ALTER TABLE … RENAME` (reverses a rename)
//!
//! Any `down` that would REQUIRE the 12-step rebuild — a column TYPE-change
//! reversal, a constraint add/drop, a `CHECK`/`DEFAULT`/nullability flip — is
//! REFUSED up-front with [`RollbackError::SqliteRebuildRequired`] (the rebuild path
//! is not yet built). We do NOT half-implement a rebuild here. The classifier
//! ([`down_needs_rebuild`]) is a lightweight SQLite-aware scan: the libpg_query
//! parser the PG path uses (`crate::classify`) is a POSTGRES parser and would
//! mis-parse SQLite DDL, so it cannot be reused here.
//!
//! # Confinement (invariants unchanged)
//!
//! The creator `down` runs under `CreatorUp` — a malicious `down` attempting a
//! `_mig` write / ATTACH / PRAGMA is still denied by the authorizer at prepare
//! time. The `rolled_back` journal write runs under `EngineJournal` (the only
//! mode that permits a `_mig` write). The mode flip lands between separate
//! prepares, never inside one batch.

use std::time::Instant;

use crate::apply::executor::RollbackError;
use crate::model::migration::Migration;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;
use super::journal_sql;

/// Map a SQLite actor error onto the dialect-neutral `Backend` arm of [`RollbackError`].
fn rb_err(e: SqliteActorError) -> RollbackError {
    RollbackError::Backend(e.to_string())
}

/// Roll back ONE migration's `down` transactionally + journal a `rolled_back`
/// event. Mirrors the PG
/// [`rollback_one_transactional`](crate::apply::backend::postgres::session::rollback_one_transactional)
/// semantics, dialect-translated.
///
/// Preconditions (the caller, the generic executor, has already established): the
/// version is currently net-applied, its `down` is `Some`, and approval/guard gates
/// have cleared. This method does the dialect-coupled work only.
///
/// # Errors
/// - [`RollbackError::SqliteRebuildRequired`] if the `down` needs the 12-step
/// rebuild — refused before any statement runs; nothing changes.
/// - [`RollbackError::DownFailedSqlite`]-shaped `Backend` error if the `down` SQL
/// fails or is denied by the authorizer (the txn is rolled back).
/// - [`RollbackError::Backend`] on a journal-write / commit failure.
pub(crate) async fn rollback_one_transactional(
    actor: &MigrationActor,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RollbackError> {
    let down = m.down.as_deref().ok_or_else(|| {
        RollbackError::Backend(format!(
            "sqlite rollback called for {} but its `down` is None (caller must gate this)",
            m.version.as_str()
        ))
    })?;

    // GATE: refuse a rebuild-needing `down` UP FRONT (before BEGIN), so nothing
    // is half-done. This is the additive-only boundary; the rebuild is not yet built.
    if let Some(reason) = down_needs_rebuild(down) {
        return Err(RollbackError::SqliteRebuildRequired {
            version: m.version.as_str().to_string(),
            reason,
        });
    }

    // Ensure the journal exists (idempotent, engine mode) before we open the txn.
    journal_sql::ensure_journal(actor).await.map_err(rb_err)?;

    run_rollback_txn(actor, m, down, applied_by).await
}

/// The transactional body, factored so a failure path can ROLLBACK + report a wedge
/// (the same autocommit-probe discipline the apply path uses).
async fn run_rollback_txn(
    actor: &MigrationActor,
    m: &Migration,
    down: &str,
    applied_by: &str,
) -> Result<(), RollbackError> {
    let started = Instant::now();

    // 1. BEGIN IMMEDIATE under engine mode (the engine owns txn boundaries; the
    // creator `down` in CreatorUp can never open/close a transaction).
    actor.set_mode(Mode::EngineJournal).await.map_err(rb_err)?;
    actor.exec("BEGIN IMMEDIATE").await.map_err(rb_err)?;

    let result = async {
        // 2. CreatorUp — the creator `down` is confined from `_mig`. A malicious
        // `down` attempting a `_mig` write / ATTACH / PRAGMA is denied at prepare.
        actor.set_mode(Mode::CreatorUp).await?;
        actor.exec(down).await?;

        // 3. EngineJournal — INSERT the immutable rolled_back event into the
        // consolidated table (event_seq is AUTOINCREMENT, not supplied). SEPARATE
        // prepares from the creator `down`, mode flip strictly between. A
        // rolled_back row leaves kind/phase/outcome NULL (the event-shape CHECK).
        actor.set_mode(Mode::EngineJournal).await?;
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let version = journal_sql::sql_lit(m.version.as_str());
        let name = journal_sql::sql_lit(&m.name);
        let checksum = journal_sql::sql_lit(m.checksum.as_str());
        let by = journal_sql::sql_lit(applied_by);
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_kind, version, name, checksum, \"by\", exec_ms) \
                 VALUES ('{rolled_back}', {version}, {name}, {checksum}, {by}, {exec_ms})",
                rolled_back = crate::apply::journal::EventKind::RolledBack.as_str()
            ))
            .await?;
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            actor
                .commit_or_cleanup("migration rollback")
                .await
                .map_err(rb_err)?;
            Ok(())
        }
        Err(e) => {
            // Roll back so a denied/failed `down` never leaves a partial journal.
            // Same discipline as apply: the AUTOCOMMIT state — not the ROLLBACK
            // result — is the wedge signal.
            actor.set_mode(Mode::EngineJournal).await.map_err(rb_err)?;
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(RollbackError::Backend(format!(
                    "sqlite rollback of {} failed: {e}",
                    m.version.as_str()
                ))),
                Ok(false) => Err(RollbackError::Backend(format!(
                    "sqlite migration connection poisoned: transaction still open after ROLLBACK \
                     (rollback result: {rb:?}); original down error: {e}"
                ))),
                Err(probe) => Err(RollbackError::Backend(format!(
                    "sqlite migration connection poisoned: could not confirm autocommit after \
                     ROLLBACK: {probe}; original down error: {e}"
                ))),
            }
        }
    }
}

/// Lightweight SQLite-aware detector: does this `down` contain a statement that
/// would require the 12-step table REBUILD, rather than an operation SQLite
/// ≥ 3.35 performs natively?
///
/// Returns `Some(reason)` for a rebuild-needing `down`, `None` if every statement
/// is additively-reversible. **Fail-closed:** an unrecognised `ALTER TABLE`
/// subcommand is treated as rebuild-needing (we never silently let an
/// unimplemented reversal through as if additive).
///
/// This is a token scan, NOT the libpg_query parser (which is Postgres and
/// mis-parses SQLite DDL). It is conservative: it recognises the additive verbs
/// (`DROP TABLE`/`DROP COLUMN`/`DROP INDEX`/`RENAME`) and the explicitly
/// rebuild-needing `ALTER … TYPE`; anything else `ALTER`-shaped is refused.
#[must_use]
pub(crate) fn down_needs_rebuild(down: &str) -> Option<String> {
    for stmt in split_statements(down) {
        // `norm` is space-wrapped (leading + trailing space) so token matches like
        // `.contains(" ADD ")` catch a token at either end; `trimmed` is for the
        // statement-kind `starts_with`.
        let norm = normalize_ws(&stmt).to_ascii_uppercase();
        let trimmed = norm.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Non-ALTER statements that are additively reversible / harmless:
        // DROP TABLE / DROP INDEX / DROP VIEW / DROP TRIGGER, CREATE *, DML, etc.
        // None of these need a rebuild.
        if !trimmed.starts_with("ALTER TABLE") {
            continue;
        }
        // ALTER TABLE subcommands. SQLite supports exactly four:
        // RENAME TO, RENAME [COLUMN] x TO y, ADD [COLUMN] …, DROP [COLUMN] …
        // All four are additive/native. ANYTHING else expressed as ALTER TABLE — and
        // in particular a type/constraint change (which SQLite has NO native ALTER
        // for, so it can only be a 12-step rebuild dressed up) — needs the rebuild.
        let is_rename = norm.contains(" RENAME ");
        let is_add = norm.contains(" ADD ");
        let is_drop = norm.contains(" DROP ");
        if is_rename || is_add || is_drop {
            // Native — additively reversible. (An `ADD COLUMN` here would be part of
            // a `down` that re-adds a column; its reversal is native too.)
            continue;
        }
        // An ALTER TABLE that is neither RENAME/ADD/DROP — e.g. a smuggled
        // `ALTER TABLE t ALTER COLUMN c TYPE …` (not even valid SQLite, only
        // expressible via rebuild) — is refused as rebuild-needing.
        return Some(format!(
            "ALTER TABLE without RENAME/ADD/DROP (type or constraint change): `{}`",
            stmt.trim()
        ));
    }
    None
}

/// Split a `down` script into statements on top-level `;` (ignoring `;` inside
/// single/double-quoted strings, identifiers, and `/* … */` block comments). A
/// simple but correct-enough splitter for the rebuild classifier.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_block_comment = false;
    let mut in_line_comment = false;
    while let Some(ch) = chars.next() {
        if in_line_comment {
            cur.push(ch);
            if ch == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            cur.push(ch);
            if ch == '*' && chars.peek() == Some(&'/') {
                cur.push(chars.next().unwrap());
                in_block_comment = false;
            }
            continue;
        }
        if in_single {
            cur.push(ch);
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            cur.push(ch);
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                cur.push(ch);
            }
            '"' => {
                in_double = true;
                cur.push(ch);
            }
            '-' if chars.peek() == Some(&'-') => {
                in_line_comment = true;
                cur.push(ch);
            }
            '/' if chars.peek() == Some(&'*') => {
                in_block_comment = true;
                cur.push(ch);
                cur.push(chars.next().unwrap());
            }
            ';' => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Collapse runs of whitespace (incl. newlines) to single spaces, and strip block
/// comments, so keyword matching is robust to formatting.
fn normalize_ws(s: &str) -> String {
    // Drop block comments first (their contents must not be matched as keywords).
    let mut without_comments = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // skip to */
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            without_comments.push(' ');
            continue;
        }
        without_comments.push(bytes[i] as char);
        i += 1;
    }
    // Wrap the matched keywords with leading/trailing spaces so `.contains(" ADD ")`
    // matches a token at the start too.
    let collapsed: String = without_comments
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(" {collapsed} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_downs_do_not_need_rebuild() {
        assert_eq!(down_needs_rebuild("DROP TABLE users;"), None);
        assert_eq!(down_needs_rebuild("DROP TABLE \"users\";"), None);
        assert_eq!(down_needs_rebuild("DROP INDEX ix_users_email;"), None);
        assert_eq!(
            down_needs_rebuild("ALTER TABLE users DROP COLUMN nickname;"),
            None
        );
        assert_eq!(
            down_needs_rebuild("ALTER TABLE users RENAME TO people;"),
            None
        );
        assert_eq!(
            down_needs_rebuild("ALTER TABLE users RENAME COLUMN a TO b;"),
            None
        );
        assert_eq!(
            down_needs_rebuild("ALTER TABLE users ADD COLUMN re_added TEXT;"),
            None
        );
        // Multi-statement additive down.
        assert_eq!(
            down_needs_rebuild("DROP INDEX ix; ALTER TABLE t DROP COLUMN c;"),
            None
        );
    }

    #[test]
    fn type_change_down_needs_rebuild() {
        // A type-change reversal is not native SQLite; refuse it.
        let r = down_needs_rebuild("ALTER TABLE users ALTER COLUMN age TYPE INTEGER;");
        assert!(r.is_some(), "type change must need a rebuild");
        assert!(r.unwrap().contains("type or constraint change"));
    }

    #[test]
    fn semicolon_inside_string_is_not_a_split() {
        // A `;` inside a string literal must not split the statement.
        let downs = split_statements("INSERT INTO t VALUES ('a;b'); DROP TABLE t;");
        assert_eq!(downs.len(), 2);
    }

    #[test]
    fn rename_keyword_at_token_boundary() {
        // Ensure the leading/trailing space wrap makes RENAME match even formatted oddly.
        assert_eq!(
            down_needs_rebuild("ALTER  TABLE   users    RENAME   TO people ;"),
            None
        );
    }
}
