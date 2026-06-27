//! SQLite schema-dump serialization for the `zeroship-migrate dump` command
//! (engine-agnostic `dump` parity with the Postgres `pg_dump --schema-only` leg).
//!
//! Derives the schema DDL from the LIVE `main` database the same dependency-light
//! way the drift introspector reads it: a single `sqlite_master` read under engine
//! mode, emitting the stored `CREATE TABLE/INDEX/VIEW/TRIGGER` text in a
//! DETERMINISTIC order (tables before their indexes/triggers; stable name ordering
//! within each kind) so the dump is reproducible. We do NOT shell out to `sqlite3`
//! (it may be absent, and the DB is opened through our own hardened actor) — the
//! DDL comes verbatim from `sqlite_master.sql`.
//!
//! # What is excluded (no journal / internal leakage, §2.5.2)
//!
//! - The `_mig` journal lives in a SEPARATE attached database, so a `main`-scoped
//!   `sqlite_master` read never sees `schema_migrations` / its triggers.
//! - SQLite internal objects (`sqlite_sequence`, `sqlite_autoindex_*`,
//!   `sqlite_stat*`, …) are filtered Rust-side (the hardened authorizer's function
//!   allowlist has no `LIKE`, so we cannot `WHERE name NOT LIKE 'sqlite_%'`; we
//!   match the prefix in Rust, exactly like the drift introspector).
//! - Rows with a NULL `sql` (the implicit rowid index of an `INTEGER PRIMARY KEY`,
//!   internal auto-indexes) carry no DDL and are skipped.

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

/// True iff `name` is a SQLite-internal object excluded from a schema dump (any
/// `sqlite_`-prefixed name: `sqlite_sequence`, `sqlite_autoindex_*`, `sqlite_stat*`).
fn is_internal(name: &str) -> bool {
    name.starts_with("sqlite_")
}

/// A deterministic sort key for one `sqlite_master` row so the dump is reproducible:
/// tables/views FIRST (so a table is emitted before the indexes/triggers that
/// reference it), then indexes, then triggers; ties broken by name.
///
/// `table` and `view` share rank 0 (both are "container" objects with no
/// dependency on an index/trigger); `index` is rank 1; `trigger` is rank 2;
/// anything else sorts last (rank 3). The secondary key is the object name so the
/// order is stable across reads regardless of `sqlite_master` scan order.
fn kind_rank(obj_type: &str) -> u8 {
    match obj_type {
        "table" | "view" => 0,
        "index" => 1,
        "trigger" => 2,
        _ => 3,
    }
}

/// Serialize the LIVE `main` schema as a deterministic sequence of CREATE
/// statements, each terminated with `;`, ready to write into the `schema.sql`
/// dump (before the applied-versions trailer the bin appends).
///
/// Read-only; runs under engine mode (the `sqlite_master` read requires it on the
/// hardened connection, §2.5.1). Tables/views are emitted before indexes/triggers,
/// each kind name-ordered, so re-running `dump` on an unchanged schema is
/// byte-identical. The `_mig` journal + `sqlite_*` internals never appear (see the
/// module docs).
///
/// # Errors
/// [`SqliteActorError`] on a `sqlite_master` read failure.
pub(crate) async fn dump_schema(actor: &MigrationActor) -> Result<String, SqliteActorError> {
    // Least privilege: the dump is a single plain `SELECT … FROM main.sqlite_master`
    // — an `AuthAction::Read` on `main`, which the hardened authorizer already allows
    // under the most-confined `CreatorUp` (the `_ => Allow` catch-all; the `_mig`
    // deny arms never fire for a `main`-scoped read). Unlike the drift introspector
    // (which ALSO issues `PRAGMA table_info/index_list/…`, and so MUST run under
    // `EngineJournal` where those PRAGMAs are allowlisted), the dump touches no
    // PRAGMA and no `_mig`, so `EngineJournal` would be strictly broader than needed.
    // Run under `CreatorUp`. Read-only — no DDL.
    actor.set_mode(Mode::CreatorUp).await?;

    // `type, name, sql` for every object with stored DDL on `main`. The
    // `sql IS NOT NULL` filter drops the implicit indexes (rowid PK auto-index)
    // that carry no DDL; the `sqlite_%` exclusion is done Rust-side (no `LIKE` in
    // the hardened authorizer's function allowlist). `main.` scopes the read to
    // the app file — the `_mig` journal (a separate attached DB) is never seen.
    let rows = actor
        .query(
            "SELECT type, name, sql FROM main.sqlite_master \
             WHERE sql IS NOT NULL \
             ORDER BY name",
        )
        .await?;

    // Gather (kind_rank, name, ddl) so we can apply the deterministic ordering
    // without relying on the catalog scan order.
    let mut objects: Vec<(u8, String, String)> = Vec::new();
    for r in &rows {
        let obj_type = cell(r, 0);
        let name = cell(r, 1);
        let sql = cell(r, 2);
        if is_internal(&name) {
            continue;
        }
        // An empty/whitespace `sql` carries no DDL — skip rather than emit a stray
        // `;` (defensive; `sql IS NOT NULL` already excludes the NULL rows).
        let ddl = sql.trim();
        if ddl.is_empty() {
            continue;
        }
        objects.push((kind_rank(&obj_type), name, ddl.to_string()));
    }
    // Deterministic order: tables/views, then indexes, then triggers; name within.
    objects.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut out = String::new();
    for (_, _, ddl) in objects {
        out.push_str(&ddl);
        // `sqlite_master.sql` stores the CREATE without the trailing `;` — add it so
        // the dump is a runnable script (mirrors `pg_dump`'s statement terminators).
        out.push_str(";\n");
    }
    Ok(out)
}

