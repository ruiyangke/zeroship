//! SQLite full-text search helpers — FTS5 external-content virtual
//! tables + AFTER-trigger mirror lifecycle.
//!
//! **P4 PR 5** (`docs/proposals/p4-search-implementation-plan.md` §4.2
//! + §8 PR 5). This module owns the SQL-shape primitives — the five
//! idempotent DDL statements (`CREATE VIRTUAL TABLE`, initial
//! population `INSERT … SELECT`, three AFTER triggers) and the
//! search SQL composition (`SELECT … JOIN … MATCH … ORDER BY bm25`).
//!
//! The actual `ensure_fts_index` / `fts_search` orchestration lives in
//! [`crate::backend::sqlite::mod.rs`]'s `impl FullTextIndex for
//! SqliteBackend` block — this module is the strict primitive layer so
//! the SQL string shapes stay unit-testable in isolation against the
//! plan's documented form.
//!
//! ## FTS5 vtable shape
//!
//! - **External-content** (`content="<coll>"`, `content_rowid="rowid"`):
//!   the FTS5 index stores only the tokenised inverted-index payload;
//!   the source text stays in the base table. This avoids the doubled
//!   storage the default contentless / standalone FTS5 modes would
//!   incur and keeps `SELECT t.* FROM coll t JOIN coll__fts f ON
//!   t.rowid = f.rowid` returning every base-column unchanged.
//! - **AFTER triggers** (`__fts_ai` / `__fts_au` / `__fts_ad`): mirror
//!   every row mutation onto the FTS index in the same transaction.
//!   Fire AFTER (vs. BEFORE) so the canonical row state has landed by
//!   the time the FTS index sees it.
//!
//! ## Trigger-vs-preupdate-hook ordering (Q-P4-F)
//!
//! The P2 CDC adapter installs a `preupdate_hook` on the rusqlite
//! connection. SQLite's documented hook order is:
//!
//! 1. `preupdate_hook` fires BEFORE the row mutation (carrying the
//!    pre-image for UPDATE / DELETE).
//! 2. The row mutation lands.
//! 3. AFTER triggers fire on the post-image.
//! 4. `update_hook` fires (post-mutation, post-trigger).
//!
//! Both `preupdate` and the AFTER triggers execute within the same
//! transaction — the broker sees the base-row event with the FTS
//! index already updated atomically with the row at COMMIT. There is
//! no race window: a subscriber draining its `CommitPacket` and
//! querying the FTS index back will always see consistent state
//! (the FTS row landed by the time COMMIT fires, regardless of when
//! the publisher task wakes up to drain the channel).

use crate::query::{build_where, quote_ident};

/// Build the `CREATE VIRTUAL TABLE IF NOT EXISTS` DDL for the
/// external-content FTS5 vtable backing `<app>.<collection>`.
///
/// Shape (canonical, plan §4.2):
/// ```sql
/// CREATE VIRTUAL TABLE IF NOT EXISTS "<app>"."<coll>__fts"
///   USING fts5("col1","col2", content="<coll>", content_rowid="rowid")
/// ```
///
/// **External-content mode** ties the FTS index to the base table
/// through `content` / `content_rowid` so SELECT-time joins resolve
/// every base-row column without doubled storage. FTS5 reads the
/// source text from `<coll>` on rebuild.
///
/// **Identifier quoting**: `app_id`, `collection`, and every column
/// in `columns` are routed through [`crate::query::quote_ident`] which
/// double-quotes and doubles embedded `"` characters. The `content` /
/// `content_rowid` option values are also double-quoted per FTS5's
/// option-value syntax (the FTS5 parser accepts both single-quoted
/// and double-quoted option values; we use the double-quoted form
/// for visual consistency with the SQL surrounds).
///
/// **Caller contract**: `columns` is the canonical ordered list of
/// FTS-flagged columns on the collection (one per `t.string().fts()`
/// in the SDK). Empty `columns` produces an FTS5 vtable with no
/// indexed columns — the engine would accept this but every search
/// would return zero hits; the caller (the `ensure_fts_index` impl)
/// is responsible for refusing the empty case loudly.
pub(crate) fn build_create_fts_table_sql(
    app_id: &str,
    collection: &str,
    columns: &[String],
) -> String {
    let qschema = quote_ident(app_id);
    let qfts = quote_ident(&format!("{collection}__fts"));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(",");
    let qcoll = quote_ident(collection);
    let qrowid = quote_ident("rowid");
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {qschema}.{qfts} \
         USING fts5({qcols_csv}, content={qcoll}, content_rowid={qrowid})"
    )
}

