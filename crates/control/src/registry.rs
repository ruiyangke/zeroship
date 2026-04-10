//! Registry — application CRUD backed by PostgreSQL (appbase-pg).

use std::collections::HashMap;

use appbase_common::auth::hash_api_key;
use appbase_common::types::{AppRecord, RouteEntry, RouteMap, VersionMap};
use appbase_pg::Conn;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RegistryError {
    NotFound(String),
    AlreadyExists(String),
    Database(String),
    InvalidInput(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::AlreadyExists(s) => write!(f, "already exists: {s}"),
            Self::Database(s) => write!(f, "database: {s}"),
            Self::InvalidInput(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl From<appbase_pg::Error> for RegistryError {
    fn from(e: appbase_pg::Error) -> Self {
        let msg = e.to_string();
        if msg.contains("duplicate key") || msg.contains("unique") || msg.contains("23505") {
            Self::AlreadyExists(msg)
        } else {
            Self::Database(msg)
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Application registry backed by PostgreSQL. Stores the DB URL and creates a
/// fresh connection per query — suitable for the low-traffic control plane.
#[derive(Debug)]
pub struct Registry {
    db_url: String,
}

impl Registry {
    /// Connect to the database, run schema migrations, and return a `Registry`.
    pub async fn new(db_url: &str) -> Result<Self, String> {
        let mut conn = Conn::connect(db_url).await.map_err(|e| e.to_string())?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS apps (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL UNIQUE,
                plan_id UUID NOT NULL,
                deploy_hash TEXT,
                api_key TEXT NOT NULL,
                created_at BIGINT NOT NULL DEFAULT (EXTRACT(EPOCH FROM NOW())::bigint),
                updated_at BIGINT NOT NULL DEFAULT (EXTRACT(EPOCH FROM NOW())::bigint)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                resource TEXT NOT NULL,
                value BIGINT NOT NULL DEFAULT 0,
                PRIMARY KEY (app_id, resource)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage_history (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                period TEXT NOT NULL,
                counters JSONB NOT NULL,
                created_at BIGINT NOT NULL DEFAULT (EXTRACT(EPOCH FROM NOW())::bigint)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_history_app ON usage_history(app_id, period)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        let _ = conn.close().await;

        Ok(Self {
            db_url: db_url.to_string(),
        })
    }

    /// Open a fresh connection.
    async fn conn(&self) -> Result<Conn, RegistryError> {
        Conn::connect(&self.db_url).await.map_err(RegistryError::from)
    }

    // -- App CRUD -----------------------------------------------------------

    /// Create a new application. Returns the created `AppRecord`.
    pub async fn create_app(
        &self,
        name: &str,
        plan_id: &Uuid,
    ) -> Result<AppRecord, RegistryError> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RegistryError::InvalidInput(
                "name must be 1-64 alphanumeric/hyphen/underscore".into(),
            ));
        }

        let api_key = Uuid::new_v4().to_string();
        let mut conn = self.conn().await?;

        conn.execute(
            "INSERT INTO apps (name, plan_id, api_key) VALUES ($1, $2, $3)",
            &[&name, plan_id, &api_key],
        )
        .await?;

        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at, updated_at \
                 FROM apps WHERE name = $1",
                &[&name],
            )
            .await?;

        rows.first()
            .map(row_to_record)
            .ok_or_else(|| RegistryError::Database("insert ok but read-back failed".into()))
    }

    /// Get an app by primary key.
    pub async fn get_app(&self, id: &Uuid) -> Result<Option<AppRecord>, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at, updated_at \
                 FROM apps WHERE id = $1",
                &[id],
            )
            .await?;
        Ok(rows.first().map(row_to_record))
    }

    /// Get an app by unique name.
    pub async fn get_app_by_name(&self, name: &str) -> Result<Option<AppRecord>, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at, updated_at \
                 FROM apps WHERE name = $1",
                &[&name],
            )
            .await?;
        Ok(rows.first().map(row_to_record))
    }

    /// List all apps ordered by name.
    pub async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at, updated_at \
                 FROM apps ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().map(row_to_record).collect())
    }

    /// Delete an app by id. Returns true if a row was deleted.
    pub async fn delete_app(&self, id: &Uuid) -> Result<bool, RegistryError> {
        let mut conn = self.conn().await?;
        let n = conn
            .execute("DELETE FROM apps WHERE id = $1", &[id])
            .await?;
        Ok(n > 0)
    }

    /// Set the deploy hash (content-addressable bundle hash) for an app.
    pub async fn set_deploy_hash(&self, id: &Uuid, hash: &str) -> Result<bool, RegistryError> {
        let mut conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE apps SET deploy_hash = $1, \
                 updated_at = (EXTRACT(EPOCH FROM NOW())::bigint) WHERE id = $2",
                &[&hash, id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Change the plan for an app.
    pub async fn set_plan(&self, id: &Uuid, plan_id: &Uuid) -> Result<bool, RegistryError> {
        let mut conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE apps SET plan_id = $1, \
                 updated_at = (EXTRACT(EPOCH FROM NOW())::bigint) WHERE id = $2",
                &[plan_id, id],
            )
            .await?;
        Ok(n > 0)
    }

    // -- Versions / Routes --------------------------------------------------

    /// Return every app's current deploy hash (used by workers to sync).
    pub async fn get_versions(&self) -> Result<VersionMap, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query("SELECT id, deploy_hash FROM apps", &[])
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let hash: Option<String> = row.get("deploy_hash");
            map.insert(id, hash);
        }
        Ok(map)
    }

    /// Build the full route table for the gateway.
    pub async fn get_routes(&self) -> Result<RouteMap, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, api_key, deploy_hash FROM apps",
                &[],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let api_key: String = row.get("api_key");
            map.insert(
                id,
                RouteEntry {
                    name: row.get("name"),
                    plan_id: row.get("plan_id"),
                    api_key_hash: hash_api_key(&api_key),
                    deploy_hash: row.get("deploy_hash"),
                },
            );
        }
        Ok(map)
    }

    // -- Usage / Metering ---------------------------------------------------

    /// Increment a usage counter for an app (upsert).
    pub async fn record_usage(
        &self,
        app_id: &Uuid,
        resource: &str,
        delta: i64,
    ) -> Result<(), RegistryError> {
        let mut conn = self.conn().await?;
        conn.execute(
            "INSERT INTO usage (app_id, resource, value) VALUES ($1, $2, $3) \
             ON CONFLICT (app_id, resource) DO UPDATE SET value = usage.value + $3",
            &[app_id, &resource, &delta],
        )
        .await?;
        Ok(())
    }

    /// Get all usage counters for an app.
    pub async fn get_usage(
        &self,
        app_id: &Uuid,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let mut conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT resource, value FROM usage WHERE app_id = $1",
                &[app_id],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            map.insert(row.get::<String>("resource"), row.get::<i64>("value"));
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a query row into an `AppRecord`.
///
/// Columns: id (UUID), name (TEXT), plan_id (UUID), deploy_hash (TEXT | NULL),
///          api_key (TEXT), created_at (BIGINT), updated_at (BIGINT).
fn row_to_record(row: &appbase_pg::Row) -> AppRecord {
    AppRecord {
        id: row.get("id"),
        name: row.get("name"),
        plan_id: row.get("plan_id"),
        deploy_hash: row.get("deploy_hash"),
        api_key: row.get("api_key"),
        created_at: row.get::<i64>("created_at") as u64,
        updated_at: row.get::<i64>("updated_at") as u64,
    }
}
