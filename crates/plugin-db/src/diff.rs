//! Schema diff engine — A2 of the @zeroship/db v2 proposal
//! (docs/proposals/zeroship-db-v2.md, section A2).
//!
//! Compares a desired (declared) schema against the live `pg_catalog`
//! state and classifies each change into **additive** (auto-apply),
//! **compatible** (auto-apply, may need a validation backfill), or
//! **destructive** (refused; surfaces a `validation_refused` envelope).
//!
//! The engine intentionally does not run any DDL on its own — it returns
//! a `Vec<DiffOp>` that the orchestrator in `callbacks::exec_register_model`
//! then sequences with the advisory lock, audit writes, and validation
//! pass.
//!
//! ## Volatile-default trap
//!
//! Proposal A2 line 120: Postgres' fast-path `ALTER TABLE ADD COLUMN
//! NOT NULL DEFAULT 'literal'` is metadata-only, but a volatile or
//! stable default (`DEFAULT NOW()`, `DEFAULT gen_random_uuid()`) forces
//! a full table rewrite under `ACCESS EXCLUSIVE`. The classifier inspects
//! `pg_get_expr` + `pg_proc.provolatile`: only `'i'` (immutable) takes the
//! fast path; `'v'` and `'s'` escalate to destructive. For now we apply a
//! literal-only heuristic on declared defaults (we never emit a volatile
//! default — `default()` values are JS literals) and surface the
//! introspection scaffolding so a future change can plug a real
//! pg_get_expr inspection in.

use compio_postgres::Pool;
use serde_json::Value;

/// Classification per the proposal A2 three-bucket split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeClass {
    /// Add column nullable, add index, relax constraint — auto-apply.
    Additive,
    /// Add column with constant default, type widening, add unique
    /// after validation passes — auto-apply with backfill if needed.
    Compatible,
    /// Drop column, type narrowing, tighten constraint, drop index,
    /// add NOT NULL to non-empty table, add column with volatile default
    /// — refused; user-approval workflow.
    Destructive,
}

impl ChangeClass {
    /// Convert to the audit row enum value.
    pub fn as_audit(self) -> crate::audit::ChangeClass {
        match self {
            Self::Additive => crate::audit::ChangeClass::Additive,
            Self::Compatible => crate::audit::ChangeClass::Compatible,
            Self::Destructive => crate::audit::ChangeClass::Destructive,
        }
    }
}

/// A single planned change with the SQL to apply, the classification,
/// and the metadata needed for the audit row.
#[derive(Debug, Clone)]
pub struct DiffOp {
    /// The user-visible collection (table) name this change affects.
    pub collection: String,
    /// The change kind — written to `__zeroship_migrations.change_kind`.
    pub change_kind: ChangeKind,
    /// Classification, drives apply / refuse / backfill routing.
    pub class: ChangeClass,
    /// SQL to run when the op is applied. `None` for destructive ops
    /// that are surfaced via the error envelope without being executed.
    pub sql: Option<String>,
    /// Structured metadata — copied into `details` JSONB.
    pub details: Value,
    /// Field name involved, if any. Used by validation pass.
    pub field: Option<String>,
}

/// Specific change kinds the diff engine recognises.
#[derive(Debug, Clone)]
pub enum ChangeKind {
    /// Brand-new table — emitted when the table is absent from the live
    /// snapshot. Carries the full `CREATE TABLE` body for application.
    CreateTable,
    /// `ALTER TABLE … ADD COLUMN` for a field that doesn't exist yet.
    AddColumn,
    /// `ALTER TABLE … DROP COLUMN` — destructive, never auto-applied.
    DropColumn,
    /// `CREATE INDEX CONCURRENTLY` for a new index marker.
    AddIndex,
    /// `DROP INDEX` for an index no longer in the declared schema.
    DropIndex,
    /// B2 — `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`. Emitted when a
    /// column already exists but no FK constraint is attached, or when
    /// the table was created with FK emission deferred (cross-table
    /// declaration order).
    AddForeignKey,
    /// B2 — `ALTER TABLE … DROP CONSTRAINT` for a FK no longer declared.
    DropForeignKey,
}

