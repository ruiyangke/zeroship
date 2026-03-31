use appbase_core::plugin::{Aggregation, MeterResource, Plugin, PluginContext, PluginMeter};
use deno_core::op2;
use deno_core::{OpDecl, OpState};
use rusqlite::Connection;
use serde_json;
use std::rc::Rc;
use std::sync::Arc;
use uuid::Uuid;

/// SQLite-backed document database plugin.
///
/// In multi-tenant mode, each app gets its own database file
/// at `{data_dir}/db.sqlite` (data_dir is set by PluginContext).
/// In single-app mode, use `DbPlugin::with_path("appbase.db")` for a fixed path.
pub struct DbPlugin {
    /// If set, overrides the default `{data_dir}/db.sqlite` path.
    fixed_path: Option<String>,
}

impl DbPlugin {
    /// Create a db plugin that uses `{data_dir}/db.sqlite` per app.
    pub fn new() -> Self {
        Self { fixed_path: None }
    }

    /// Create a db plugin with a fixed database path (single-app mode).
    pub fn with_path(path: &str) -> Self {
        Self {
            fixed_path: Some(path.to_string()),
        }
    }
}

impl Plugin for DbPlugin {
    fn name(&self) -> &str {
        "db"
    }

    fn ops(&self) -> Vec<OpDecl> {
        vec![
            op_db_ensure_table(),
            op_db_insert(),
            op_db_find(),
            op_db_update(),
            op_db_delete(),
        ]
    }

    fn js_bridge(&self) -> &str {
        r#"
globalThis.db = {
  collection(name) {
    Deno.core.ops.op_db_ensure_table(name);
    return {
      insert: (doc) => Deno.core.ops.op_db_insert(name, doc),
      find: (filter) => Deno.core.ops.op_db_find(name, filter || {}),
      update: (id, updates) => Deno.core.ops.op_db_update(name, id, updates),
      delete: (id) => Deno.core.ops.op_db_delete(name, id),
    };
  },
};
"#
    }

    fn init(&self, ctx: &mut PluginContext<'_>) {
        let db_path = self
            .fixed_path
            .clone()
            .unwrap_or_else(|| ctx.data_dir.join("db.sqlite").to_string_lossy().to_string());

        if let Some(parent) = std::path::Path::new(&db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Rc::new(
            Connection::open(&db_path)
                .unwrap_or_else(|e| panic!("Failed to open db {db_path}: {e}")),
        );
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        ctx.op_state.put(conn);
        ctx.op_state.put(DbMeter(ctx.meter.clone()));
    }

    fn meter_resources(&self) -> Vec<MeterResource> {
        vec![
            MeterResource {
                name: "db.reads".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
            MeterResource {
                name: "db.writes".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
        ]
    }
}

/// Newtype wrapper around the meter to avoid OpState type collision.
struct DbMeter(Arc<dyn PluginMeter>);

#[derive(Debug, thiserror::Error, deno_error::JsError)]
#[class(generic)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid collection name: {0}")]
    InvalidName(String),
}

fn validate_collection_name(name: &str) -> Result<(), DbError> {
    if name.is_empty() || name.len() > 64 {
        return Err(DbError::InvalidName("must be 1-64 characters".into()));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(DbError::InvalidName(
            "only alphanumeric and underscore allowed".into(),
        ));
    }
    Ok(())
}

#[op2(fast)]
pub fn op_db_ensure_table(state: &mut OpState, #[string] name: &str) -> Result<(), DbError> {
    validate_collection_name(name)?;
    let db = state.borrow::<Rc<Connection>>().clone();
    state.borrow::<DbMeter>().0.increment("db.writes", 1);
    db.execute(
        &format!(
            r#"CREATE TABLE IF NOT EXISTS "{name}" (
                id TEXT PRIMARY KEY,
                data JSON NOT NULL,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now'))
            )"#
        ),
        [],
    )?;
    Ok(())
}

#[op2]
#[serde]
pub fn op_db_insert(
    state: &mut OpState,
    #[string] collection: &str,
    #[serde] mut doc: serde_json::Value,
) -> Result<serde_json::Value, DbError> {
    validate_collection_name(collection)?;
    let db = state.borrow::<Rc<Connection>>().clone();
    state.borrow::<DbMeter>().0.increment("db.writes", 1);

    let id = Uuid::new_v4().to_string();
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("id".to_string(), serde_json::Value::String(id.clone()));
    }

    let data = serde_json::to_string(&doc)?;
    db.execute(
        &format!(r#"INSERT INTO "{collection}" (id, data) VALUES (?1, ?2)"#),
        rusqlite::params![id, data],
    )?;

    Ok(doc)
}

#[op2]
#[serde]
pub fn op_db_find(
    state: &mut OpState,
    #[string] collection: &str,
    #[serde] filter: serde_json::Value,
) -> Result<Vec<serde_json::Value>, DbError> {
    validate_collection_name(collection)?;
    let db = state.borrow::<Rc<Connection>>().clone();
    state.borrow::<DbMeter>().0.increment("db.reads", 1);

    let mut stmt = db.prepare(&format!(r#"SELECT data FROM "{collection}""#))?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            let data: String = row.get(0)?;
            Ok(data)
        })?
        .filter_map(|r| r.ok())
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
        .collect();

    if filter.as_object().is_none_or(|o| o.is_empty()) {
        Ok(rows)
    } else {
        let filter_obj = filter.as_object().unwrap();
        Ok(rows
            .into_iter()
            .filter(|doc| {
                filter_obj
                    .iter()
                    .all(|(k, v)| doc.get(k).is_some_and(|dv| dv == v))
            })
            .collect())
    }
}

#[op2]
#[serde]
pub fn op_db_update(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] id: &str,
    #[serde] updates: serde_json::Value,
) -> Result<serde_json::Value, DbError> {
    validate_collection_name(collection)?;
    let db = state.borrow::<Rc<Connection>>().clone();
    state.borrow::<DbMeter>().0.increment("db.reads", 1);
    state.borrow::<DbMeter>().0.increment("db.writes", 1);

    let existing: String = db.query_row(
        &format!(r#"SELECT data FROM "{collection}" WHERE id = ?1"#),
        rusqlite::params![id],
        |row| row.get(0),
    )?;

    let mut doc: serde_json::Value = serde_json::from_str(&existing)?;
    if let (Some(doc_obj), Some(upd_obj)) = (doc.as_object_mut(), updates.as_object()) {
        for (k, v) in upd_obj {
            doc_obj.insert(k.clone(), v.clone());
        }
    }

    let data = serde_json::to_string(&doc)?;
    db.execute(
        &format!(
            r#"UPDATE "{collection}" SET data = ?1, updated_at = datetime('now') WHERE id = ?2"#
        ),
        rusqlite::params![data, id],
    )?;

    Ok(doc)
}

#[op2(fast)]
pub fn op_db_delete(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] id: &str,
) -> Result<(), DbError> {
    validate_collection_name(collection)?;
    let db = state.borrow::<Rc<Connection>>().clone();
    state.borrow::<DbMeter>().0.increment("db.writes", 1);
    db.execute(
        &format!(r#"DELETE FROM "{collection}" WHERE id = ?1"#),
        rusqlite::params![id],
    )?;
    Ok(())
}