/// Build the initial-population `INSERT INTO __fts SELECT FROM coll`
/// statement.
///
/// Shape (plan §4.2):
/// ```sql
/// INSERT INTO "<app>"."<coll>__fts" (rowid, "col1", "col2")
///   SELECT rowid, "col1", "col2" FROM "<app>"."<coll>"
/// ```
///
/// **Idempotency**: the statement is NOT idempotent on re-run — calling
/// it twice doubles the FTS index payload. The `ensure_fts_index` impl
/// runs it exactly once at vtable-creation time. A second `register_model`
/// call against a collection whose FTS index already exists short-circuits
/// before this statement via `CREATE VIRTUAL TABLE IF NOT EXISTS` (the
/// vtable presence is checked by `sqlite_master` lookup; if it's already
/// there, the create is a no-op and we skip the population).
///
/// **PR 5 ensures the population runs only once** by gating it on
/// "did we just create the vtable" — the impl block in `mod.rs` performs
/// a `SELECT 1 FROM sqlite_master WHERE name = '<coll>__fts'` probe
/// before running this statement.
pub(crate) fn build_initial_population_sql(
    app_id: &str,
    collection: &str,
    columns: &[String],
) -> String {
    let qschema = quote_ident(app_id);
    let qfts = quote_ident(&format!("{collection}__fts"));
    let qcoll = quote_ident(collection);
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {qschema}.{qfts} (rowid, {qcols_csv}) \
         SELECT rowid, {qcols_csv} FROM {qschema}.{qcoll}"
    )
}

/// Build the `AFTER INSERT` trigger DDL.
///
/// Shape (plan §4.2, adjusted for the SQLite trigger-body rule):
/// ```sql
/// CREATE TRIGGER IF NOT EXISTS "<app>"."<coll>__fts_ai"
///   AFTER INSERT ON "<app>"."<coll>"
///   BEGIN
///     INSERT INTO "<coll>__fts" (rowid, "col1", "col2")
///       VALUES (NEW.rowid, NEW."col1", NEW."col2");
///   END
/// ```
///
/// The trigger fires AFTER each base-table INSERT so the FTS index
/// reflects every committed row. SQLite serialises this AFTER-trigger
/// body within the same transaction as the parent INSERT — a ROLLBACK
/// of the parent rolls back the trigger's INSERT too.
///
/// **Unqualified body table** (SQLite engine rule): the CREATE TRIGGER
/// statement and the trigger header can carry a qualified
/// `"<schema>"."<name>"` form, but the SQL inside the BEGIN..END block
/// must reference tables WITHOUT a schema qualifier. SQLite resolves
/// those names within the same attached database as the trigger
/// itself. See `https://www.sqlite.org/lang_createtrigger.html` —
/// section "Restrictions on CREATE TRIGGER":
///   > The table referenced by the SQL statements inside the body of
///   > a trigger cannot be qualified by the database name.
///
/// In practice the FTS5 vtable `<coll>__fts` is co-located with the
/// base table `<coll>` (we create it in the same `app_id`-keyed
/// attached database), so the unqualified reference inside the body
/// resolves to the correct vtable.
pub(crate) fn build_insert_trigger_sql(
    app_id: &str,
    collection: &str,
    columns: &[String],
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qfts_unqual = quote_ident(&format!("{collection}__fts"));
    let qtrg = quote_ident(&format!("{collection}__fts_ai"));
    let qcols_csv: String = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    // `NEW."col"` reads from the row being inserted. Same identifier
    // quoting as the column list — keeps any embedded `"` in the
    // column name handled uniformly.
    let new_cols_csv: String = columns
        .iter()
        .map(|c| format!("NEW.{}", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER INSERT ON {qschema}.{qcoll} BEGIN \
         INSERT INTO {qfts_unqual} (rowid, {qcols_csv}) \
         VALUES (NEW.rowid, {new_cols_csv}); END"
    )
}

/// Build the `AFTER DELETE` trigger DDL.
///
/// Shape (plan §4.2 + Q-P4-H, refined for FTS5 external-content
/// semantics): external-content FTS5 vtables don't allow plain
/// `DELETE FROM __fts WHERE rowid = OLD.rowid` — the engine needs
/// the OLD column values to remove the right inverted-index entries
/// (it can't look them up from the base table because the AFTER
/// trigger fires post-mutation, so the row is already gone). The
/// FTS5 documented pattern is the special "delete command" form:
///
/// ```sql
/// INSERT INTO "<coll>__fts" (<coll>__fts, rowid, "col1", "col2")
///   VALUES ('delete', OLD.rowid, OLD."col1", OLD."col2");
/// ```
///
/// The `(<coll>__fts)` "column" in the column list is a magic
/// FTS5 sentinel — when the first element of an `INSERT` against a
/// FTS5 vtable matches the vtable's own name, the row is treated as
/// a command (`'delete'`, `'delete-all'`, `'rebuild'`, etc.). See
/// `https://www.sqlite.org/fts5.html` §4.4 "The 'delete' Command".
///
/// ```sql
/// CREATE TRIGGER IF NOT EXISTS "<app>"."<coll>__fts_ad"
///   AFTER DELETE ON "<app>"."<coll>"
///   BEGIN
///     INSERT INTO "<coll>__fts" ("<coll>__fts", rowid, "col1", "col2")
///       VALUES ('delete', OLD.rowid, OLD."col1", OLD."col2");
///   END
/// ```
///
/// Same unqualified-table-in-body rule as `build_insert_trigger_sql`
/// — see that helper's rustdoc.
pub(crate) fn build_delete_trigger_sql(
    app_id: &str,
    collection: &str,
    columns: &[String],
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qfts_unqual = quote_ident(&format!("{collection}__fts"));
    // The FTS5 "command sentinel" column name is the vtable name
    // itself. Inside the trigger body, the table is unqualified so
    // the sentinel matches the unqualified form too.
    let sentinel = quote_ident(&format!("{collection}__fts"));
    let qtrg = quote_ident(&format!("{collection}__fts_ad"));
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
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER DELETE ON {qschema}.{qcoll} BEGIN \
         INSERT INTO {qfts_unqual} ({sentinel}, rowid, {qcols_csv}) \
         VALUES ('delete', OLD.rowid, {old_cols_csv}); END"
    )
}