impl ChangeKind {
    /// Stringly representation written to `change_kind` column. Matches
    /// the values listed in proposal A3 section.
    pub fn as_sql(&self) -> &'static str {
        match self {
            Self::CreateTable => "create_table",
            Self::AddColumn => "add_column",
            Self::DropColumn => "drop_column",
            Self::AddIndex => "add_index",
            Self::DropIndex => "drop_index",
            Self::AddForeignKey => "add_foreign_key",
            Self::DropForeignKey => "drop_foreign_key",
        }
    }
}

/// Snapshot of the live schema as introspected from `pg_catalog`. Only
/// the fields we currently consult are populated; this is a small struct
/// because the diff classifier is fundamentally a join between declared
/// fields and the live column / index sets.
#[derive(Debug, Default)]
pub struct LiveSchema {
    /// Per-table live column set: `tables[<table>][<column>] = ColumnInfo`.
    pub tables: std::collections::HashMap<String, std::collections::HashMap<String, ColumnInfo>>,
    /// Per-table live index set: `indexes[<table>][<index_name>] = IndexInfo`.
    pub indexes: std::collections::HashMap<String, std::collections::HashMap<String, IndexInfo>>,
    /// Per-table row count (used by the validation budget to decide
    /// fast-path additive vs. compatible paths). 0 means empty, which
    /// is sound for the "ADD NOT NULL on empty table is safe" rule.
    pub row_counts: std::collections::HashMap<String, i64>,
    /// B2 — per-table foreign-key set, keyed by the local column name.
    /// `foreign_keys[<table>][<column>] = ForeignKeyInfo`.
    pub foreign_keys:
        std::collections::HashMap<String, std::collections::HashMap<String, ForeignKeyInfo>>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub pg_type: String,
    pub not_null: bool,
    pub default_expr: Option<String>,
    /// `pg_proc.provolatile` for the default expression's function, if
    /// the default is a function call. `i`/`s`/`v`. `None` if the default
    /// is a plain literal.
    pub default_volatility: Option<char>,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub is_unique: bool,
    pub columns: Vec<String>,
    /// Whether `pg_index.indisvalid` is true. An INVALID index means a
    /// prior CREATE INDEX CONCURRENTLY failed; the diff engine flags it
    /// for retry.
    pub is_valid: bool,
}

/// B2 — observed FK constraint read from `pg_constraint`.
#[derive(Debug, Clone)]
pub struct ForeignKeyInfo {
    /// Postgres constraint name (e.g. `"author_id_fkey"`).
    pub constraint_name: String,
    /// Local column the FK is attached to.
    pub column: String,
    /// Referenced table name (relative to the same app schema).
    pub target_table: String,
    /// Referenced column on the target table — typically `id`.
    pub target_column: String,
    /// ON DELETE policy in upper-case Postgres form (`RESTRICT`,
    /// `CASCADE`, `SET NULL`, `NO ACTION`).
    pub on_delete: String,
    /// ON UPDATE policy.
    pub on_update: String,
    /// True if the constraint is `DEFERRABLE` (any timing).
    pub deferrable: bool,
}

/// Introspect the live schema for the given app + collection. Returns an
/// empty `LiveSchema` if the schema itself doesn't exist yet (first
/// deploy).
///
/// We restrict the catalog scan to the app's namespace (`nspname = <app_id>`)
/// to avoid leaking cross-tenant metadata. The query joins
/// `pg_namespace -> pg_class -> pg_attribute / pg_index` and pulls
/// `pg_get_expr(adbin, adrelid)` for default expressions along with
/// `provolatile` for any function the default invokes.
pub async fn read_live_schema(pool: &Pool, app_id: &str) -> Result<LiveSchema, String> {
    let mut out = LiveSchema::default();
    let app_param = app_id.to_string();
    let params: Vec<&str> = vec![app_param.as_str()];

    // ----- columns -----
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
           AND d.refclassid = 'pg_proc'::regclass) AS default_volatility
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
 WHERE n.nspname = $1
   AND c.relkind = 'r'
   AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY c.relname, a.attnum
"#;
    let rows = pool
        .query_text_params(col_sql, &params)
        .await
        .map_err(|e| format!("diff: read columns failed: {e}"))?;
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
        out.tables.entry(table).or_default().insert(
            column,
            ColumnInfo {
                pg_type,
                not_null,
                default_expr,
                default_volatility,
            },
        );
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
        .map_err(|e| format!("diff: read foreign_keys failed: {e}"))?;
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
        .map_err(|e| format!("diff: read indexes failed: {e}"))?;
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

