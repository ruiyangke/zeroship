//! Catalog evidence and column-key access for the protection pipeline.
use super::*;
use crate::{error::DbError, protection::Protection};
use async_trait::async_trait;

#[async_trait(?Send)]
impl crate::protection::Catalog for SqliteBackend {
    // Same associated type as the PG impl — the diff engine consumes
    // a uniform `LiveSchema` shape; the SQLite impl populates the
    // PG-style `pg_type` strings with SQLite affinity names
    // (`TEXT`/`INTEGER`/`REAL`/`BLOB`/`NUMERIC`). Classifier
    // teaching about the new vocabulary follows in a later PR.

    /// Walk the SQLite catalog for `app_id`'s attached database and
    /// produce a [`crate::sql::catalog::LiveSchema`] in the same shape the PG
    /// impl emits — populated via four PRAGMA round-trips per table:
    ///
    /// 1. `SELECT name FROM "<app_id>".sqlite_master WHERE type='table'`
    ///    — table list, filtered to user tables (`sqlite_*` system
    ///    tables and our `__zs_*` bookkeeping tables are excluded).
    /// 2. `PRAGMA "<app_id>".table_info("<collection>")` — columns:
    ///    name, type, notnull (0/1), dflt_value, pk.
    /// 3. `PRAGMA "<app_id>".index_list("<collection>")` — indexes:
    ///    seq, name, unique (0/1), origin, partial. The PG impl
    ///    excludes the primary-key index (`indisprimary`); we mirror
    ///    that by skipping indexes whose `origin = 'pk'`.
    /// 4. For each non-PK index: `PRAGMA "<app_id>".index_info(...)` —
    ///    the index's column list in seqno order.
    /// 5. `PRAGMA "<app_id>".foreign_key_list("<collection>")` —
    ///    FKs: id, seq, table (target), from, to, on_update,
    ///    on_delete, match.
    ///
    /// Per plan §3.4 each PRAGMA flows through the session actor's
    /// `Query` command (one round-trip per call); the totals stay
    /// bounded at `1 + 4N` for N tables, which is fine at dev scale
    /// where this code path runs. The PG impl achieves the same with
    /// 3 SQL statements; folding the SQLite walk into a single SQL
    /// statement isn't possible (PRAGMA is non-composable), but the
    /// per-table count stays well below the orchestrator's budget for
    /// a registration round-trip.
    ///
    /// **System-table filter** (plan §3.4): drop any name beginning
    /// with `sqlite_` (engine-internal) or `__zs_` (our bookkeeping —
    /// migrations / audit / replication). The diff classifier consumes
    /// only user-declared tables; surfacing system tables would
    /// trigger spurious "drop table" classifications.
    async fn introspect_schema(
        &self,
        app_id: &str,
    ) -> Result<crate::sql::catalog::LiveSchema, DbError> {
        self.attach_app_file(app_id).await?;
        let mut out = crate::sql::catalog::LiveSchema::default();

        // 1. Table list. The `app_id` is interpolated as a quoted
        //    identifier — the dialect's `quote_ident` doubles embedded
        //    `"`s; PRAGMA / sqlite_master both accept the dotted form
        //    `"app_id".sqlite_master`.
        let q_app = crate::sql::mapping::quote_ident(app_id);
        let tables_sql =
            format!("SELECT name FROM {q_app}.sqlite_master WHERE type = 'table' ORDER BY name");
        let table_rows = self.session.query(&tables_sql, &[]).await?;
        let mut user_tables: Vec<String> = Vec::with_capacity(table_rows.len());
        for row in &table_rows {
            let name = row.first().and_then(|c| c.clone()).unwrap_or_default();
            // Filter out system + bookkeeping tables (plan §3.4).
            if name.starts_with("sqlite_") || name.starts_with("__zs_") {
                continue;
            }
            user_tables.push(name);
        }

        for collection in &user_tables {
            let q_coll = crate::sql::mapping::quote_ident(collection);

            // 2. Columns via `PRAGMA table_info`.
            //
            //    PRAGMA columns: 0=cid, 1=name, 2=type, 3=notnull,
            //    4=dflt_value, 5=pk. The cell shape is `Option<String>`
            //    uniformly (the session materialises every value as a
            //    stringified `Option<String>`), so we read positionally
            //    and parse the `notnull` "0"/"1" into a bool.
            let table_info_sql = format!("PRAGMA {q_app}.table_info({q_coll})");
            let col_rows = self.session.query(&table_info_sql, &[]).await?;

            // Read protection sentinels from stored CREATE TABLE text because
            // PRAGMA table_info does not retain column comments.
            let master_sql_query = format!(
                "SELECT sql FROM {q_app}.sqlite_master \
                 WHERE type = 'table' AND name = ?"
            );
            let master_rows = self
                .session
                .query(&master_sql_query, &[collection.as_str()])
                .await?;
            let create_table_text: String = master_rows
                .first()
                .and_then(|r| r.first())
                .and_then(|c| c.clone())
                .unwrap_or_default();
            let encryption_by_col = parse_encryption_sentinels(&create_table_text);
            // Recover mask sentinels from the visible columns in the stored DDL.
            let mask_by_parent = parse_mask_sentinels(&create_table_text);

            let mut col_map = std::collections::HashMap::new();
            for row in &col_rows {
                let name = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
                let pg_type = row.get(2).and_then(|c| c.clone()).unwrap_or_default();
                let not_null = row
                    .get(3)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let default_expr = row.get(4).and_then(|c| c.clone());
                let encrypted = encryption_by_col.contains(&name);
                let mask = mask_by_parent.get(&name).cloned();
                col_map.insert(
                    name,
                    crate::sql::catalog::ColumnInfo {
                        pg_type,
                        not_null,
                        default_expr,
                        // SQLite expression defaults are stored as raw
                        // text without a volatility tag (the engine has
                        // no `pg_proc.provolatile` analogue). Leaving
                        // this `None` matches what the PG side sets for
                        // literal defaults; the diff classifier reads
                        // `default_volatility` only when the default
                        // looks like a function call. Future changes can
                        // pattern-match on common volatile defaults
                        // (`CURRENT_TIMESTAMP`, `(unixepoch())`, etc.).
                        default_volatility: None,
                        // New fields default; `vector_dims` /
                        // `is_geopoint` are populated
                        // from `sqlite_master.sql` introspection regexes.
                        encrypted,
                        mask,
                        ..Default::default()
                    },
                );
            }
            if !col_map.is_empty() {
                out.tables.insert(collection.clone(), col_map);
            }

            // 3. Indexes via `PRAGMA index_list` + `PRAGMA index_info`.
            //
            //    `index_list` columns: 0=seq, 1=name, 2=unique,
            //    3=origin, 4=partial. We exclude `origin='pk'` to match
            //    the PG impl's `NOT i.indisprimary` filter.
            let index_list_sql = format!("PRAGMA {q_app}.index_list({q_coll})");
            let idx_rows = self.session.query(&index_list_sql, &[]).await?;
            let mut idx_map = std::collections::HashMap::new();
            for row in &idx_rows {
                let idx_name = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
                let is_unique = row
                    .get(2)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let origin = row.get(3).and_then(|c| c.clone()).unwrap_or_default();
                if origin == "pk" {
                    // PG impl skips primary-key indexes; we mirror.
                    // The auto-generated `sqlite_autoindex_*` names
                    // also appear here, and they all carry origin='pk'
                    // or 'u' (unique constraint). We surface 'u'-origin
                    // indexes because they correspond to declared
                    // UNIQUE columns the diff engine cares about.
                    continue;
                }

                // 4. Columns for this index via `PRAGMA index_info`.
                //    Returns: 0=seqno, 1=cid, 2=name.
                let q_idx = crate::sql::mapping::quote_ident(&idx_name);
                let index_info_sql = format!("PRAGMA {q_app}.index_info({q_idx})");
                let info_rows = self.session.query(&index_info_sql, &[]).await?;
                let mut columns = Vec::with_capacity(info_rows.len());
                for info_row in &info_rows {
                    let col_name = info_row.get(2).and_then(|c| c.clone()).unwrap_or_default();
                    columns.push(col_name);
                }

                idx_map.insert(
                    idx_name,
                    crate::sql::catalog::IndexInfo {
                        is_unique,
                        columns,
                        // SQLite indexes are always considered valid
                        // once `CREATE INDEX` returns — there is no
                        // analogue to PG's `indisvalid` (which can be
                        // false after a failed `CREATE INDEX
                        // CONCURRENTLY`). Mark every observed index
                        // valid; nothing downstream needs a tri-state.
                        is_valid: true,
                    },
                );
            }
            if !idx_map.is_empty() {
                out.indexes.insert(collection.clone(), idx_map);
            }

            // 5. Foreign keys via `PRAGMA foreign_key_list`.
            //
            //    Columns: 0=id, 1=seq, 2=table (target),
            //    3=from (local column), 4=to (target column),
            //    5=on_update, 6=on_delete, 7=match.
            //
            //    We synthesise a `constraint_name` from the FK id +
            //    local column (SQLite doesn't expose user-given FK
            //    names through PRAGMA — only the implicit auto-name).
            //    The PG impl uses `pg_constraint.conname` directly.
            let fk_sql = format!("PRAGMA {q_app}.foreign_key_list({q_coll})");
            let fk_rows = self.session.query(&fk_sql, &[]).await?;
            let mut fk_map = std::collections::HashMap::new();
            for row in &fk_rows {
                let fk_id = row.first().and_then(|c| c.clone()).unwrap_or_default();
                let target_table = row.get(2).and_then(|c| c.clone()).unwrap_or_default();
                let from_col = row.get(3).and_then(|c| c.clone()).unwrap_or_default();
                let target_column = row.get(4).and_then(|c| c.clone()).unwrap_or_default();
                let on_update = row.get(5).and_then(|c| c.clone()).unwrap_or_default();
                let on_delete = row.get(6).and_then(|c| c.clone()).unwrap_or_default();
                let constraint_name = format!("fk_{fk_id}_{from_col}");
                fk_map.insert(
                    from_col.clone(),
                    crate::sql::catalog::ForeignKeyInfo {
                        constraint_name,
                        column: from_col,
                        target_table,
                        target_column,
                        // SQLite's PRAGMA already emits the upper-case
                        // SQL form ("CASCADE", "SET NULL", "NO
                        // ACTION", …); no decode step needed (contrast
                        // PG's single-char code).
                        on_delete,
                        on_update,
                        // SQLite FKs do not surface a deferrable bit
                        // through PRAGMA. The engine supports
                        // `DEFERRABLE INITIALLY DEFERRED` syntax but
                        // doesn't echo it back via foreign_key_list;
                        // default to `false` to match the PG impl's
                        // bool shape.
                        deferrable: false,
                    },
                );
            }
            if !fk_map.is_empty() {
                out.foreign_keys.insert(collection.clone(), fk_map);
            }
        }

        Ok(out)
    }
}
impl Protection for SqliteBackend {
    fn key_store(&self) -> &crate::encryption::KeyStore {
        self.key_store()
    }
}

impl SqliteBackend {
    pub async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        let q_app = crate::sql::mapping::quote_ident(app_id);
        let q_coll = crate::sql::mapping::quote_ident(collection);
        let sql = format!("SELECT COUNT(*) FROM {q_app}.{q_coll}");
        let rows = match self.session.query(&sql, &[]).await {
            Ok(rows) => rows,
            Err(DbError::Transient { message }) if message.contains("no such table") => {
                return Ok(0);
            }
            Err(e) => return Err(e),
        };
        let n = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.as_deref())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Ok(n)
    }
}
