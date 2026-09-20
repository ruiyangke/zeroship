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
        .query(&store.binding, "PRAGMA database_list", &[])
        .await
        .map_err(super::store::database_error)?;
    let namespace = store
        .backend
        .namespace(&store.binding);
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

pub use zeroship_workflow_schema::SQLITE_SQL;

/// Instantiate canonical DDL in the customer's resolved physical schema.
/// The provisioning host supplies its own authorized migration connection.
///
/// The substitution itself lives in `zeroship-workflow-schema` beside the
/// artifact it binds, so the installer and this engine cannot disagree about it.
#[must_use]
pub fn postgres_sql(schema: &super::store::SchemaName) -> String {
    zeroship_workflow_schema::postgres_sql(schema.as_str())
}

pub(crate) fn fingerprint(dialect: &str) -> Result<String, WorkflowServiceError> {
    zeroship_workflow_schema::fingerprint(dialect)
        .map(str::to_owned)
        .ok_or_else(|| WorkflowServiceError::Internal("unsupported workflow schema dialect".into()))
}

/// Bring the local journal to the current version, installing it if absent.
///
/// This is the SQLite half of a deliberate seam: `zeroship-migrate-server` is
/// PostgreSQL-only and refuses everything else, so the local journal is applied
/// here instead. The two appliers share ONE source - the ordered series in
/// `zeroship-workflow-schema` - so they can differ in who applies a version,
/// never in which versions exist or what they contain.
///
/// Every version runs in ONE transaction with the stamp write, so an upgrade
/// that fails part way leaves the stamp at the version that is actually
/// installed. Business tables can already exist in the database.
///
/// # Errors
/// Refuses a corrupted journal (the stamp's version matches but its fingerprint
/// does not) and one written by a NEWER platform, and reports filesystem or
/// database failures.
pub fn initialize_sqlite(path: &Path) -> Result<(), WorkflowServiceError> {
    use zeroship_workflow_schema::{SQLITE, STAMP_ROW_ID, STAMP_TABLE, VERSION};

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
    let stamped: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [STAMP_TABLE],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    let installed = if stamped {
        let row = tx
            .query_row(
                &format!("SELECT version, fingerprint FROM {STAMP_TABLE} WHERE id = ?1"),
                [STAMP_ROW_ID],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| incompatible())?;
        Some(row)
    } else {
        // A database carrying journal objects but NO stamp is a partial or
        // foreign journal, not an empty one. Installing over it would adopt
        // whatever is there; refuse instead, and leave its rows alone.
        let residue: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name GLOB '__zeroship_workflow_*')",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if residue {
            return Err(incompatible());
        }
        None
    };

    let target = i64::from(VERSION);
    let expected = fingerprint(SQLITE)?;
    let applied_through = match installed {
        None => 0,
        Some((version, actual)) if version == target => {
            if actual != expected {
                return Err(incompatible());
            }
            return tx.commit().map_err(sqlite_error);
        }
        Some((version, _)) if version < target => version,
        Some((version, _)) => return Err(journal_ahead(version, target)),
    };

    let series = zeroship_workflow_schema::versions(SQLITE)
        .ok_or_else(|| WorkflowServiceError::Internal("no workflow schema series".into()))?;
    for step in series
        .iter()
        .filter(|step| i64::from(step.version) > applied_through)
    {
        tx.execute_batch(step.sql).map_err(sqlite_error)?;
    }
    tx.execute(
        &format!(
            "INSERT INTO {STAMP_TABLE} (id, version, fingerprint) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET version = excluded.version, \
             fingerprint = excluded.fingerprint"
        ),
        rusqlite::params![STAMP_ROW_ID, target, expected],
    )
    .map_err(sqlite_error)?;
    tx.commit().map_err(sqlite_error)
}

pub(crate) fn incompatible() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(
        "workflow store schema is incompatible; apply workflow migrations to the app database"
            .into(),
    )
}

/// The journal was written by a platform NEWER than this one.
///
/// Distinct from [`incompatible`] because the remedy is the opposite: nothing
/// should apply an older series over it, and the operator has to upgrade this
/// process rather than repair the database.
fn journal_ahead(installed: i64, target: i64) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(format!(
        "workflow journal is at version {installed}, ahead of this build's {target}; \
         upgrade the process rather than downgrading the journal"
    ))
}

fn sqlite_error(_error: rusqlite::Error) -> WorkflowServiceError {
    WorkflowServiceError::Internal("workflow local schema operation failed".into())
}
