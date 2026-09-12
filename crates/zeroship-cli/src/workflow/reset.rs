//! Explicit local reset, with durable intent and verified project ownership.

use super::{
    state::{check_regular, journal_files, StateLock, StatePaths},
    LocalConfig,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroship_core::app_id::AppId;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    app: String,
    journal: PathBuf,
    objects: PathBuf,
}

struct Reset {
    paths: StatePaths,
    _lock: StateLock,
}
impl Reset {
    fn prepare(root: &Path, config: LocalConfig) -> Result<Self, String> {
        let app = super::read_project_identity(root)?;
        let config = config.resolve(root)?;
        let paths = StatePaths::new(root, &config, &app)?;
        let lock = paths.lock(true)?;
        let intent = Intent {
            app: app.as_str().into(),
            journal: paths.journal.clone(),
            objects: paths.objects.clone(),
        };
        let mut resuming = false;
        for marker in &paths.markers {
            if let Some(existing) = read_intent(marker)? {
                if existing != intent {
                    return Err(
                        "unfinished workflow reset belongs to another app or configuration".into(),
                    );
                }
                resuming = true;
            }
        }
        validate_targets(&paths)?;
        verify_journal(&paths.journal, &app, resuming)?;
        for marker in &paths.markers {
            write_intent(marker, &intent)?;
        }
        Ok(Self { paths, _lock: lock })
    }

    fn execute(self) -> Result<(), String> {
        validate_targets(&self.paths)?;
        for file in journal_files(&self.paths.journal) {
            remove_file(&file)?;
        }
        for bucket in &self.paths.buckets {
            match std::fs::remove_dir_all(bucket) {
                Ok(()) => sync_parent(bucket)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "remove workflow objects: {error}; rerun the reset to finish"
                    ))
                }
            }
        }
        zeroship_workflow::service::schema::initialize_sqlite(&self.paths.journal)
            .map_err(|error| format!("initialize reset workflow journal: {error}"))?;
        File::open(&self.paths.journal)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        sync_parent(&self.paths.journal)?;
        for marker in &self.paths.markers {
            remove_file(marker)?;
        }
        Ok(())
    }
}

pub(super) fn reset(root: &Path, config: LocalConfig) -> Result<(), String> {
    Reset::prepare(root, config)?.execute()
}

fn verify_journal(path: &Path, app: &AppId, resuming: bool) -> Result<(), String> {
    if !path.exists() {
        if !resuming && journal_files(path).iter().skip(1).any(|file| file.exists()) {
            return Err("workflow journal is missing but SQLite sidecars remain; restore the journal before resetting".into());
        }
        return Ok(());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("inspect workflow journal: {error}"))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    let integrity: String = connection
        .pragma_query_value(None, "quick_check", |row| row.get(0))
        .map_err(|error| format!("verify workflow journal: {error}"))?;
    if integrity != "ok" {
        return Err("workflow journal is damaged; restore it before resetting".into());
    }
    let mut statement = connection
        .prepare("SELECT type, name, tbl_name FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'")
        .map_err(|error| error.to_string())?;
    let objects = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for (kind, name, table) in objects {
        if !owned_name(&name) || !owned_name(&table) || !matches!(kind.as_str(), "table" | "index")
        {
            return Err("refusing to reset a database containing non-workflow objects".into());
        }
        if kind != "table" || name == "__zeroship_workflow_schema_version" {
            continue;
        }
        // Owned identifiers are restricted before inclusion; the identity is a parameter.
        let foreign: bool = connection
            .query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM \"{name}\" WHERE app_id IS NULL OR app_id<>?1)"
                ),
                [app.as_str()],
                |row| row.get(0),
            )
            .map_err(|_| "workflow journal ownership could not be verified".to_string())?;
        if foreign {
            return Err("refusing to reset a workflow journal containing another app".into());
        }
    }
    Ok(())
}

fn owned_name(name: &str) -> bool {
    name.starts_with("__zeroship_workflow_")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_targets(paths: &StatePaths) -> Result<(), String> {
    for file in journal_files(&paths.journal)
        .into_iter()
        .chain(paths.markers.iter().cloned())
    {
        match std::fs::symlink_metadata(&file) {
            Ok(metadata) => check_regular(&metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect workflow reset file: {error}")),
        }
    }
    for bucket in &paths.buckets {
        for directory in [
            bucket.parent().expect("workflow namespace"),
            bucket.as_path(),
        ] {
            match std::fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(
                        "workflow object scope must be a directory without symbolic links".into(),
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("inspect workflow object scope: {error}")),
            }
        }
    }
    Ok(())
}

fn read_intent(path: &Path) -> Result<Option<Intent>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            check_regular(&metadata)?;
            if metadata.len() > 16 * 1024 {
                return Err("workflow reset record is oversized".into());
            }
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| file.take(16 * 1024 + 1).read_to_end(&mut bytes))
                .map_err(|error| error.to_string())?;
            if bytes.len() > 16 * 1024 {
                return Err("workflow reset record is oversized".into());
            }
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| "workflow reset record is invalid".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read workflow reset record: {error}")),
    }
}

fn write_intent(path: &Path, intent: &Intent) -> Result<(), String> {
    if read_intent(path)?.is_some() {
        return Ok(());
    }
    let mut pending = tempfile::NamedTempFile::new_in(path.parent().expect("reset marker parent"))
        .map_err(|error| error.to_string())?;
    pending
        .write_all(&serde_json::to_vec(intent).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    pending
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    pending
        .persist_noclobber(path)
        .map_err(|error| error.to_string())?;
    sync_parent(path)
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove workflow state: {error}; rerun the reset to finish"
        )),
    }
}
fn sync_parent(path: &Path) -> Result<(), String> {
    File::open(path.parent().expect("resolved state parent"))
        .and_then(|dir| dir.sync_all())
        .map_err(|error| format!("sync workflow state directory: {error}"))
}

#[cfg(test)]
mod tests;