/// Decode Postgres' single-character FK action code into the SQL
/// keyword equivalent. The pg_constraint columns confdeltype and
/// confupdtype use these codes (per the Postgres source: `gram.y`).
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

/// Cheap row-count check used by the classifier. Returns 0 if the table
/// doesn't exist. The query uses `pg_class.reltuples` for non-blocking
/// estimation when the table is large; the cold-start orchestrator
/// already holds the advisory lock so an estimate is good enough for the
/// "empty vs. non-empty" decision.
pub async fn estimate_row_count(pool: &Pool, app_id: &str, collection: &str) -> Result<i64, String> {
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
        .map_err(|e| format!("diff: estimate_row_count failed: {e}"))?;
    let n: i64 = rows.first().map(|r| r.get::<_, i64>("rows")).unwrap_or(0);
    Ok(n)
}

/// Exact row-count (used by the validation pass when row_count > 0 and
/// we need the precise number for the error envelope). Pricier than
/// `estimate_row_count` but only invoked when classification has already
/// established that a violation check is mandatory.
pub async fn count_violating_not_null(
    pool: &Pool,
    app_id: &str,
    collection: &str,
    field: &str,
) -> Result<(i64, Vec<i64>), String> {
    let sql = format!(
        r#"SELECT id FROM "{app_id}"."{collection}" WHERE "{field}" IS NULL LIMIT 5"#
    );
    let empty: Vec<&str> = Vec::new();
    let sample_rows = pool
        .query_text_params(&sql, &empty)
        .await
        .map_err(|e| format!("diff: count_violating_not_null sample failed: {e}"))?;
    let samples: Vec<i64> = sample_rows
        .iter()
        .filter_map(|r| r.try_get::<_, i32>("id").ok().map(i64::from))
        .collect();

    let count_sql = format!(
        r#"SELECT COUNT(*) AS n FROM "{app_id}"."{collection}" WHERE "{field}" IS NULL"#
    );
    let cnt_rows = pool
        .query_text_params(&count_sql, &empty)
        .await
        .map_err(|e| format!("diff: count_violating_not_null count failed: {e}"))?;
    let n: i64 = cnt_rows.first().map(|r| r.get::<_, i64>("n")).unwrap_or(0);
    Ok((n, samples))
}