/// Build the `AFTER UPDATE OF cols` trigger DDL.
///
/// Shape (plan §4.2): scoped to the FTS-indexed columns via `UPDATE OF
/// col1, col2`. Inside the trigger body, the OLD row is removed from
/// the FTS index via the FTS5 `'delete'` command (see
/// `build_delete_trigger_sql`'s rustdoc on why plain `DELETE FROM`
/// doesn't work on external-content FTS5 vtables) and the NEW row is
/// inserted. The `rowid` is stable across UPDATE so the
/// delete-then-insert pair targets the same FTS row.
///
/// ```sql
/// CREATE TRIGGER IF NOT EXISTS "<app>"."<coll>__fts_au"
///   AFTER UPDATE OF "col1","col2" ON "<app>"."<coll>"
///   BEGIN
///     INSERT INTO "<coll>__fts" ("<coll>__fts", rowid, "col1", "col2")
///       VALUES ('delete', OLD.rowid, OLD."col1", OLD."col2");
///     INSERT INTO "<coll>__fts" (rowid, "col1", "col2")
///       VALUES (NEW.rowid, NEW."col1", NEW."col2");
///   END
/// ```
///
/// Scoping the trigger to the FTS columns (vs. the implicit
/// "any-column" form) avoids redundant FTS index churn on UPDATEs
/// that don't touch a tokenised column — same optimisation the PG
/// adapter applies (`BEFORE INSERT OR UPDATE OF cols ON …`). Same
/// unqualified-table-in-body rule as the insert trigger.
pub(crate) fn build_update_trigger_sql(
    app_id: &str,
    collection: &str,
    columns: &[String],
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qfts_unqual = quote_ident(&format!("{collection}__fts"));
    let sentinel = quote_ident(&format!("{collection}__fts"));
    let qtrg = quote_ident(&format!("{collection}__fts_au"));
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
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER UPDATE OF {qcols_csv} ON {qschema}.{qcoll} BEGIN \
         INSERT INTO {qfts_unqual} ({sentinel}, rowid, {qcols_csv}) \
         VALUES ('delete', OLD.rowid, {old_cols_csv}); \
         INSERT INTO {qfts_unqual} (rowid, {qcols_csv}) \
         VALUES (NEW.rowid, {new_cols_csv}); END"
    )
}

