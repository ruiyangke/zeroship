//! PostgreSQL live-schema introspection.
//!
//! **PG TIER.** Catalog SQL, [`compio_postgres::Pool`], PostgreSQL row
//! decoding, and PostgreSQL foreign-key action codes are vendor mechanics.
//! The neutral schema snapshot and metadata stay in `zeroship-data-orm::sql`; this
//! module populates those values from PostgreSQL without embedding the driver
//! in the shared schema floor.

use crate::sql::catalog::{ColumnInfo, ForeignKeyInfo, IndexInfo, LiveSchema, MaskMeta};
use compio_postgres::Pool;

/// Error from the live-schema introspection helpers.
///
/// Carries the per-call-site context phrase (for example,
/// `"read columns failed"`) plus the underlying driver error. The PG error
/// translator re-creates the exact `coded_sql("diff: <context>", e)` shape,
/// preserving both SQLSTATE classification and the operator-facing message.
#[derive(Debug)]
pub(crate) struct SchemaError {
    /// The module-local context phrase without the `"diff: "` prefix.
    pub(crate) context: String,
    /// The underlying driver error, carried verbatim for SQLSTATE
    /// classification at the PG error boundary.
    pub(crate) source: compio_postgres::Error,
}

impl SchemaError {
    /// Wrap a driver error with a context phrase.
    #[must_use]
    fn new(context: impl Into<String>, source: compio_postgres::Error) -> Self {
        Self {
            context: context.into(),
            source,
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for SchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Wrap a driver error in [`SchemaError`] with a context phrase so operators
/// see what the introspection layer was doing when the SQL failed.
fn coded_sql(context: &str, e: compio_postgres::Error) -> SchemaError {
    SchemaError::new(context, e)
}

/// Read catalog metadata for the bound physical schema on the supplied connection.
pub(crate) async fn read_live_schema(
    client: &compio_postgres::Client,
    schema: &str,
) -> Result<LiveSchema, SchemaError> {
    let mut out = LiveSchema::default();
    let params = [schema];

    // ----- columns -----
    //
    // Read protection metadata from column comments and attach it after the
    // complete column map is available.
    let col_sql = r#"
SELECT c.relname AS table_name,
       a.attname AS column_name,
       format_type(a.atttypid, a.atttypmod) AS pg_type,
       a.attnotnull AS not_null,
       pg_get_expr(ad.adbin, ad.adrelid) AS default_expr,
       (SELECT MIN(p.provolatile::text)
          FROM pg_depend d
          JOIN pg_proc p ON p.oid = d.refobjid
         WHERE d.classid = 'pg_attrdef'::regclass
           AND d.objid = ad.oid
           AND d.refclassid = 'pg_proc'::regclass) AS default_volatility,
       pgd.description AS pg_comment
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
  LEFT JOIN pg_description pgd
         ON pgd.objoid = c.oid
        AND pgd.objsubid = a.attnum
 WHERE n.nspname = $1
   AND c.relkind = 'r'
   AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY c.relname, a.attnum
"#;
    let rows = client
        .query_text_params(col_sql, &params)
        .await
        .map_err(|e| coded_sql("read columns failed", e))?;
    // Collect mask sentinels, then attach their metadata to the completed column map.
    let mut mask_sentinels: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let column: String = row.try_get("column_name").unwrap_or_default();
        let pg_type: String = row.try_get("pg_type").unwrap_or_default();
        let not_null: bool = row.try_get("not_null").unwrap_or(false);
        let default_expr: Option<String> = row.try_get::<_, String>("default_expr").ok();
        let default_volatility = row
            .try_get::<_, String>("default_volatility")
            .ok()
            .and_then(|s| s.chars().next());
        let pg_comment: Option<String> = row.try_get::<_, String>("pg_comment").ok();
        // Recover protection markers from persisted column comments.
        // Mask metadata is attached after all columns have been collected.
        let mut encrypted = false;
        if let Some(comment) = &pg_comment {
            // The mask sentinel belongs to the visible field column.
            if comment.starts_with(crate::sql::mask_codec::MASK_SENTINEL_PREFIX) {
                mask_sentinels.insert((table.clone(), column.clone()), comment.clone());
            } else if crate::sql::mask_codec::is_encryption_sentinel(comment) {
                encrypted = true;
            }
        }
        out.tables.entry(table).or_default().insert(
            column,
            ColumnInfo {
                pg_type,
                not_null,
                default_expr,
                default_volatility,
                encrypted,
                // Remaining fields default; vector/geo are populated
                // from `information_schema` + `pg_indexes` introspection.
                ..Default::default()
            },
        );
    }
    // Attach each mask sentinel to the declared display column that carries it.
    for ((table, display_column), sentinel) in mask_sentinels {
        let Some(table_cols) = out.tables.get_mut(&table) else {
            continue;
        };
        let Some(column) = table_cols.get_mut(&display_column) else {
            tracing::warn!(
                table = %table,
                column = %display_column,
                "diff: mask sentinel on a column the introspection did not \
                 return; ignoring",
            );
            continue;
        };
        let (kind, classification) = match crate::sql::mask_codec::parse_mask_sentinel(&sentinel) {
            Ok(p) => p,
            Err(e) => {
                // Surface a malformed sentinel as a tracing::warn.
                // the diff will then treat the parent as
                // `mask: None` and a re-deploy would re-emit the
                // sentinel via the AddColumn / CreateTable path.
                // We don't propagate as an Err because a transient
                // hand-edit shouldn't take the entire deploy down;
                // operators get a loud warn instead.
                tracing::warn!(
                    table = %table,
                    column = %display_column,
                    sentinel = %sentinel,
                    error = %e,
                    "diff: malformed mask sentinel on a PG column; \
                     treating it as unmasked"
                );
                continue;
            }
        };
        column.mask = Some(MaskMeta {
            kind,
            classification,
            raw_column: crate::sql::mapping::raw_column_name(&display_column),
        });
    }

    // ----- foreign keys -----
    // pg_constraint.contype = 'f' is a foreign-key. confkey/conkey are
    // arrays of column attribute numbers — we resolve them to names via
    // pg_attribute joins. confdeltype / confupdtype are one-char codes
    // mapped to the SQL keyword equivalents below.
    let fk_sql = r#"
SELECT con.conname AS constraint_name,
       cl.relname AS table_name,
       (SELECT a.attname FROM pg_attribute a
         WHERE a.attrelid = con.conrelid AND a.attnum = con.conkey[1]) AS column_name,
       fcl.relname AS target_table,
       (SELECT a.attname FROM pg_attribute a
         WHERE a.attrelid = con.confrelid AND a.attnum = con.confkey[1]) AS target_column,
       con.confdeltype AS on_delete,
       con.confupdtype AS on_update,
       con.condeferrable AS deferrable
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_class fcl ON fcl.oid = con.confrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND con.contype = 'f'
"#;
    let rows = client
        .query_text_params(fk_sql, &params)
        .await
        .map_err(|e| coded_sql("read foreign_keys failed", e))?;
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let constraint_name: String = row.try_get("constraint_name").unwrap_or_default();
        let column: String = row.try_get("column_name").unwrap_or_default();
        let target_table: String = row.try_get("target_table").unwrap_or_default();
        let target_column: String = row.try_get("target_column").unwrap_or_default();
        let on_delete_code: String = row.try_get("on_delete").unwrap_or_default();
        let on_update_code: String = row.try_get("on_update").unwrap_or_default();
        let deferrable: bool = row.try_get("deferrable").unwrap_or(false);

        let on_delete = decode_fk_action(&on_delete_code);
        let on_update = decode_fk_action(&on_update_code);

        out.foreign_keys.entry(table).or_default().insert(
            column.clone(),
            ForeignKeyInfo {
                constraint_name,
                column,
                target_table,
                target_column,
                on_delete: on_delete.to_string(),
                on_update: on_update.to_string(),
                deferrable,
            },
        );
    }

    // ----- indexes -----
    let idx_sql = r#"
SELECT c.relname AS table_name,
       ic.relname AS index_name,
       i.indisunique AS is_unique,
       i.indisvalid AS is_valid,
       array_to_string(ARRAY(
         SELECT a.attname FROM pg_attribute a
          WHERE a.attrelid = c.oid AND a.attnum = ANY(i.indkey)
          ORDER BY array_position(i.indkey, a.attnum)
       ), ',') AS column_list
  FROM pg_index i
  JOIN pg_class c ON c.oid = i.indrelid
  JOIN pg_class ic ON ic.oid = i.indexrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1
   AND NOT i.indisprimary
"#;
    let rows = client
        .query_text_params(idx_sql, &params)
        .await
        .map_err(|e| coded_sql("read indexes failed", e))?;
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let index_name: String = row.try_get("index_name").unwrap_or_default();
        let is_unique: bool = row.try_get("is_unique").unwrap_or(false);
        let is_valid: bool = row.try_get("is_valid").unwrap_or(false);
        let column_list: String = row.try_get("column_list").unwrap_or_default();
        let columns: Vec<String> = column_list
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        out.indexes.entry(table).or_default().insert(
            index_name,
            IndexInfo {
                is_unique,
                columns,
                is_valid,
            },
        );
    }

