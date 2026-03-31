//! SQLite MeterStore adapter — durable warm-tier storage.
//!
//! Uses WAL mode for concurrent reads during writes.
//! Batched transactions for flush (INSERT ... ON CONFLICT DO UPDATE).
//! Per spec v3.0 §4.4.

use appbase_core::meter_store::{MeterStore, MeterStoreError, PeriodSnapshot, ResourceDelta};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct SqliteMeterStore {
    conn: Mutex<Connection>,
}

impl SqliteMeterStore {
    pub fn new(path: &str) -> Result<Self, MeterStoreError> {
        let conn = Connection::open(path)
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;

        // WAL mode for concurrent read/write
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;

        // Create tables
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS counters (
                app_id TEXT NOT NULL,
                resource TEXT NOT NULL,
                value INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (app_id, resource)
            );
            CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                app_id TEXT NOT NULL,
                period TEXT NOT NULL,
                counters TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_history_app ON history(app_id, period);"
        ).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Create an in-memory SQLite store (for testing).
    pub fn in_memory() -> Result<Self, MeterStoreError> {
        Self::new(":memory:")
    }
}

impl MeterStore for SqliteMeterStore {
    fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), MeterStoreError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO counters (app_id, resource, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(app_id, resource) DO UPDATE SET value = value + ?3"
            ).map_err(|e| MeterStoreError::Io(e.to_string()))?;

            for delta in deltas {
                stmt.execute(params![app_id, delta.resource, delta.delta as i64])
                    .map_err(|e| MeterStoreError::Io(e.to_string()))?;
            }
        }

        tx.commit().map_err(|e| MeterStoreError::Io(e.to_string()))?;
        Ok(())
    }

    fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, MeterStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT resource, value FROM counters WHERE app_id = ?1"
        ).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        let rows = stmt.query_map(params![app_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        }).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        let mut counters = HashMap::new();
        for row in rows {
            let (resource, value) = row.map_err(|e| MeterStoreError::Io(e.to_string()))?;
            counters.insert(resource, value);
        }
        Ok(counters)
    }

    fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, MeterStoreError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;

        // Read current counters
        let counters = {
            let mut stmt = tx.prepare_cached(
                "SELECT resource, value FROM counters WHERE app_id = ?1"
            ).map_err(|e| MeterStoreError::Io(e.to_string()))?;
            let rows = stmt.query_map(params![app_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            }).map_err(|e| MeterStoreError::Io(e.to_string()))?;
            let mut c = HashMap::new();
            for row in rows {
                let (r, v) = row.map_err(|e| MeterStoreError::Io(e.to_string()))?;
                c.insert(r, v);
            }
            c
        };

        let now = time::OffsetDateTime::now_utc();
        let period = format!("{}-{:02}", now.year(), now.month() as u8);

        // Store in history as JSON
        let counters_json = serde_json::to_string(&counters)
            .map_err(|e| MeterStoreError::Other(e.to_string()))?;
        tx.execute(
            "INSERT INTO history (app_id, period, counters) VALUES (?1, ?2, ?3)",
            params![app_id, period, counters_json],
        ).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        // Delete current counters
        tx.execute("DELETE FROM counters WHERE app_id = ?1", params![app_id])
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;

        tx.commit().map_err(|e| MeterStoreError::Io(e.to_string()))?;

        Ok(PeriodSnapshot {
            app_id: app_id.to_string(),
            period,
            counters,
        })
    }

    fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, MeterStoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT period, counters FROM history WHERE app_id = ?1 ORDER BY id DESC LIMIT ?2"
        ).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        let rows = stmt.query_map(params![app_id, periods], |row| {
            let period: String = row.get(0)?;
            let counters_json: String = row.get(1)?;
            Ok((period, counters_json))
        }).map_err(|e| MeterStoreError::Io(e.to_string()))?;

        let mut snapshots = Vec::new();
        for row in rows {
            let (period, json) = row.map_err(|e| MeterStoreError::Io(e.to_string()))?;
            let counters: HashMap<String, u64> = serde_json::from_str(&json)
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

    fn close(&self) -> Result<(), MeterStoreError> {
        // Connection drops when SqliteMeterStore drops.
        // Force a WAL checkpoint for clean shutdown.
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| MeterStoreError::Io(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use appbase_core::meter_store::ResourceDelta;

    #[test]
    fn flush_and_load() {
        let store = SqliteMeterStore::in_memory().unwrap();
        store.flush("app1", &[
            ResourceDelta { resource: "requests".into(), delta: 100 },
            ResourceDelta { resource: "cpu_ms".into(), delta: 5000 },
        ]).unwrap();
        let c = store.load("app1").unwrap();
        assert_eq!(c["requests"], 100);
        assert_eq!(c["cpu_ms"], 5000);
    }

    #[test]
    fn flush_is_additive() {
        let store = SqliteMeterStore::in_memory().unwrap();
        store.flush("app1", &[ResourceDelta { resource: "requests".into(), delta: 10 }]).unwrap();
        store.flush("app1", &[ResourceDelta { resource: "requests".into(), delta: 20 }]).unwrap();
        let c = store.load("app1").unwrap();
        assert_eq!(c["requests"], 30);
    }

    #[test]
    fn rollover_resets_and_archives() {
        let store = SqliteMeterStore::in_memory().unwrap();
        store.flush("app1", &[ResourceDelta { resource: "requests".into(), delta: 42 }]).unwrap();
        let snap = store.rollover("app1").unwrap();
        assert_eq!(snap.counters["requests"], 42);
        // Counters should be reset
        let c = store.load("app1").unwrap();
        assert!(c.is_empty());
        // History should have 1 entry
        let hist = store.history("app1", 10).unwrap();
        assert_eq!(hist.len(), 1);
    }

    #[test]
    fn load_unknown_returns_empty() {
        let store = SqliteMeterStore::in_memory().unwrap();
        let c = store.load("ghost").unwrap();
        assert!(c.is_empty());
    }

    #[test]
    fn multiple_apps_isolated() {
        let store = SqliteMeterStore::in_memory().unwrap();
        store.flush("a", &[ResourceDelta { resource: "r".into(), delta: 1 }]).unwrap();
        store.flush("b", &[ResourceDelta { resource: "r".into(), delta: 2 }]).unwrap();
        assert_eq!(store.load("a").unwrap()["r"], 1);
        assert_eq!(store.load("b").unwrap()["r"], 2);
    }
}