/// Build the FTS search SQL.
///
/// Shape (plan §4.2):
/// ```sql
/// SELECT t.*, bm25(f) AS _rank
///   FROM "<app>"."<coll>" t
///   JOIN "<app>"."<coll>__fts" f ON t.rowid = f.rowid
///  WHERE f."<coll>__fts" MATCH ?1
///    [AND <filter>]
///  ORDER BY _rank
///  [LIMIT ?2]
/// ```
///
/// **Parameter contract**: `$1` is the FTS query (bound as TEXT;
/// FTS5 MATCH accepts the raw query syntax verbatim — phrase, prefix
/// (`foo*`), NEAR/AND/OR boolean, etc.); `$2` is the LIMIT when
/// `has_limit` is `true`. The `filter_clause` is spliced into the
/// WHERE composition `AND`-joined with the MATCH predicate; the
/// caller is responsible for pre-allocating the params vector so the
/// builder's `$N` numbering lines up (filter params start at `$2`
/// when `has_limit` is false, or `$3` when true). This is the same
/// param-offset contract pgvector / spatial use upstream.
///
/// **bm25 ordering**: SQLite FTS5's `bm25(f)` returns NEGATIVE doubles
/// where more-relevant rows have MORE-NEGATIVE values. `ORDER BY
/// _rank` (ASC, the default) places the most relevant rows first
/// — same direction the PG `ts_rank DESC` arm produces, just with
/// the sign flipped. The synthetic `_rank` column the caller sees
/// carries the engine value verbatim (caller transformations like
/// `Math.abs` happen SDK-side, not here).
pub(crate) fn build_fts_search_sql(
    app_id: &str,
    collection: &str,
    filter_clause: &str,
    has_limit: bool,
    schema_hint: Option<&serde_json::Value>,
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qfts = quote_ident(&format!("{collection}__fts"));
    let qfts_match_col = quote_ident(&format!("{collection}__fts"));
    let select_expr =
        crate::query::build_masked_aware_select_expr_for_table_alias(schema_hint, "t");
    // **FTS5 `rank` column** (vs. `bm25(<fts_table>)` explicit form):
    // every FTS5 vtable exposes a hidden `rank` column that returns
    // the bm25 score for the currently-matched row. We pick the
    // hidden `rank` column on the aliased FTS table (`f.rank`)
    // because the explicit `bm25(<unqualified-table-or-alias>)` form
    // parses ambiguously when the alias `f` shadows the table name —
    // the SQLite parser interprets `bm25(f)` as "the bm25 of column
    // f" rather than "the bm25 of FTS table f". The `rank` column is
    // equivalent (returns the same bm25 score) and resolves without
    // the ambiguity. See `https://www.sqlite.org/fts5.html` §
    // "Auxiliary Functions - bm25()".
    let mut sql = format!(
        "SELECT {select_expr}, f.rank AS _rank \
         FROM {qschema}.{qcoll} t \
         JOIN {qschema}.{qfts} f ON t.rowid = f.rowid \
         WHERE f.{qfts_match_col} MATCH $1"
    );
    if !filter_clause.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(filter_clause);
    }
    sql.push_str(" ORDER BY _rank");
    if has_limit {
        sql.push_str(" LIMIT $2");
    }
    sql
}

/// Compose the filter clause for an FTS search, advancing the param
/// vector starting at the next free `$N` slot.
///
/// Wraps [`build_where`] with one tweak: every `$N` placeholder in
/// the emitted predicate references the params buffer the caller pre-
/// seeded with `[query_text, limit_str]` (so the first filter param
/// lands at `$3` when a limit is bound, or `$2` otherwise). The
/// builder is already param-offset-aware — this helper just gives a
/// callable name to the operation.
pub(crate) fn build_fts_filter_clause(
    filter: &serde_json::Value,
    params: &mut Vec<String>,
) -> Result<String, crate::query::QueryError> {
    build_where(filter, params)
}

#[cfg(test)]
mod tests {
    //! Unit tests pin the documented SQL output for each helper. The
    //! shape is asserted byte-for-byte against the plan §4.2 form so
    //! a future refactor that drifts the identifier-quoting, the column
    //! list ordering, or the trigger names trips here rather than at
    //! the integration-test boundary.

    use super::*;