/// Compute the diff between the declared schema and the live snapshot.
///
/// `schema` is the JS-side schema object as already passed to
/// `build_create_table` (`{ field: { type, required, ... } }`).
///
/// The first deploy (`live.tables` doesn't contain `collection`) returns
/// a single [`ChangeKind::CreateTable`] op. Subsequent deploys diff
/// per-field against the live column set.
pub fn compute_diff(
    live: &LiveSchema,
    app_id: &str,
    collection: &str,
    schema: &Value,
    create_table_sql: &str,
    declared_indexes: &[crate::query::IndexSpec],
) -> Vec<DiffOp> {
    let mut ops = Vec::new();

    let live_cols = live.tables.get(collection);
    let live_indexes = live.indexes.get(collection);

    // ----- table create -----
    if live_cols.is_none() {
        ops.push(DiffOp {
            collection: collection.to_string(),
            change_kind: ChangeKind::CreateTable,
            class: ChangeClass::Additive,
            sql: Some(create_table_sql.to_string()),
            details: serde_json::json!({
                "kind": "create_table",
                "field_count": schema.as_object().map(serde_json::Map::len).unwrap_or(0)
            }),
            field: None,
        });
    }

    // ----- column additions -----
    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            let exists = live_cols.map(|c| c.contains_key(field)).unwrap_or(false);
            if exists {
                continue;
            }
            let class = classify_add_column(def, live, collection);
            // Build the ALTER. Note: build_add_column emits IF NOT EXISTS,
            // making the operation idempotent even if the live snapshot
            // is briefly stale.
            let sql = crate::query::build_add_column(app_id, collection, field, def)
                .ok()
                .map(|s| s.to_string());
            ops.push(DiffOp {
                collection: collection.to_string(),
                change_kind: ChangeKind::AddColumn,
                class,
                sql,
                details: serde_json::json!({
                    "kind": "add_column",
                    "field": field,
                    "declared_type": def.get("type").cloned().unwrap_or(Value::Null),
                    "required": def.get("required").cloned().unwrap_or(Value::Bool(false)),
                    "has_default": def.get("default").is_some(),
                }),
                field: Some(field.clone()),
            });
        }
    }

    // ----- index additions -----
    let live_idx_set = live_indexes.cloned().unwrap_or_default();
    for spec in declared_indexes {
        let exists = live_idx_set.contains_key(&spec.name);
        if exists {
            continue;
        }
        let class = if spec.unique && live.row_counts.get(collection).copied().unwrap_or(0) > 0 {
            // Adding a UNIQUE constraint to a non-empty table needs a
            // validation pass first.
            ChangeClass::Compatible
        } else {
            ChangeClass::Additive
        };
        ops.push(DiffOp {
            collection: collection.to_string(),
            change_kind: ChangeKind::AddIndex,
            class,
            sql: Some(spec.sql.clone()),
            details: serde_json::json!({
                "kind": "add_index",
                "index_name": spec.name,
                "columns": spec.columns,
                "unique": spec.unique,
            }),
            field: spec.columns.first().cloned(),
        });
    }

    // ----- foreign-key additions (B2) -----
    //
    // Walk the declared schema and emit an AddForeignKey op for every
    // `t.ref("target")` field that doesn't already have a matching FK
    // in the live snapshot.
    //
    // First-time CREATE TABLE inlines the FK in the same statement (see
    // build_create_table_with_fks's Deferred mode), so when the table is
    // brand new (`live_cols.is_none()`) we only emit AddForeignKey ops
    // for refs whose target *doesn't* exist yet — but currently
    // build_create_table emits FKs inline always. To stay safe and
    // explicit, we let the orchestrator decide: when the table already
    // exists, the FK might need to be attached; when it's a fresh
    // create_table, the FK is already part of the CREATE TABLE SQL and
    // we skip the standalone op.
    //
    // Classification:
    // - new column + new FK (ref field, table is being created):
    //   the FK is part of CREATE TABLE; no AddForeignKey op.
    // - existing column + new FK (was bare number, now t.ref): the FK
    //   needs ALTER TABLE ADD CONSTRAINT. Classification is **compatible**
    //   — we'd need to validate every existing row before turning the
    //   constraint on, but Postgres' `NOT VALID` + `VALIDATE` two-step
    //   makes this safe (deferred to follow-up; today we attempt the add
    //   and Postgres will refuse if data is bad).
    // - existing FK, declared no longer (field removed from schema):
    //   this is captured by the column-drop path; the FK drop is
    //   implicit. We surface it explicitly when the column stays but the
    //   ref marker is removed (rare: would require re-declaring as
    //   t.number()).
    let live_fk_set = live
        .foreign_keys
        .get(collection)
        .cloned()
        .unwrap_or_default();
    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            if def.get("type").and_then(|t| t.as_str()) != Some("ref") {
                continue;
            }
            let target = def
                .get("refTarget")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if target.is_empty() {
                continue;
            }

            let live_fk = live_fk_set.get(field);
            let column_exists = live_cols.map(|c| c.contains_key(field)).unwrap_or(false);

            if !column_exists {
                // The column itself doesn't exist yet.
                if live_cols.is_some() {
                    // Existing table — AddColumn already emitted above;
                    // emit AddForeignKey as a separate op so the
                    // orchestrator can run ALTER TABLE ADD CONSTRAINT
                    // after the column is created.
                    let sql = crate::query::build_add_foreign_key(app_id, collection, field, def)
                        .ok()
                        .map(|s| s.to_string());
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::AddForeignKey,
                        class: ChangeClass::Compatible,
                        sql,
                        details: serde_json::json!({
                            "kind": "add_foreign_key",
                            "field": field,
                            "target_table": target,
                            "target_column": "id",
                            "on_delete": def.get("onDelete").cloned().unwrap_or(Value::String("restrict".into())),
                            "on_update": def.get("onUpdate").cloned().unwrap_or(Value::String("restrict".into())),
                        }),
                        field: Some(field.clone()),
                    });
                }
                // First-time CREATE TABLE — FK is inlined in CREATE TABLE;
                // skip standalone op.
                continue;
            }

            // Column exists. Need to attach the FK if not present, or
            // detect a policy mismatch.
            if live_fk.is_none() {
                let sql = crate::query::build_add_foreign_key(app_id, collection, field, def)
                    .ok()
                    .map(|s| s.to_string());
                ops.push(DiffOp {
                    collection: collection.to_string(),
                    change_kind: ChangeKind::AddForeignKey,
                    class: ChangeClass::Compatible,
                    sql,
                    details: serde_json::json!({
                        "kind": "add_foreign_key",
                        "field": field,
                        "target_table": target,
                        "target_column": "id",
                        "on_delete": def.get("onDelete").cloned().unwrap_or(Value::String("restrict".into())),
                        "on_update": def.get("onUpdate").cloned().unwrap_or(Value::String("restrict".into())),
                    }),
                    field: Some(field.clone()),
                });
            } else if let Some(fk) = live_fk {
                // Detect policy mismatch — surfaced as paired DROP+ADD.
                let declared_on_delete = crate::query::normalize_fk_action_pub(
                    def.get("onDelete").and_then(|v| v.as_str()),
                );
                let declared_on_update = crate::query::normalize_fk_action_pub(
                    def.get("onUpdate").and_then(|v| v.as_str()),
                );
                let declared_target = target;
                if declared_on_delete != fk.on_delete
                    || declared_on_update != fk.on_update
                    || declared_target != fk.target_table
                {
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::DropForeignKey,
                        class: ChangeClass::Compatible,
                        sql: crate::query::build_drop_foreign_key(app_id, collection, &fk.constraint_name).ok(),
                        details: serde_json::json!({
                            "kind": "drop_foreign_key",
                            "field": field,
                            "constraint_name": fk.constraint_name,
                        }),
                        field: Some(field.clone()),
                    });
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::AddForeignKey,
                        class: ChangeClass::Compatible,
                        sql: crate::query::build_add_foreign_key(app_id, collection, field, def).ok(),
                        details: serde_json::json!({
                            "kind": "add_foreign_key",
                            "field": field,
                            "target_table": target,
                            "target_column": "id",
                            "on_delete": declared_on_delete,
                            "on_update": declared_on_update,
                        }),
                        field: Some(field.clone()),
                    });
                }
            }
        }
    }

    // ----- column drops (destructive) -----
    if let Some(live_cols) = live_cols {
        let declared_set: std::collections::HashSet<&String> = schema
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        for col in live_cols.keys() {
            // System columns and auto-generated platform columns are
            // never declared in the user schema — skip them.
            if matches!(col.as_str(), "id" | "created_at" | "updated_at") {
                continue;
            }
            if declared_set.contains(col) {
                continue;
            }
            ops.push(DiffOp {
                collection: collection.to_string(),
                change_kind: ChangeKind::DropColumn,
                class: ChangeClass::Destructive,
                sql: None,
                details: serde_json::json!({
                    "kind": "drop_column",
                    "field": col,
                }),
                field: Some(col.clone()),
            });
        }
    }

    ops
}

