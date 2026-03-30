use deno_core::op2;
use deno_core::OpState;
use rusqlite::Connection;
use serde_json::Value;
use std::rc::Rc;
use uuid::Uuid;

#[derive(Debug, thiserror::Error, deno_error::JsError)]
#[class(generic)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[op2(fast)]
pub fn op_db_ensure_table(state: &mut OpState, #[string] name: &str) -> Result<(), DbError> {
    let db = state.borrow::<Rc<Connection>>().clone();
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
#[string]
pub fn op_db_insert(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] doc_json: &str,
) -> Result<String, DbError> {
    let db = state.borrow::<Rc<Connection>>().clone();

    let id = Uuid::new_v4().to_string();
    let mut doc: Value = serde_json::from_str(doc_json)?;
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.clone()));
    }

    let data = serde_json::to_string(&doc)?;
    db.execute(
        &format!(r#"INSERT INTO "{collection}" (id, data) VALUES (?1, ?2)"#),
        rusqlite::params![id, data],
    )?;

    Ok(data)
}

#[op2]
#[string]
pub fn op_db_find(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] filter_json: &str,
) -> Result<String, DbError> {
    let db = state.borrow::<Rc<Connection>>().clone();

    let mut stmt = db.prepare(&format!(r#"SELECT data FROM "{collection}""#))?;
    let rows: Vec<Value> = stmt
        .query_map([], |row| {
            let data: String = row.get(0)?;
            Ok(data)
        })?
        .filter_map(|r| r.ok())
        .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
        .collect();

    let filter: Value = serde_json::from_str(filter_json)?;
    let results: Vec<&Value> = if filter.as_object().is_none_or(|o| o.is_empty()) {
        rows.iter().collect()
    } else {
        rows.iter()
            .filter(|doc| {
                filter
                    .as_object()
                    .unwrap()
                    .iter()
                    .all(|(k, v)| doc.get(k).is_some_and(|dv| dv == v))
            })
            .collect()
    };

    Ok(serde_json::to_string(&results)?)
}

#[op2]
#[string]
pub fn op_db_update(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] id: &str,
    #[string] updates_json: &str,
) -> Result<String, DbError> {
    let db = state.borrow::<Rc<Connection>>().clone();

    let existing: String = db.query_row(
        &format!(r#"SELECT data FROM "{collection}" WHERE id = ?1"#),
        rusqlite::params![id],
        |row| row.get(0),
    )?;

    let mut doc: Value = serde_json::from_str(&existing)?;
    let updates: Value = serde_json::from_str(updates_json)?;

    if let (Some(doc_obj), Some(upd_obj)) = (doc.as_object_mut(), updates.as_object()) {
        for (k, v) in upd_obj {
            doc_obj.insert(k.clone(), v.clone());
        }
    }

    let data = serde_json::to_string(&doc)?;
    db.execute(
        &format!(r#"UPDATE "{collection}" SET data = ?1, updated_at = datetime('now') WHERE id = ?2"#),
        rusqlite::params![data, id],
    )?;

    Ok(data)
}

#[op2(fast)]
pub fn op_db_delete(
    state: &mut OpState,
    #[string] collection: &str,
    #[string] id: &str,
) -> Result<(), DbError> {
    let db = state.borrow::<Rc<Connection>>().clone();
    db.execute(
        &format!(r#"DELETE FROM "{collection}" WHERE id = ?1"#),
        rusqlite::params![id],
    )?;
    Ok(())
}
