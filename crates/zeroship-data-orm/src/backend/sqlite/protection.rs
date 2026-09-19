//! Catalog evidence and column-key access for the protection pipeline.
use super::*;
use crate::{error::DbError, protection::Protection};
use async_trait::async_trait;

#[async_trait(?Send)]
impl crate::protection::Catalog for SqliteBackend {
    /// Read user tables, columns, indexes, and foreign keys from SQLite PRAGMAs.
    async fn introspect_schema(
        &self,
        binding: &crate::binding::DbBinding,
        transaction: Option<&crate::driver::Session>,
    ) -> Result<crate::sql::catalog::LiveSchema, DbError> {
        let shared;
        let session = if let Some(transaction) = transaction {
            transaction
                .get::<super::session::SqliteSessionHandle>()
                .ok_or_else(|| DbError::internal("catalog received a non-SQLite session"))?
        } else {
            self.attach_binding(binding).await?;
            shared = super::session::SqliteSessionHandle::new(self.session.clone());
            &shared
        };
        let mut out = crate::sql::catalog::LiveSchema::default();

        // The catalog of the database the binding's statements address.
        let q_app = crate::sql::mapping::quote_ident(Self::database_alias(binding));
        let tables_sql =
            format!("SELECT name FROM {q_app}.sqlite_master WHERE type = 'table' ORDER BY name");
        let table_rows = session.query(&tables_sql, &[]).await?;
        let mut user_tables: Vec<String> = Vec::with_capacity(table_rows.len());
        for row in &table_rows {
            let name = row.first().and_then(|c| c.clone()).unwrap_or_default();
            if name.starts_with("sqlite_") {
                continue;
            }
            user_tables.push(name);
        }

        for collection in &user_tables {
            let q_coll = crate::sql::mapping::quote_ident(collection);

            let table_info_sql = format!("PRAGMA {q_app}.table_info({q_coll})");
            let col_rows = session.query(&table_info_sql, &[]).await?;

            // Read protection sentinels from stored CREATE TABLE text because
            // PRAGMA table_info does not retain column comments.
            let master_sql_query = format!(
                "SELECT sql FROM {q_app}.sqlite_master \
                 WHERE type = 'table' AND name = ?"
            );
            let master_rows = session
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
                        // SQLite does not expose default volatility.
                        default_volatility: None,
                        encrypted,
                        mask,
                        ..Default::default()
                    },
                );
            }
            if !col_map.is_empty() {
                out.tables.insert(collection.clone(), col_map);
            }

            let index_list_sql = format!("PRAGMA {q_app}.index_list({q_coll})");
            let idx_rows = session.query(&index_list_sql, &[]).await?;
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
                    continue;
                }

                let q_idx = crate::sql::mapping::quote_ident(&idx_name);
                let index_info_sql = format!("PRAGMA {q_app}.index_info({q_idx})");
                let info_rows = session.query(&index_info_sql, &[]).await?;
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
                        // SQLite exposes only usable indexes here.
                        is_valid: true,
                    },
                );
            }
            if !idx_map.is_empty() {
                out.indexes.insert(collection.clone(), idx_map);
            }

            let fk_sql = format!("PRAGMA {q_app}.foreign_key_list({q_coll})");
            let fk_rows = session.query(&fk_sql, &[]).await?;
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
                        on_delete,
                        on_update,
                        // SQLite's PRAGMA omits deferrability.
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
