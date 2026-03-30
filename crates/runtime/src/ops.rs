use deno_core::op2;
use deno_core::OpState;
use rusqlite::Connection;
use serde_json;
use std::rc::Rc;
use uuid::Uuid;

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
    db.execute(
        &format!(r#"DELETE FROM "{collection}" WHERE id = ?1"#),
        rusqlite::params![id],
    )?;
    Ok(())
}
