//! Shared SQLite FTS5 DDL shapes — the single source of truth for the
//! external-content FTS5 virtual table + AFTER-trigger mirror lifecycle.
//!
//! Two consumers, one shape (the "byte-identical FTS5 structure" contract):
//!
//! - **the data-plane runtime** (`backend/sqlite/fts.rs::ensure_fts_index`) — creates
//!   the FTS index lazily on the data plane, schema-QUALIFIED into the app's
//!   ATTACHed database (`"<app>"."<coll>__fts"`).
//! - **the zero-migrate engine** (`declarative.rs` SQLite emitter) — emits the
//!   SAME FTS index as a migration `up`/`down`, UNqualified because the engine
//!   opens the per-app file directly as `main`.
//!
//! The qualification is the only difference; both produce the identical FTS5
//! vtable + trigger structure, so an engine-applied FTS index and a runtime-built
//! one are interchangeable. Each builder takes `schema: Option<&str>` — `Some(app)`
//! emits the schema-qualified form (plugin-db's ATTACH alias), `None` emits the
//! unqualified `main` form (the migrate engine's confined SQLite path).
//!
//! ## FTS5 vtable shape
//!
//! - **External-content** (`content="<coll>"`, `content_rowid="rowid"`): the FTS5
//!   index stores only the tokenised inverted-index payload; the source text
//!   stays in the base table.
//! - **AFTER triggers** (`__fts_ai` / `__fts_au` / `__fts_ad`): mirror every row
//!   mutation onto the FTS index in the same transaction. Fire AFTER (vs. BEFORE)
//!   so the canonical row state has landed by the time the FTS index sees it. The
//!   DELETE/UPDATE-eviction use the FTS5 external-content "delete command" form
//!   (`INSERT INTO <fts>(<fts>, rowid, cols) VALUES ('delete', OLD.rowid, …)`),
//!   because a plain `DELETE FROM` is not allowed on an external-content vtable.
//!
//! **Unqualified body table** (SQLite engine rule): the table referenced inside a
//! trigger BODY cannot be database-qualified — SQLite resolves it within the same
//! attached database as the trigger. So the body always references `"<coll>__fts"`
//! unqualified, even when the header is qualified.

use crate::query::quote_ident;

/// The fixed suffix of the FTS5 virtual table for a collection (`<coll>__fts`).
#[must_use]
pub fn fts_vtable_name(collection: &str) -> String {
    format!("{collection}__fts")
}

/// The fixed AFTER-INSERT / DELETE / UPDATE trigger names for a collection.
#[must_use]
pub fn fts_trigger_names(collection: &str) -> [String; 3] {
    [
        format!("{collection}__fts_ai"),
        format!("{collection}__fts_ad"),
        format!("{collection}__fts_au"),
    ]
}

/// Render `"<schema>"."<name>"` when `schema` is `Some`, else the unqualified
/// `"<name>"` (the migrate engine's `main`-scoped confined path).
fn qualified(schema: Option<&str>, name: &str) -> String {
    schema.map_or_else(
        || quote_ident(name),
        |s| format!("{}.{}", quote_ident(s), quote_ident(name)),
    )
}

/// Build the `CREATE VIRTUAL TABLE IF NOT EXISTS` DDL for the external-content
/// FTS5 vtable backing `<collection>`.
///
/// Shape (canonical):
/// ```sql
/// CREATE VIRTUAL TABLE IF NOT EXISTS "<schema>"."<coll>__fts"
///   USING fts5("col1","col2", content="<coll>", content_rowid="rowid")
/// ```
#[must_use]
pub fn build_create_fts_table_sql(
    schema: Option<&str>,
    collection: &str,
    columns: &[String],
) -> String {
    let qfts = qualified(schema, &fts_vtable_name(collection));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(",");
    let qcoll = quote_ident(collection);
    let qrowid = quote_ident("rowid");
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {qfts} \
         USING fts5({qcols_csv}, content={qcoll}, content_rowid={qrowid})"
    )
}

/// Build the initial-population `INSERT INTO __fts SELECT FROM coll` statement.
///
/// NOT idempotent on re-run (it doubles the index payload); the caller runs it
/// exactly once, at vtable-creation time.
#[must_use]
pub fn build_initial_population_sql(
    schema: Option<&str>,
    collection: &str,
    columns: &[String],
) -> String {
    let qfts = qualified(schema, &fts_vtable_name(collection));
    let qcoll = qualified(schema, collection);
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {qfts} (rowid, {qcols_csv}) \
         SELECT rowid, {qcols_csv} FROM {qcoll}"
    )
}

