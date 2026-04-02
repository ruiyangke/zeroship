//! SqlxRegistry — AppRegistry backed by sqlx AnyPool (SQLite or Postgres).
//!
//! Uses `sqlx::any::AnyPool` so the same code works against both SQLite and Postgres.
//! SQL uses `$N` bind parameters and `CURRENT_TIMESTAMP` for cross-database compatibility.

use async_trait::async_trait;
use sqlx::any::{Any, AnyRow};
use sqlx::{Executor, Row};

type AnyPool = sqlx::Pool<Any>;

use crate::{AppData, AppRecord, AppRegistry, RegistryError};

fn db_err(e: sqlx::Error) -> RegistryError {
    RegistryError::Database(e.to_string())
}

fn app_record_from_row(r: &AnyRow) -> AppRecord {
    AppRecord {
        id: r.get("id"),
        plan_id: r.get("plan_id"),
        version: r.get("version"),
        api_key: r.get("api_key"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// AppRegistry implementation backed by sqlx AnyPool (SQLite or Postgres).
pub struct SqlxRegistry {
    pool: AnyPool,
    master_key: String,
}

impl SqlxRegistry {
    /// Open (or create) the registry database from a connection URL.
    ///
    /// Examples:
    /// - `sqlite://path/to/apps.db`
    /// - `sqlite://:memory:`
    /// - `postgres://user:pass@host/db`
    pub async fn new(database_url: &str, master_key: String) -> Result<Self, RegistryError> {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(database_url)
            .await
            .map_err(|e| RegistryError::Database(format!("Connection failed: {e}")))?;

        let registry = Self { pool, master_key };
        registry.migrate().await?;
        Ok(registry)
    }

    /// Run schema migrations.
    async fn migrate(&self) -> Result<(), RegistryError> {
        self.pool
            .execute(
                "CREATE TABLE IF NOT EXISTS apps (
                    id          TEXT PRIMARY KEY,
                    plan_id     TEXT NOT NULL DEFAULT 'free',
                    server_js   TEXT NOT NULL DEFAULT '',
                    client_html BLOB,
                    version     INTEGER NOT NULL DEFAULT 0,
                    api_key     TEXT NOT NULL,
                    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                )",
            )
            .await
            .map_err(|e| RegistryError::Database(format!("Migration failed: {e}")))?;

        Ok(())
    }

    /// Validate the master key for admin operations.
    pub fn check_master_key(&self, key: &str) -> bool {
        self.master_key == key
    }
}

#[async_trait]
impl AppRegistry for SqlxRegistry {
    async fn get_app(&self, app_id: &str) -> Result<Option<AppData>, RegistryError> {
        let row = sqlx::query(
            "SELECT id, plan_id, server_js, client_html, version, api_key, created_at, updated_at FROM apps WHERE id = $1",
        )
        .bind(app_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(row.map(|r| AppData {
            id: r.get("id"),
            plan_id: r.get("plan_id"),
            server_js: r.get("server_js"),
            client_html: r.get("client_html"),
            version: r.get("version"),
            api_key: r.get("api_key"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        }))
    }

    async fn get_version(&self, app_id: &str) -> Result<Option<i64>, RegistryError> {
        let row = sqlx::query("SELECT version FROM apps WHERE id = $1")
            .bind(app_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(row.map(|r| r.get("version")))
    }

    async fn get_plan(&self, app_id: &str) -> Result<String, RegistryError> {
        let row = sqlx::query("SELECT plan_id FROM apps WHERE id = $1")
            .bind(app_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        row.map(|r| r.get("plan_id"))
            .ok_or_else(|| RegistryError::NotFound(app_id.to_string()))
    }

    async fn create_app(&self, app_id: &str, plan_id: &str) -> Result<AppRecord, RegistryError> {
        if app_id.is_empty()
            || app_id.len() > 64
            || !app_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RegistryError::InvalidInput(
                "app_id must be 1-64 alphanumeric/hyphen/underscore characters".to_string(),
            ));
        }

        let api_key = uuid::Uuid::new_v4().to_string();

        let result = sqlx::query("INSERT INTO apps (id, plan_id, api_key) VALUES ($1, $2, $3)")
            .bind(app_id)
            .bind(plan_id)
            .bind(&api_key)
            .execute(&self.pool)
            .await;

        if let Err(e) = result {
            let msg = e.to_string();
            if msg.contains("UNIQUE") || msg.contains("duplicate key") {
                return Err(RegistryError::AlreadyExists(app_id.to_string()));
            }
            return Err(RegistryError::Database(msg));
        }

        let row = sqlx::query(
            "SELECT id, plan_id, version, api_key, created_at, updated_at FROM apps WHERE id = $1",
        )
        .bind(app_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(app_record_from_row(&row))
    }

    async fn deploy(
        &self,
        app_id: &str,
        server_js: &str,
        client_html: Option<&[u8]>,
    ) -> Result<i64, RegistryError> {
        let q = sqlx::query(
            "UPDATE apps SET server_js = $1, client_html = $2, version = version + 1, updated_at = CURRENT_TIMESTAMP WHERE id = $3",
        )
        .bind(server_js)
        .bind(client_html)
        .bind(app_id);
        let result = self.pool.execute(q).await.map_err(db_err)?;

        if result.rows_affected() == 0 {
            return Err(RegistryError::NotFound(app_id.to_string()));
        }

        let row = sqlx::query("SELECT version FROM apps WHERE id = $1")
            .bind(app_id)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(row.get("version"))
    }

    async fn delete_app(&self, app_id: &str) -> Result<bool, RegistryError> {
        let q = sqlx::query("DELETE FROM apps WHERE id = $1").bind(app_id);
        let result = self.pool.execute(q).await.map_err(db_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        let rows = sqlx::query(
            "SELECT id, plan_id, version, api_key, created_at, updated_at FROM apps ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(rows.iter().map(app_record_from_row).collect())
    }

    async fn set_plan(&self, app_id: &str, plan_id: &str) -> Result<bool, RegistryError> {
        let q = sqlx::query(
            "UPDATE apps SET plan_id = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
        )
        .bind(plan_id)
        .bind(app_id);
        let result = self.pool.execute(q).await.map_err(db_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn validate_key(&self, app_id: &str, key: &str) -> Result<bool, RegistryError> {
        let row = sqlx::query("SELECT api_key FROM apps WHERE id = $1")
            .bind(app_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(row
            .map(|r| {
                let stored: String = r.get("api_key");
                stored == key
            })
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_db_url() -> String {
        let id = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("sqlite:file:testdb_{id}?mode=memory&cache=shared")
    }

    #[tokio::test]
    async fn create_and_get_app() {
        let reg = SqlxRegistry::new(&test_db_url(), "test-key".into())
            .await
            .unwrap();
        let record = reg.create_app("test-app", "free").await.unwrap();
        assert_eq!(record.id, "test-app");
        assert_eq!(record.plan_id, "free");
        assert_eq!(record.version, 0);
        assert!(!record.api_key.is_empty());

        let app = reg.get_app("test-app").await.unwrap().unwrap();
        assert_eq!(app.id, "test-app");
        assert_eq!(app.server_js, ""); // empty until deployed
    }

    #[tokio::test]
    async fn create_duplicate_fails() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        reg.create_app("app1", "free").await.unwrap();
        let err = reg.create_app("app1", "free").await.unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn deploy_and_get_version() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        reg.create_app("app1", "free").await.unwrap();

        let v1 = reg.deploy("app1", "var __rpc = {};", None).await.unwrap();
        assert_eq!(v1, 1);

        let v2 = reg
            .deploy("app1", "var __rpc = { ping: function() {} };", None)
            .await
            .unwrap();
        assert_eq!(v2, 2);

        let app = reg.get_app("app1").await.unwrap().unwrap();
        assert!(app.server_js.contains("ping"));
        assert_eq!(app.version, 2);
    }

    #[tokio::test]
    async fn deploy_nonexistent_fails() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        let err = reg.deploy("ghost", "code", None).await.unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_app() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        reg.create_app("app1", "free").await.unwrap();
        assert!(reg.delete_app("app1").await.unwrap());
        assert!(reg.get_app("app1").await.unwrap().is_none());
        assert!(!reg.delete_app("app1").await.unwrap()); // already deleted
    }

    #[tokio::test]
    async fn list_apps() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        reg.create_app("a", "free").await.unwrap();
        reg.create_app("b", "pro").await.unwrap();
        let apps = reg.list_apps().await.unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].id, "a");
        assert_eq!(apps[1].id, "b");
    }

    #[tokio::test]
    async fn set_plan() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        reg.create_app("app1", "free").await.unwrap();
        assert!(reg.set_plan("app1", "pro").await.unwrap());
        let plan = reg.get_plan("app1").await.unwrap();
        assert_eq!(plan, "pro");
    }

    #[tokio::test]
    async fn validate_key() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        let record = reg.create_app("app1", "free").await.unwrap();
        assert!(reg.validate_key("app1", &record.api_key).await.unwrap());
        assert!(!reg.validate_key("app1", "wrong-key").await.unwrap());
        assert!(!reg.validate_key("nonexistent", "any").await.unwrap());
    }

    #[tokio::test]
    async fn get_version() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        assert!(reg.get_version("ghost").await.unwrap().is_none());
        reg.create_app("app1", "free").await.unwrap();
        assert_eq!(reg.get_version("app1").await.unwrap(), Some(0));
        reg.deploy("app1", "code", None).await.unwrap();
        assert_eq!(reg.get_version("app1").await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn invalid_app_id() {
        let reg = SqlxRegistry::new(&test_db_url(), "k".into())
            .await
            .unwrap();
        assert!(reg.create_app("", "free").await.is_err());
        assert!(reg.create_app("has spaces", "free").await.is_err());
        assert!(reg.create_app(&"a".repeat(65), "free").await.is_err());
    }
}
