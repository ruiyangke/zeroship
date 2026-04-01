//! SqliteRegistry — AppRegistry backed by SQLite via rusqlite.
//!
//! Uses `tokio::task::spawn_blocking` to bridge sync rusqlite with async trait.
//! For Postgres support, migrate to sqlx when libsqlite3-sys conflict is resolved.

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

use crate::{AppData, AppRecord, AppRegistry, RegistryError};

/// AppRegistry implementation backed by SQLite.
pub struct SqliteRegistry {
    conn: Arc<Mutex<Connection>>,
    master_key: String,
}

impl SqliteRegistry {
    /// Open (or create) the registry database.
    pub fn new(path: &str, master_key: String) -> Result<Self, RegistryError> {
        let conn = Connection::open(path)
            .map_err(|e| RegistryError::Database(format!("Failed to open: {e}")))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS apps (
                 id          TEXT PRIMARY KEY,
                 plan_id     TEXT NOT NULL DEFAULT 'free',
                 server_js   TEXT NOT NULL DEFAULT '',
                 client_html BLOB,
                 version     INTEGER NOT NULL DEFAULT 0,
                 api_key     TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                 updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .map_err(|e| RegistryError::Database(format!("Migration failed: {e}")))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            master_key,
        })
    }

    /// Validate the master key for admin operations.
    pub fn check_master_key(&self, key: &str) -> bool {
        self.master_key == key
    }

    /// Run a blocking database operation on tokio's blocking thread pool.
    async fn blocking<F, T>(&self, f: F) -> Result<T, RegistryError>
    where
        F: FnOnce(&Connection) -> Result<T, RegistryError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            f(&conn)
        })
        .await
        .map_err(|e| RegistryError::Database(format!("Task join error: {e}")))?
    }
}

#[async_trait]
impl AppRegistry for SqliteRegistry {
    async fn get_app(&self, app_id: &str) -> Result<Option<AppData>, RegistryError> {
        let id = app_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT id, plan_id, server_js, client_html, version FROM apps WHERE id = ?1",
                params![id],
                |row| {
                    Ok(AppData {
                        id: row.get(0)?,
                        plan_id: row.get(1)?,
                        server_js: row.get(2)?,
                        client_html: row.get(3)?,
                        version: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(|e| RegistryError::Database(e.to_string()))
        })
        .await
    }

    async fn get_version(&self, app_id: &str) -> Result<Option<i64>, RegistryError> {
        let id = app_id.to_string();
        self.blocking(move |conn| {
            conn.query_row("SELECT version FROM apps WHERE id = ?1", params![id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|e| RegistryError::Database(e.to_string()))
        })
        .await
    }

    async fn get_plan(&self, app_id: &str) -> Result<String, RegistryError> {
        let id = app_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT plan_id FROM apps WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| RegistryError::Database(e.to_string()))?
            .ok_or_else(|| RegistryError::NotFound(id))
        })
        .await
    }

    async fn create_app(&self, app_id: &str, plan_id: &str) -> Result<AppRecord, RegistryError> {
        let id = app_id.to_string();
        let plan = plan_id.to_string();

        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RegistryError::InvalidInput(
                "app_id must be 1-64 alphanumeric/hyphen/underscore characters".to_string(),
            ));
        }

        let api_key = uuid::Uuid::new_v4().to_string();
        let key = api_key.clone();

        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO apps (id, plan_id, api_key) VALUES (?1, ?2, ?3)",
                params![id, plan, key],
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("UNIQUE") {
                    RegistryError::AlreadyExists(id.clone())
                } else {
                    RegistryError::Database(msg)
                }
            })?;

            conn.query_row(
                "SELECT id, plan_id, version, api_key, created_at, updated_at FROM apps WHERE id = ?1",
                params![id],
                |row| {
                    Ok(AppRecord {
                        id: row.get(0)?,
                        plan_id: row.get(1)?,
                        version: row.get(2)?,
                        api_key: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .map_err(|e| RegistryError::Database(e.to_string()))
        })
        .await
    }

    async fn deploy(
        &self,
        app_id: &str,
        server_js: &str,
        client_html: Option<&[u8]>,
    ) -> Result<i64, RegistryError> {
        let id = app_id.to_string();
        let js = server_js.to_string();
        let html = client_html.map(|b| b.to_vec());

        self.blocking(move |conn| {
            let rows = conn
                .execute(
                    "UPDATE apps SET server_js = ?1, client_html = ?2, version = version + 1, updated_at = datetime('now') WHERE id = ?3",
                    params![js, html, id],
                )
                .map_err(|e| RegistryError::Database(e.to_string()))?;

            if rows == 0 {
                return Err(RegistryError::NotFound(id));
            }

            conn.query_row("SELECT version FROM apps WHERE id = ?1", params![id], |row| {
                row.get(0)
            })
            .map_err(|e| RegistryError::Database(e.to_string()))
        })
        .await
    }

    async fn delete_app(&self, app_id: &str) -> Result<bool, RegistryError> {
        let id = app_id.to_string();
        self.blocking(move |conn| {
            let rows = conn
                .execute("DELETE FROM apps WHERE id = ?1", params![id])
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            Ok(rows > 0)
        })
        .await
    }

    async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, plan_id, version, api_key, created_at, updated_at FROM apps ORDER BY id")
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(AppRecord {
                        id: row.get(0)?,
                        plan_id: row.get(1)?,
                        version: row.get(2)?,
                        api_key: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                })
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| RegistryError::Database(e.to_string()))
        })
        .await
    }

    async fn set_plan(&self, app_id: &str, plan_id: &str) -> Result<bool, RegistryError> {
        let id = app_id.to_string();
        let plan = plan_id.to_string();
        self.blocking(move |conn| {
            let rows = conn
                .execute(
                    "UPDATE apps SET plan_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![plan, id],
                )
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            Ok(rows > 0)
        })
        .await
    }

    async fn validate_key(&self, app_id: &str, key: &str) -> Result<bool, RegistryError> {
        let id = app_id.to_string();
        let k = key.to_string();
        self.blocking(move |conn| {
            let stored: Option<String> = conn
                .query_row("SELECT api_key FROM apps WHERE id = ?1", params![id], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            Ok(stored.as_deref() == Some(k.as_str()))
        })
        .await
    }
}