/// Build the `AFTER INSERT` trigger DDL. The trigger header may be qualified; the
/// table inside the BODY is always unqualified (SQLite rule).
#[must_use]
pub fn build_insert_trigger_sql(
    schema: Option<&str>,
    collection: &str,
    columns: &[String],
) -> String {
    let qcoll = qualified(schema, collection);
    let qfts_unqual = quote_ident(&fts_vtable_name(collection));
    let qtrg = qualified(schema, &format!("{collection}__fts_ai"));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let new_cols_csv: String = columns
        .iter()
        .map(|c| format!("NEW.{}", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qtrg} \
         AFTER INSERT ON {qcoll} BEGIN \
         INSERT INTO {qfts_unqual} (rowid, {qcols_csv}) \
         VALUES (NEW.rowid, {new_cols_csv}); END"
    )
}

/// Build the `AFTER DELETE` trigger DDL (FTS5 external-content "delete command"
/// form — a plain `DELETE FROM` is not allowed on an external-content vtable).
#[must_use]
pub fn build_delete_trigger_sql(
    schema: Option<&str>,
    collection: &str,
    columns: &[String],
) -> String {
    let qcoll = qualified(schema, collection);
    let qfts_unqual = quote_ident(&fts_vtable_name(collection));
    let sentinel = quote_ident(&fts_vtable_name(collection));
    let qtrg = qualified(schema, &format!("{collection}__fts_ad"));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let old_cols_csv: String = columns
        .iter()
        .map(|c| format!("OLD.{}", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qtrg} \
         AFTER DELETE ON {qcoll} BEGIN \
         INSERT INTO {qfts_unqual} ({sentinel}, rowid, {qcols_csv}) \
         VALUES ('delete', OLD.rowid, {old_cols_csv}); END"
    )
}

