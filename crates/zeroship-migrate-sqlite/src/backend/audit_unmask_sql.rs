//! `<app>.__zeroship_audit_unmask` - the per-app unmask audit table, SQLite half.
//!
//! # Why this is in the migration backend and not in the data plane
//!
//! Schema change belongs to `zeroship-migrate`; the data plane emits no DDL.
//! Issuing this table's `CREATE TABLE` / `CREATE INDEX` from inside the worker on
//! every `unmask()` would put DDL on the privileged read path, executed by the
//! process that runs creator code.
//!
//! The table could not simply be deleted. The audit row is where authorization
//! and provenance for a plaintext read live, so removing the writer's dependency
//! without giving the table a creator would keep the writer and drop the
//! guarantee. This module IS that creator, for SQLite.
//!
//! # Why the shape is NOT shared with the Postgres half
//!
//! It is not shareable, and the journal tables already establish that. The two
//! dialects' journals are two separate definitions in two separate authority
//! crates ([`crate::backend::journal_sql`] and
//! `zeroship-migrate-postgres`'s `backend/journal_sql.rs`) with deliberately
//! different shapes - `BIGINT GENERATED ALWAYS AS IDENTITY` against `INTEGER
//! PRIMARY KEY AUTOINCREMENT`, `TIMESTAMPTZ DEFAULT now()` against `TEXT DEFAULT
//! CURRENT_TIMESTAMP`. The audit table divides on exactly the same lines, so it
//! follows exactly the same split: each dialect's authority owns its own DDL.
//!
//! The Postgres half is `zeroship_migrate_server::provisioning`.
//!
//! # This crate provides the capability; the HOST decides to use it
//!
//! Nothing here runs on its own. [`ensure_audit_unmask_table`] is called by the
//! dev-tier apply host (`zeroship-migrate-node`'s `applyIr` under an in-process
//! driver), the same way
//! [`crate::backend::journal_sql::ensure_journal`] is called by the engine rather
//! than firing from inside the backend. That keeps the policy decision - "a
//! zeroship app schema carries this platform table" - in a zeroship host, and
//! leaves this vendor crate holding only the SQL.
//!
//! # `main` and unfenced - and why the schema is a PARAMETER
//!
//! This host opens the tenant's `zs-<app_id>.sqlite` as `main`, so it passes
//! `"main"`. The worker opens a DIFFERENT file as `main` (the session database)
//! and ATTACHes the app file under the `<app_id>` alias, so the same physical
//! table is `"<app_id>"."__zeroship_audit_unmask"` from there. One table, two
//! spellings, decided entirely by who opened what.
//!
//! [`audit_unmask_ddl`] therefore takes the qualifier rather than baking one in.
//! That is what lets `zeroship-data-v8`'s SQLite fixtures - which reach the
//! file through the worker's backend, under the alias - execute the SAME
//! generator the apply host does. A fixture with its own hardcoded copy of this
//! DDL would stay green while production drifted away from it.
//!
//! What must NOT happen either way is the table landing behind the journal's fence. The journal
//! file is attached only by the migration actor; the worker never attaches it
//! and could not reach a table put there, so the writer would be unable to see
//! its own audit log.
//!
//! # Mode
//!
//! It runs under [`Mode::CreatorUp`], not `EngineJournal`. The table is an
//! ordinary table in the tenant's own file, reached by ordinary parameterised
//! SQL from the worker - exactly the privilege a creator migration's own
//! `CREATE TABLE` has. `EngineJournal` is for the journal and would be a strictly
//! larger grant for no reason.

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

/// The unqualified table name, shared with the data plane's INSERT.
///
/// Migration authoring reserves `__zeroship` so creator DDL cannot collide with
/// this provisioned relation. That authoring rule does not hide the table from
/// a trusted runtime descriptor. Name parity is covered by the data-v8 audit
/// table test.
pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