    #[test]
    fn create_fts_table_sql_two_columns() {
        let ddl = build_create_fts_table_sql(
            "myapp",
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
    fn create_fts_table_sql_one_column() {
        let ddl =
            build_create_fts_table_sql("myapp", "people", &["bio".to_string()]);
        assert!(
            ddl.contains("USING fts5(\"bio\", content=\"people\", content_rowid=\"rowid\")"),
            "single-column form: {ddl}"
        );
    }

    #[test]
    fn initial_population_sql_two_columns() {
        let sql = build_initial_population_sql(
            "myapp",
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
    fn insert_trigger_sql_two_columns() {
        let sql = build_insert_trigger_sql(
            "myapp",
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        // Trigger body uses UNQUALIFIED `"docs__fts"` (SQLite rule);
        // the trigger header retains the `"myapp"."docs"` qualifier.
        assert_eq!(
            sql,
            "CREATE TRIGGER IF NOT EXISTS \"myapp\".\"docs__fts_ai\" \
             AFTER INSERT ON \"myapp\".\"docs\" BEGIN \
             INSERT INTO \"docs__fts\" (rowid, \"title\", \"body\") \
             VALUES (NEW.rowid, NEW.\"title\", NEW.\"body\"); END"
        );
    }

    #[test]
    fn delete_trigger_sql_shape() {
        let sql = build_delete_trigger_sql(
            "myapp",
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        // FTS5 external-content "delete command" form: INSERT INTO
        // <fts>(<fts>, rowid, cols) VALUES ('delete', OLD.rowid,
        // OLD.cols). Plain DELETE FROM doesn't work on external-content
        // vtables.
        assert_eq!(
            sql,
            "CREATE TRIGGER IF NOT EXISTS \"myapp\".\"docs__fts_ad\" \
             AFTER DELETE ON \"myapp\".\"docs\" BEGIN \
             INSERT INTO \"docs__fts\" (\"docs__fts\", rowid, \"title\", \"body\") \
             VALUES ('delete', OLD.rowid, OLD.\"title\", OLD.\"body\"); END"
        );
    }

    #[test]
    fn update_trigger_sql_two_columns() {
        let sql = build_update_trigger_sql(
            "myapp",
            "docs",
            &["title".to_string(), "body".to_string()],
        );
        // Same FTS5 "delete command" form for the OLD-row eviction,
        // then a plain INSERT for the NEW row.
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
    fn fts_search_sql_no_filter_no_limit() {
        let sql = build_fts_search_sql("myapp", "docs", "", false, None);
        assert_eq!(
            sql,
            "SELECT \"t\".*, f.rank AS _rank \
             FROM \"myapp\".\"docs\" t \
             JOIN \"myapp\".\"docs__fts\" f ON t.rowid = f.rowid \
             WHERE f.\"docs__fts\" MATCH $1 ORDER BY _rank"
        );
    }

    #[test]
    fn fts_search_sql_with_filter_with_limit() {
        let sql = build_fts_search_sql("myapp", "docs", "\"lang\" = $3", true, None);
        assert_eq!(
            sql,
            "SELECT \"t\".*, f.rank AS _rank \
             FROM \"myapp\".\"docs\" t \
             JOIN \"myapp\".\"docs__fts\" f ON t.rowid = f.rowid \
             WHERE f.\"docs__fts\" MATCH $1 AND \"lang\" = $3 ORDER BY _rank LIMIT $2"
        );
    }

    #[test]
    fn fts_search_sql_with_limit_no_filter() {
        let sql = build_fts_search_sql("myapp", "docs", "", true, None);
        assert!(sql.ends_with("ORDER BY _rank LIMIT $2"), "got: {sql}");
        assert!(!sql.contains(" AND "), "no filter -> no AND: {sql}");
    }

    #[test]
    fn fts_search_sql_reads_masked_sibling_when_schema_cached() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "body": { "type": "string" }
        });
        let sql = build_fts_search_sql("app1", "docs", "", false, Some(&schema));
        assert!(
            !sql.starts_with("SELECT \"t\".*"),
            "fts search must not use t.* when masked columns exist: {sql}"
        );
        assert!(
            sql.contains(r#""t"."ssn_masked" AS "ssn""#),
            "fts search must read the masked sibling: {sql}"
        );
    }

    #[test]
    fn create_fts_table_sql_escapes_embedded_quote_in_column() {
        // Hostile column name with an embedded double quote: the
        // helper must double it. The identifier validator at the
        // SDK boundary rejects these names long before reaching this
        // helper, but the lexical layer stays robust.
        let ddl = build_create_fts_table_sql(
            "app",
            "docs",
            &["ev\"il".to_string()],
        );
        assert!(
            ddl.contains("\"ev\"\"il\""),
            "embedded `\"` must be doubled: {ddl}"
        );
    }

    #[test]
    fn fts_filter_clause_advances_params() {
        // Pre-seed params with `[query_text, limit_str]` so the
        // filter starts at `$3`. The builder is param-offset-aware.
        let mut params: Vec<String> = vec!["rust".into(), "100".into()];
        let clause = build_fts_filter_clause(
            &serde_json::json!({ "lang": "en" }),
            &mut params,
        )
        .expect("filter compiles");
        assert!(clause.contains("$3"), "filter param must be $3: {clause}");
        assert_eq!(params.len(), 3, "params must grow by 1");
        assert_eq!(params[2], "en");
    }
}
