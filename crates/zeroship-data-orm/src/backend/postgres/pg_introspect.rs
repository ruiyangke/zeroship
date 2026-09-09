//! PostgreSQL live-schema introspection.
//!
//! **PG TIER.** Catalog SQL, [`compio_postgres::Pool`], PostgreSQL row
//! decoding, and PostgreSQL foreign-key action codes are vendor mechanics.
//! The neutral schema snapshot and metadata stay in `zeroship-data-sql`; this
//! module populates those values from PostgreSQL without embedding the driver
//! in the shared schema floor.

use compio_postgres::Pool;
use zeroship_data_sql::catalog::{
    ColumnInfo, EncryptionMeta, ForeignKeyInfo, IndexInfo, LiveSchema, MaskMeta,
};

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

/// Introspect the live schema for the given app + collection. Returns an
/// empty [`LiveSchema`] if the schema itself does not exist yet (first deploy).
///
/// We restrict the catalog scan to the app's namespace (`nspname = <app_id>`)
/// to avoid leaking cross-tenant metadata. The query joins
/// `pg_namespace -> pg_class -> pg_attribute / pg_index` and pulls
/// `pg_get_expr(adbin, adrelid)` for default expressions along with
/// `provolatile` for any function the default invokes.
pub(crate) async fn read_live_schema(pool: &Pool, app_id: &str) -> Result<LiveSchema, SchemaError> {
    let mut out = LiveSchema::default();
    let app_param = app_id.to_string();
    let params: Vec<&str> = vec![app_param.as_str()];

    // ----- columns -----
    //
    // The LEFT JOIN against `pg_description` pulls the
    // per-column comment populated by the
    // `COMMENT ON COLUMN <coll>.<sibling> IS 'zero-migrate:mask:...'`
    // statements the DDL emitter writes alongside CREATE TABLE +
    // ALTER ADD COLUMN. We hand the raw description string back as
    // `pg_comment`; the second pass below parses sentinel-tagged
    // sibling columns and back-attaches a `MaskMeta` onto the parent.
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
    let rows = pool
        .query_text_params(col_sql, &params)
        .await
        .map_err(|e| coded_sql("read columns failed", e))?;
    // Collect siblings + their sentinel strings here, then in a
    // second pass attach `MaskMeta` to the parent column entries.
    // Two passes because the parent column may appear before or
    // after the sibling in the `ORDER BY a.attnum` walk depending on
    // whether the column was added at create-time or via an
    // `ALTER ADD COLUMN` after the parent.
    let mut sibling_sentinels: std::collections::HashMap<(String, String), String> =
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
        // Column comments carry TWO sentinel families:
        //   - `zero-migrate:mask:…` on a `<col>_masked` sibling → deferred to the second
        //     pass (stamps `MaskMeta` on the PARENT);
        //   - `zero-migrate:enc:…` on the encrypted column itself → parsed inline here into
        //     `EncryptionMeta`. On PG the inline `/* zero-migrate:enc */` DDL comment is
        //     parse-discarded, so the migration emitter also writes a
        //     `COMMENT ON COLUMN` carrying the `zero-migrate:enc:` body; this is where the
        //     data plane (and the diff) recover it.
        let mut encryption: Option<EncryptionMeta> = None;
        if let Some(comment) = &pg_comment {
            // The mask sentinel rides the MASKED column, which after the
            // storage flip is the field's OWN column - so there is no suffix
            // to test and no parent to resolve.
            //
            // The `&& column.ends_with("_masked")` conjunct that used to be
            // here had no `else`: a `zero-migrate:mask:` comment on a column that did
            // not match fell through both arms and was discarded in silence,
            // while the malformed-sentinel arm below warns loudly. After the
            // flip that arm would have matched EVERY sentinel.
            if comment.starts_with(zeroship_data_sql::mask_codec::MASK_SENTINEL_PREFIX) {
                sibling_sentinels.insert((table.clone(), column.clone()), comment.clone());
            } else if comment.starts_with(zeroship_data_sql::mask_codec::ENC_SENTINEL_PREFIX) {
                match zeroship_data_sql::mask_codec::parse_encryption_sentinel(comment) {
                    Ok(meta) => encryption = Some(meta),
                    Err(e) => {
                        // A malformed encryption sentinel is treated like a
                        // malformed mask sentinel: warn loudly and treat the
                        // column as unencrypted rather than failing the whole
                        // introspection. The data plane then fails closed at the
                        // codec boundary (plaintext expected on a column the
                        // schema declared encrypted) rather than silently
                        // decrypting with a guessed mode.
                        tracing::warn!(
                            table = %table,
                            column = %column,
                            comment = %comment,
                            error = %e,
                            "diff: malformed zero-migrate:enc sentinel on PG column; \
                             treating column as unencrypted"
                        );
                    }
                }
            }
        }
        out.tables.entry(table).or_default().insert(
            column,
            ColumnInfo {
                pg_type,
                not_null,
                default_expr,
                default_volatility,
                encryption,
                // Remaining fields default; vector/geo are populated
                // from `information_schema` + `pg_indexes` introspection.
                ..Default::default()
            },
        );
    }
    // Second pass: for every column carrying a `zero-migrate:mask:…` sentinel, parse
    // the kind+classification and stamp `MaskMeta` on it. The column carrying
    // the sentinel IS the declared field - the mask lives under the field's own
    // name - so there is nothing to resolve. The diff classifier reads
    // `col.mask` to decide whether to emit a backfill, rewrite, or removal op.
    for ((table, masked_column), sentinel) in sibling_sentinels {
        let Some(table_cols) = out.tables.get_mut(&table) else {
            continue;
        };
        let Some(parent_col) = table_cols.get_mut(&masked_column) else {
            tracing::warn!(
                table = %table,
                column = %masked_column,
                "diff: mask sentinel on a column the introspection did not \
                 return; ignoring",
            );
            continue;
        };
        let sibling = masked_column;
        let (kind, classification) =
            match zeroship_data_sql::mask_codec::parse_mask_sentinel(&sentinel) {
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
                        column = %sibling,
                        sentinel = %sentinel,
                        error = %e,
                        "diff: malformed mask sentinel on a PG column; \
                         treating it as unmasked"
                    );
                    continue;
                }
            };
        parent_col.mask = Some(MaskMeta {
            kind,
            classification,
            // The OTHER column of the pair: the one holding the real value.
            sibling_column: zeroship_data_sql::compile::raw_column_name(&sibling),
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
    let rows = pool
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
    let rows = pool
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