    Ok(out)
}

/// Decode Postgres' single-character FK action code into the SQL keyword
/// equivalent. The `pg_constraint` columns `confdeltype` and `confupdtype` use
/// these codes.
fn decode_fk_action(code: &str) -> &'static str {
    match code.chars().next().unwrap_or('a') {
        'a' => "NO ACTION",
        'r' => "RESTRICT",
        'c' => "CASCADE",
        'n' => "SET NULL",
        'd' => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

/// Cheap row-count check used by the classifier. Returns 0 if the table does
/// not exist. The query uses `pg_class.reltuples` for non-blocking estimation
/// when the table is large; the cold-start orchestrator already holds the
/// advisory lock, so an estimate is sufficient for the empty/non-empty choice.
pub(crate) async fn estimate_row_count(
    pool: &Pool,
    app_id: &str,
    collection: &str,
) -> Result<i64, SchemaError> {
    let sql = r#"
SELECT COALESCE(c.reltuples::bigint, 0) AS rows
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'r'
"#;
    let params: Vec<&str> = vec![app_id, collection];
    let rows = pool
        .query_text_params(sql, &params)
        .await
        .map_err(|e| coded_sql("estimate_row_count failed", e))?;
    let n: i64 = rows.first().map(|r| r.get::<_, i64>("rows")).unwrap_or(0);
    Ok(n)
}
