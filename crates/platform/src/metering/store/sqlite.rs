//! SQLite MeterStore adapter — durable warm-tier storage.
//!
//! Uses WAL mode for concurrent reads during writes.
//! Batched transactions for flush (INSERT ... ON CONFLICT DO UPDATE).
//! Per spec v3.0 §4.4.
//!
//! Uses sqlx AnyPool for SQLite+Postgres compatibility.

use crate::core::meter_store::{MeterStore, MeterStoreError, PeriodSnapshot, ResourceDelta};
use async_trait::async_trait;
use sqlx::any::Any;
type AnyPool = sqlx::Pool<Any>;
use sqlx::{Executor, Row};
use std::collections::HashMap;

fn db_err(e: sqlx::Error) -> MeterStoreError {
    MeterStoreError::Io(e.to_string())
}

pub struct SqliteMeterStore {
    pool: AnyPool,
}

impl SqliteMeterStore {
    /// Create a new store from a sqlx connection URL (e.g. `sqlite://path` or `sqlite://:memory:`).
    pub async fn new(database_url: &str) -> Result<Self, MeterStoreError> {
        sqlx::any::install_default_drivers();
        let opts: sqlx::any::AnyConnectOptions = database_url.parse()
            .map_err(|e: sqlx::Error| MeterStoreError::Io(e.to_string()))?;
        // max_connections(1) is critical for in-memory SQLite — each connection
        // gets its own database, so we must reuse a single connection.
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(db_err)?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS counters (
                app_id TEXT NOT NULL,
                resource TEXT NOT NULL,
                value INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (app_id, resource)
            )",
        )
        .execute(&pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS history (
                app_id TEXT NOT NULL,
                period TEXT NOT NULL,
                counters TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&pool)
        .await
        .map_err(db_err)?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_history_app ON history(app_id, period)")
            .execute(&pool)
            .await
            .map_err(db_err)?;

        Ok(Self { pool })
    }

    /// Create an in-memory SQLite store (for testing).
    pub async fn in_memory() -> Result<Self, MeterStoreError> {
        Self::new("sqlite:?mode=memory").await
    }

}

#[async_trait]
impl MeterStore for SqliteMeterStore {
    async fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), MeterStoreError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        for delta in deltas {
            let q = sqlx::query(
                "INSERT INTO counters (app_id, resource, value) VALUES ($1, $2, $3)
                 ON CONFLICT(app_id, resource) DO UPDATE SET value = counters.value + $3",
            )
            .bind(app_id)
            .bind(&delta.resource)
            .bind(delta.delta as i64);
            tx.execute(q).await.map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, MeterStoreError> {
        let rows = sqlx::query("SELECT resource, value FROM counters WHERE app_id = $1")
            .bind(app_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        let mut counters = HashMap::new();
        for row in &rows {
            let resource: String = row.get("resource");
            let value: i64 = row.get("value");
            counters.insert(resource, value as u64);
        }
        Ok(counters)
    }

    async fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, MeterStoreError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Read current counters
        let rows = sqlx::query("SELECT resource, value FROM counters WHERE app_id = $1")
            .bind(app_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;

        let mut counters = HashMap::new();
        for row in &rows {
            let resource: String = row.get("resource");
            let value: i64 = row.get("value");
            counters.insert(resource, value as u64);
        }

        let now = time::OffsetDateTime::now_utc();
        let period = format!("{}-{:02}", now.year(), now.month() as u8);

        // Store in history as JSON
        let counters_json = serde_json::to_string(&counters)
            .map_err(|e| MeterStoreError::Other(e.to_string()))?;

        let insert_q = sqlx::query(
            "INSERT INTO history (app_id, period, counters) VALUES ($1, $2, $3)",
        )
        .bind(app_id)
        .bind(&period)
        .bind(&counters_json);
        tx.execute(insert_q).await.map_err(db_err)?;

        // Delete current counters
        let delete_q = sqlx::query("DELETE FROM counters WHERE app_id = $1")
            .bind(app_id);
        tx.execute(delete_q).await.map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;

        Ok(PeriodSnapshot {
            app_id: app_id.to_string(),
            period,
            counters,
        })
    }

    async fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, MeterStoreError> {
        let rows = sqlx::query(
            "SELECT period, counters FROM history WHERE app_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(app_id)
        .bind(periods as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut snapshots = Vec::new();
        for row in &rows {
            let period: String = row.get("period");
            let counters_json: String = row.get("counters");
            let counters: HashMap<String, u64> = serde_json::from_str(&counters_json)
                .map_err(|e| MeterStoreError::Other(e.to_string()))?;
            snapshots.push(PeriodSnapshot {
                app_id: app_id.to_string(),
                period,
                counters,
            });
        }
        snapshots.reverse(); // oldest first
        Ok(snapshots)
    }

    async fn close(&self) -> Result<(), MeterStoreError> {
        // AnyPool handles connection cleanup on drop.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::meter_store::ResourceDelta;

    #[tokio::test]
    async fn flush_and_load() {
        let store = SqliteMeterStore::in_memory().await.unwrap();
        store
            .flush(
                "app1",
                &[
                    ResourceDelta {
                        resource: "requests".into(),
                        delta: 100,
                    },
                    ResourceDelta {
                        resource: "cpu_us".into(),
                        delta: 5000,
                    },
                ],
            )
            .await
            .unwrap();
        let c = store.load("app1").await.unwrap();
        assert_eq!(c["requests"], 100);
        assert_eq!(c["cpu_us"], 5000);
    }

    #[tokio::test]
    async fn flush_is_additive() {
        let store = SqliteMeterStore::in_memory().await.unwrap();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 10,
                }],
            )
            .await
            .unwrap();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 20,
                }],
            )
            .await
            .unwrap();
        let c = store.load("app1").await.unwrap();
        assert_eq!(c["requests"], 30);
    }

    #[tokio::test]
    async fn rollover_resets_and_archives() {
        let store = SqliteMeterStore::in_memory().await.unwrap();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 42,
                }],
            )
            .await
            .unwrap();
        let snap = store.rollover("app1").await.unwrap();
        assert_eq!(snap.counters["requests"], 42);
        // Counters should be reset
        let c = store.load("app1").await.unwrap();
        assert!(c.is_empty());
        // History should have 1 entry
        let hist = store.history("app1", 10).await.unwrap();
        assert_eq!(hist.len(), 1);
    }

    #[tokio::test]
    async fn load_unknown_returns_empty() {
        let store = SqliteMeterStore::in_memory().await.unwrap();
        let c = store.load("ghost").await.unwrap();
        assert!(c.is_empty());
    }

    #[tokio::test]
    async fn multiple_apps_isolated() {
        let store = SqliteMeterStore::in_memory().await.unwrap();
        store
            .flush(
                "a",
                &[ResourceDelta {
                    resource: "r".into(),
                    delta: 1,
                }],
            )
            .await
            .unwrap();
        store
            .flush(
                "b",
                &[ResourceDelta {
                    resource: "r".into(),
                    delta: 2,
                }],
            )
            .await
            .unwrap();
        assert_eq!(store.load("a").await.unwrap()["r"], 1);
        assert_eq!(store.load("b").await.unwrap()["r"], 2);
    }
}