/// Extract a text cell, treating NULL / missing as empty (the dump is best-effort
/// over engine-stored text; a missing cell simply contributes no DDL).
fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).and_then(Clone::clone).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_rank_orders_tables_before_indexes_before_triggers() {
        assert!(kind_rank("table") < kind_rank("index"));
        assert!(kind_rank("view") < kind_rank("index"));
        assert!(kind_rank("index") < kind_rank("trigger"));
        // Tables and views share the "container" rank.
        assert_eq!(kind_rank("table"), kind_rank("view"));
        // An unknown kind sorts last.
        assert!(kind_rank("trigger") < kind_rank("mystery"));
    }

    #[test]
    fn internal_objects_are_excluded() {
        assert!(is_internal("sqlite_sequence"));
        assert!(is_internal("sqlite_autoindex_widgets_1"));
        assert!(is_internal("sqlite_stat1"));
        assert!(!is_internal("widgets"));
        assert!(!is_internal("idx_widgets_name"));
    }

    /// Least-privilege regression (fix #4): the `dump` read must succeed under the
    /// confined `CreatorUp` authorizer mode — a plain `SELECT … FROM
    /// main.sqlite_master` is an `AuthAction::Read` on `main`, which CreatorUp
    /// already allows (the `_ => Allow` catch-all). `dump_schema` itself sets
    /// `CreatorUp`; this test additionally proves the read is NOT dependent on the
    /// broader `EngineJournal` mode by leaving the actor in CreatorUp throughout and
    /// asserting the table DDL still comes back. (Pre-fix, `dump_schema` flipped the
    /// connection to EngineJournal; this exercises the tightened path.)
    #[test]
    fn dump_read_succeeds_under_creator_up() {
        use crate::apply::backend::sqlite::Mode;
        let dir = std::env::temp_dir().join(format!(
            "zsmig_dump_creatorup_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let app = dir.join("app.sqlite");
        let journal = dir.join("app.sqlite.migrations");

        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async move {
                let actor = MigrationActor::open(&app, &journal).expect("open actor");
                // Create a creator table the way the creator `up` would: under CreatorUp.
                actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
                actor
                    .exec("CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT)")
                    .await
                    .expect("create table under CreatorUp");
                // dump_schema sets CreatorUp itself and reads sqlite_master. It must
                // succeed and surface the table DDL without ever needing EngineJournal.
                let ddl = dump_schema(&actor).await.expect("dump under CreatorUp");
                assert!(
                    ddl.contains("CREATE TABLE widgets"),
                    "dump must include the creator table DDL: {ddl}"
                );
            });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