/// Classify an `ADD COLUMN` change. Inputs:
/// - Adding a nullable column → additive.
/// - Adding a NOT NULL column to an empty table → additive (Postgres
///   accepts it).
/// - Adding a NOT NULL column to a non-empty table without a default →
///   destructive.
/// - Adding a NOT NULL column to a non-empty table with an *immutable*
///   default → compatible (Postgres fast-path).
/// - Adding a column with a volatile/stable default → destructive
///   (forces table rewrite under ACCESS EXCLUSIVE — proposal A2 line 120).
fn classify_add_column(def: &Value, live: &LiveSchema, collection: &str) -> ChangeClass {
    let required = def
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_default = def.get("default").is_some();
    let is_empty = live.row_counts.get(collection).copied().unwrap_or(0) == 0;

    if !required {
        return ChangeClass::Additive;
    }

    if is_empty {
        return ChangeClass::Additive;
    }

    if !has_default {
        return ChangeClass::Destructive;
    }

    // Has default + required + non-empty table. The SDK only emits
    // JSON-literal defaults (string / number / boolean / object / array),
    // so we treat declared defaults as immutable. A volatile default
    // (`DEFAULT NOW()`) could only sneak in via a future SDK escape hatch
    // that emitted raw SQL; document this assumption.
    ChangeClass::Compatible
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_with_rows(coll: &str, n: i64) -> LiveSchema {
        let mut live = LiveSchema::default();
        live.row_counts.insert(coll.to_string(), n);
        live
    }

    #[test]
    fn nullable_add_is_additive() {
        let def = json!({"type": "string"});
        let live = live_with_rows("users", 1000);
        assert_eq!(classify_add_column(&def, &live, "users"), ChangeClass::Additive);
    }

    #[test]
    fn required_add_on_empty_table_is_additive() {
        let def = json!({"type": "string", "required": true});
        let live = live_with_rows("users", 0);
        assert_eq!(classify_add_column(&def, &live, "users"), ChangeClass::Additive);
    }

    #[test]
    fn required_no_default_on_non_empty_is_destructive() {
        let def = json!({"type": "string", "required": true});
        let live = live_with_rows("users", 1000);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Destructive
        );
    }

    #[test]
    fn required_with_default_on_non_empty_is_compatible() {
        let def = json!({"type": "string", "required": true, "default": "guest"});
        let live = live_with_rows("users", 1000);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Compatible
        );
    }

    #[test]
    fn drop_column_is_destructive() {
        // Live has `legacy_score`, declared schema does not.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
            },
        );
        cols.insert(
            "legacy_score".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        cols.insert(
            "created_at".to_string(),
            ColumnInfo {
                pg_type: "timestamptz".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        cols.insert(
            "updated_at".to_string(),
            ColumnInfo {
                pg_type: "timestamptz".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let declared = json!({}); // Empty declared schema — drop everything user-side
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        // We should see exactly one DropColumn op for legacy_score.
        let drops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::DropColumn))
            .collect();
        assert_eq!(drops.len(), 1, "ops: {ops:?}");
        assert_eq!(drops[0].class, ChangeClass::Destructive);
        assert_eq!(drops[0].field.as_deref(), Some("legacy_score"));
    }

    #[test]
    fn create_table_emitted_when_live_empty() {
        let live = LiveSchema::default();
        let declared = json!({"name": {"type": "string"}});
        let ops = compute_diff(&live, "app1", "fresh", &declared, "CREATE TABLE ...", &[]);
        let creates: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::CreateTable))
            .collect();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].class, ChangeClass::Additive);
    }

    // -----------------------------------------------------------------
    // B2 — typed cross-table relations: diff classification
    // -----------------------------------------------------------------

    #[test]
    fn b2_add_fk_to_existing_column_is_compatible() {
        // Existing table with a bare INTEGER column — now declared with
        // t.ref("users"). The FK must be added as a separate op.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
            },
        );
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        let fk_ops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddForeignKey))
            .collect();
        assert_eq!(fk_ops.len(), 1, "ops: {ops:?}");
        assert_eq!(fk_ops[0].class, ChangeClass::Compatible);
        assert_eq!(fk_ops[0].field.as_deref(), Some("authorId"));
    }

    #[test]
    fn b2_existing_fk_no_change_is_no_op() {
        // Live FK matches declared: no op emitted.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
            },
        );
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let mut fks = std::collections::HashMap::new();
        fks.insert(
            "authorId".to_string(),
            ForeignKeyInfo {
                constraint_name: "authorId_fkey".into(),
                column: "authorId".into(),
                target_table: "users".into(),
                target_column: "id".into(),
                on_delete: "RESTRICT".into(),
                on_update: "RESTRICT".into(),
                deferrable: true,
            },
        );
        live.foreign_keys.insert("posts".to_string(), fks);

        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        assert!(
            !ops.iter().any(|o| matches!(o.change_kind, ChangeKind::AddForeignKey | ChangeKind::DropForeignKey)),
            "should not emit FK ops when policies match: {ops:?}"
        );
    }

    #[test]
    fn b2_policy_change_emits_drop_then_add() {
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let mut fks = std::collections::HashMap::new();
        fks.insert(
            "authorId".to_string(),
            ForeignKeyInfo {
                constraint_name: "authorId_fkey".into(),
                column: "authorId".into(),
                target_table: "users".into(),
                target_column: "id".into(),
                on_delete: "RESTRICT".into(),
                on_update: "RESTRICT".into(),
                deferrable: true,
            },
        );
        live.foreign_keys.insert("posts".to_string(), fks);

        // Declared now wants ON DELETE CASCADE.
        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users", "onDelete": "cascade"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        let drops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::DropForeignKey))
            .collect();
        let adds: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddForeignKey))
            .collect();
        assert_eq!(drops.len(), 1, "expected drop op: {ops:?}");
        assert_eq!(adds.len(), 1, "expected add op: {ops:?}");
    }
}
