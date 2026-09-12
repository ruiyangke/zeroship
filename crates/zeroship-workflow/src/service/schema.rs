//! Migration-compiler output used by host provisioning, never by task claims.

use std::path::Path;

use crate::WorkflowServiceError;

/// Initialize the SQLite file already selected by the host's ORM binding.
/// PostgreSQL provisioning belongs to the authorized migration host.
pub async fn initialize_local(store: &super::store::OrmStore) -> Result<(), WorkflowServiceError> {
    use zeroship_data_orm::{sql::registration::SQLITE_FAMILY, Value};
    if store.backend.sql_registration().family() != SQLITE_FAMILY {
        return Ok(());
    }
    let rows = store
        .backend
        .query(
            store.binding.app_id(),
            store.binding.schema(),
            "PRAGMA database_list",
            &[],
        )
        .await
        .map_err(super::store::database_error)?;
    let namespace = store
        .backend
        .namespace(store.binding.app_id(), store.binding.schema());
    let file = rows
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(namespace))
        .and_then(|row| row.get("file"))
        .and_then(Value::as_str)
        .filter(|file| !file.is_empty())
        .ok_or_else(|| {
            WorkflowServiceError::Unavailable("workflow app database is not attached".into())
        })?;
    initialize_sqlite(Path::new(file))
}

const POSTGRES_TEMPLATE: &str = include_str!("../../schema/postgres.sql");
pub const SQLITE_SQL: &str = include_str!("../../schema/sqlite.sql");
/// Migration-derived field and key metadata for the customer journal.
pub const RUNTIME_DESCRIPTOR: &str = include_str!("../../schema/schema.runtime.json");
/// Instantiate canonical DDL in the customer's resolved physical schema.
/// The provisioning host supplies its own authorized migration connection.
#[must_use]
pub fn postgres_sql(schema: &super::store::SchemaName) -> String {
    POSTGRES_TEMPLATE.replace(
        "\"__zeroship_workflow_schema\"",
        &zeroship_data_orm::sql::mapping::quote_ident(schema.as_str()),
    )
}

const FINGERPRINTS: &str = include_str!("../../schema/fingerprints.json");

pub(crate) fn fingerprint(dialect: &str) -> Result<String, WorkflowServiceError> {
    let fingerprints: std::collections::BTreeMap<String, String> =
        serde_json::from_str(FINGERPRINTS).map_err(|_| {
            WorkflowServiceError::Internal("invalid generated workflow schema fingerprint".into())
        })?;
    fingerprints
        .get(dialect)
        .cloned()
        .ok_or_else(|| WorkflowServiceError::Internal("unsupported workflow schema dialect".into()))
}

/// Initialize workflow tables in the app's local database.
///
/// Uses the shared migration definition. Existing workflow tables are verified
/// without altering or resetting their schema. Business tables can already
/// exist in the database.
///
/// # Errors
/// Refuses incompatible journals and reports filesystem or database failures.
pub fn initialize_sqlite(path: &Path) -> Result<(), WorkflowServiceError> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            WorkflowServiceError::Unavailable(format!("create workflow directory: {error}"))
        })?;
    }
    let mut conn = rusqlite::Connection::open(path).map_err(sqlite_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(sqlite_error)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sqlite_error)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sqlite_error)?;
    let populated: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name GLOB '__zeroship_workflow_*')",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if populated {
        let actual: String = tx
            .query_row(
                "SELECT fingerprint FROM __zeroship_workflow_schema_version WHERE id = 'workflow'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| incompatible())?;
        if actual != fingerprint("sqlite")? {
            return Err(incompatible());
        }
    } else {
        tx.execute_batch(SQLITE_SQL).map_err(sqlite_error)?;
    }
    tx.commit().map_err(sqlite_error)
}

pub(crate) fn incompatible() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(
        "workflow store schema is incompatible; apply workflow migrations to the app database"
            .into(),
    )
}

fn sqlite_error(_error: rusqlite::Error) -> WorkflowServiceError {
    WorkflowServiceError::Internal("workflow local schema operation failed".into())
}