/// Double any embedded quote so `schema` cannot leave its identifier.
///
/// Production callers pass `main` or an app id, but this generator is `pub` and
/// must not depend on its callers being careful.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Every statement that establishes `<schema>.__zeroship_audit_unmask` on
/// SQLite, in apply order. All are `IF NOT EXISTS`, so the sequence is
/// idempotent and safe to re-run on every apply.
///
/// `schema` is the qualifier the CALLER's connection reaches the tenant's app
/// file by: `main` for the apply host, which opened that file directly, and the
/// `<app_id>` ATTACH alias for the worker's backend. See the module doc.
///
/// Dialect notes against the Postgres half:
/// * `BIGSERIAL PRIMARY KEY` -> `INTEGER PRIMARY KEY` (the ROWID alias), so
///   there is no sequence object and no grant to go with it.
/// * `TIMESTAMPTZ NOT NULL DEFAULT NOW()` -> `TEXT NOT NULL DEFAULT
///   CURRENT_TIMESTAMP` (ISO 8601).
/// * SQLite puts the schema qualifier before the INDEX NAME and NEVER before
///   the table it indexes - `CREATE INDEX <schema>.<idx> ON <table>`. The
///   Postgres spelling (`ON <schema>.<table>`) is a parse error here, so the
///   two arms genuinely cannot share one string.
#[must_use]
pub fn audit_unmask_ddl(schema: &str) -> Vec<String> {
    let q = quote_ident(schema);
    let t = quote_ident(AUDIT_UNMASK_TABLE);
    vec![
        // `outcome` is CHECK-constrained rather than free text so a malformed
        // audit write refuses at the engine instead of landing an unreadable
        // row in the record of who read plaintext.
        format!(
            r#"CREATE TABLE IF NOT EXISTS {q}.{t} (
                id              INTEGER PRIMARY KEY,
                ts              TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                actor_id        TEXT,
                actor_role      TEXT,
                -- The refused DB-3 claim, verbatim. Kept in step with the
                -- PostgreSQL twin in `zeroship-migrate-server`'s
                -- `audit_unmask_table_sql`: two dialects diverging on an audit
                -- schema is its own defect. See that file for why the column
                -- exists at all.
                claimed_actor   TEXT,
                collection      TEXT NOT NULL,
                row_pk          TEXT NOT NULL,
                "column"        TEXT NOT NULL,
                classification  TEXT NOT NULL,
                reason          TEXT,
                request_id      TEXT,
                outcome         TEXT NOT NULL CHECK (outcome IN ('granted', 'denied'))
            )"#
        ),
        // The index NAMES are qualified so two apps attached into one session
        // cannot collide on them; the ON clause names the bare table, which is
        // the only shape SQLite accepts.
        format!(r#"CREATE INDEX IF NOT EXISTS {q}."{AUDIT_UNMASK_TABLE}_ts_idx" ON {t} (ts)"#),
        format!(
            r#"CREATE INDEX IF NOT EXISTS {q}."{AUDIT_UNMASK_TABLE}_actor_idx" ON {t} (actor_id, ts)"#
        ),
        format!(
            r#"CREATE INDEX IF NOT EXISTS {q}."{AUDIT_UNMASK_TABLE}_row_idx" ON {t} (row_pk, "column", ts)"#
        ),
    ]
}

/// Establish `main.__zeroship_audit_unmask` and its three indexes, idempotently.
///
/// # Errors
/// [`SqliteActorError`] if the mode flip or any statement fails.
pub async fn ensure_audit_unmask_table(actor: &MigrationActor) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::CreatorUp).await?;
    for stmt in audit_unmask_ddl("main") {
        actor.exec(&stmt).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data plane's INSERT names this table. If the constant and the DDL
    /// ever disagree, the worker writes into a table nothing created and every
    /// unmask fails at runtime rather than here.
    #[test]
    fn ddl_creates_the_table_the_constant_names() {
        let ddl = audit_unmask_ddl("main");
        assert!(
            ddl[0].contains(&format!(r#""main"."{AUDIT_UNMASK_TABLE}""#)),
            "the CREATE TABLE must name {AUDIT_UNMASK_TABLE}; got: {}",
            ddl[0]
        );
        assert_eq!(ddl.len(), 4, "one table + three indexes");
    }

    /// Every statement must be re-runnable: the host calls this on EVERY apply,
    /// and an apply that fails on the second run would brick redeploys.
    #[test]
    fn every_statement_is_idempotent() {
        for stmt in audit_unmask_ddl("main") {
            assert!(
                stmt.contains("IF NOT EXISTS"),
                "statement is not re-runnable: {stmt}"
            );
        }
    }

    /// The qualifier reaches the index NAME and never the indexed table. The
    /// Postgres spelling (`ON <schema>.<table>`) is a parse error on SQLite, so
    /// getting this backwards fails at apply, on a path a unit test can pin
    /// without a database.
    #[test]
    fn index_statements_qualify_the_name_not_the_table() {
        let ddl = audit_unmask_ddl("app7");
        for stmt in &ddl[1..] {
            assert!(
                stmt.contains(r#"IF NOT EXISTS "app7"."#),
                "the index NAME must carry the qualifier: {stmt}"
            );
            assert!(
                !stmt.contains(r#"ON "app7""#),
                "the indexed TABLE must be bare - SQLite rejects a qualified ON: {stmt}"
            );
        }
    }

    /// The qualifier is an identifier, not a splice point. Production passes
    /// `main` or an app id, but this generator is `pub`.
    #[test]
    fn schema_is_quoted_not_interpolated_raw() {
        let ddl = audit_unmask_ddl(r#"a"; DROP TABLE t; --"#);
        assert!(
            ddl[0].contains(r#""a""; DROP TABLE t; --""#),
            "the qualifier must survive as ONE doubled-quote identifier: {}",
            ddl[0]
        );
    }

    /// The audit table belongs in the app file, never in the journal file. The
    /// worker attaches the app file, so runtime ORM access requires the
    /// relation to live there.
    #[test]
    fn statements_never_target_the_journal_file() {
        for stmt in audit_unmask_ddl("main") {
            assert!(
                !stmt.contains("_mig"),
                "audit table must live in the app file, not the journal: {stmt}"
            );
        }
    }
}