/// Build the `AFTER UPDATE OF cols` trigger DDL — evict the OLD row via the FTS5
/// `'delete'` command, then insert the NEW row.
#[must_use]
pub fn build_update_trigger_sql(
    schema: Option<&str>,
    collection: &str,
    columns: &[String],
) -> String {
    let qcoll = qualified(schema, collection);
    let qfts_unqual = quote_ident(&fts_vtable_name(collection));
    let sentinel = quote_ident(&fts_vtable_name(collection));
    let qtrg = qualified(schema, &format!("{collection}__fts_au"));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let old_cols_csv: String = columns
        .iter()
        .map(|c| format!("OLD.{}", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let new_cols_csv: String = columns
        .iter()
        .map(|c| format!("NEW.{}", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qtrg} \
         AFTER UPDATE OF {qcols_csv} ON {qcoll} BEGIN \
         INSERT INTO {qfts_unqual} ({sentinel}, rowid, {qcols_csv}) \
         VALUES ('delete', OLD.rowid, {old_cols_csv}); \
         INSERT INTO {qfts_unqual} (rowid, {qcols_csv}) \
         VALUES (NEW.rowid, {new_cols_csv}); END"
    )
}

/// Parse the FTS-indexed source column list out of a stored `CREATE VIRTUAL TABLE
/// … USING fts5(...)` statement (the form `build_create_fts_table_sql` emits).
///
/// Returns the ordered, unquoted column names — the `content=` / `content_rowid=`
/// options and the `tokenize=`/etc. options are excluded (an FTS5 option is any
/// argument containing `=`). `None` if `sql` is not an `fts5(...)` create
/// statement. Used by the SQLite drift introspector to round-trip an FTS index
/// back to the same `IndexSnapshot` the desired-snapshot author produced.
#[must_use]
pub fn parse_fts5_columns(sql: &str) -> Option<Vec<String>> {
    let lower = sql.to_ascii_lowercase();
    let using_at = lower.find("using fts5")?;
    let rest = &sql[using_at..];
    let open = rest.find('(')?;
    // Find the matching close paren (the column/option list never nests parens in
    // our emitted shape, but scan defensively).
    let bytes = rest.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &rest[open + 1..close?];
    let mut cols = Vec::new();
    for arg in inner.split(',') {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        // An FTS5 OPTION (`content="x"`, `content_rowid="rowid"`, `tokenize=…`,
        // `prefix=…`) carries an `=`; a plain column does not. Skip options.
        if arg.contains('=') {
            continue;
        }
        // Strip surrounding double-quotes and un-double embedded `""`.
        let unq = if arg.starts_with('"') && arg.ends_with('"') && arg.len() >= 2 {
            arg[1..arg.len() - 1].replace("\"\"", "\"")
        } else {
            arg.to_string()
        };
        cols.push(unq);
    }
    Some(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_fts_table_qualified_matches_plugin_db_shape() {
        let ddl = build_create_fts_table_sql(
            Some("myapp"),
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        assert_eq!(
            ddl,
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"myapp\".\"docs__fts\" \
             USING fts5(\"title\",\"body\", content=\"docs\", content_rowid=\"rowid\")"
        );
    }

    #[test]
    fn create_fts_table_unqualified_for_engine_main() {
        let ddl =
            build_create_fts_table_sql(None, "docs", &["body".to_string()]);
        assert_eq!(
            ddl,
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"docs__fts\" \
             USING fts5(\"body\", content=\"docs\", content_rowid=\"rowid\")"
        );
    }

    #[test]
    fn insert_trigger_body_table_is_unqualified() {
        let sql = build_insert_trigger_sql(
            Some("myapp"),
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        assert_eq!(
            sql,
            "CREATE TRIGGER IF NOT EXISTS \"myapp\".\"docs__fts_ai\" \
             AFTER INSERT ON \"myapp\".\"docs\" BEGIN \
             INSERT INTO \"docs__fts\" (rowid, \"title\", \"body\") \
             VALUES (NEW.rowid, NEW.\"title\", NEW.\"body\"); END"
        );
    }

    #[test]
    fn delete_trigger_uses_fts5_delete_command() {
        let sql = build_delete_trigger_sql(
            Some("myapp"),
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        assert_eq!(
            sql,
            "CREATE TRIGGER IF NOT EXISTS \"myapp\".\"docs__fts_ad\" \
             AFTER DELETE ON \"myapp\".\"docs\" BEGIN \
             INSERT INTO \"docs__fts\" (\"docs__fts\", rowid, \"title\", \"body\") \
             VALUES ('delete', OLD.rowid, OLD.\"title\", OLD.\"body\"); END"
        );
    }

    #[test]
    fn update_trigger_evicts_then_inserts() {
        let sql = build_update_trigger_sql(
            Some("myapp"),
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        assert_eq!(
            sql,
            "CREATE TRIGGER IF NOT EXISTS \"myapp\".\"docs__fts_au\" \
             AFTER UPDATE OF \"title\", \"body\" ON \"myapp\".\"docs\" BEGIN \
             INSERT INTO \"docs__fts\" (\"docs__fts\", rowid, \"title\", \"body\") \
             VALUES ('delete', OLD.rowid, OLD.\"title\", OLD.\"body\"); \
             INSERT INTO \"docs__fts\" (rowid, \"title\", \"body\") \
             VALUES (NEW.rowid, NEW.\"title\", NEW.\"body\"); END"
        );
    }

    #[test]
    fn initial_population_qualified() {
        let sql = build_initial_population_sql(
            Some("myapp"),
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        assert_eq!(
            sql,
            "INSERT INTO \"myapp\".\"docs__fts\" (rowid, \"title\", \"body\") \
             SELECT rowid, \"title\", \"body\" FROM \"myapp\".\"docs\""
        );
    }

    #[test]
    fn parse_columns_round_trips_create_text() {
        let ddl = build_create_fts_table_sql(
            None,
            "posts",
            &["body".to_string(), "title".to_string()],
        );
        assert_eq!(
            parse_fts5_columns(&ddl),
            Some(vec!["body".to_string(), "title".to_string()])
        );
    }

    #[test]
    fn parse_columns_excludes_options_and_handles_quotes() {
        let sql = "CREATE VIRTUAL TABLE \"x__fts\" USING fts5(\"a\", \"b\", content=\"x\", content_rowid=\"rowid\")";
        assert_eq!(
            parse_fts5_columns(sql),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(parse_fts5_columns("CREATE TABLE foo (a int)"), None);
    }
}
