//! Local host locks and the filesystem scope shared with explicit reset.

use super::LocalConfig;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};
use zeroship_core::app_id::AppId;

const OBJECT_NAMESPACES: &[&str] = &["platform:workflow", "platform:workflow-snapshots"];

#[derive(Debug)]
pub(super) struct StatePaths {
    pub journal: PathBuf,
    pub objects: PathBuf,
    pub buckets: Vec<PathBuf>,
    pub markers: Vec<PathBuf>,
    locks: Vec<PathBuf>,
}
impl StatePaths {
    pub fn new(root: &Path, config: &LocalConfig, app: &AppId) -> Result<Self, String> {
        let journal = canonical_target(&config.journal)?;
        let objects = canonical_target(&config.objects)?;
        if let Ok(metadata) = std::fs::metadata(&journal) {
            check_regular(&metadata)?;
        }
        if let Ok(metadata) = std::fs::metadata(&objects) {
            if !metadata.is_dir() {
                return Err("workflow objects path must be a directory".into());
            }
        }
        let buckets: Vec<_> = OBJECT_NAMESPACES
            .iter()
            .map(|namespace| objects.join(namespace).join(app.as_str()))
            .collect();
        let markers = vec![
            suffix(&journal, ".workflow-reset.json"),
            objects.join(format!(".workflow-{}.reset.json", app.as_str())),
        ];
        let locks = vec![
            suffix(&journal, ".workflow.lock"),
            objects.join(format!(".workflow-{}.lock", app.as_str())),
        ];
        let identity = canonical_target(&root.join(".zeroship/app-id"))?;
        let mut protected = vec![identity];
        if let Some(bundle) = &config.bundle {
            protected.push(canonical_target(bundle)?);
        }
        let files: Vec<_> = journal_files(&journal)
            .into_iter()
            .chain(markers.iter().cloned())
            .chain(locks.iter().cloned())
            .collect();
        for file in &files {
            if protected.iter().any(|path| path == file) {
                return Err("workflow state overlaps the project identity or bundle".into());
            }
        }
        protected.extend(files);
        if buckets
            .iter()
            .any(|bucket| protected.iter().any(|file| file.starts_with(bucket)))
        {
            return Err("workflow object scope overlaps another host state path".into());
        }
        Ok(Self {
            journal,
            objects,
            buckets,
            markers,
            locks,
        })
    }

    pub fn ensure_ready(&self) -> Result<(), String> {
        for marker in &self.markers {
            match std::fs::symlink_metadata(marker) {
                Ok(_) => return Err("local workflow reset is unfinished; rerun zeroship workflows reset with the same configuration".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(format!("inspect workflow reset state: {error}")),
            }
        }
        Ok(())
    }

    pub fn lock(&self, exclusive: bool) -> Result<StateLock, String> {
        let mut files = Vec::new();
        for path in &self.locks {
            std::fs::create_dir_all(path.parent().expect("resolved lock parent"))
                .map_err(|error| format!("create workflow lock directory: {error}"))?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let file = options
                .open(path)
                .map_err(|error| format!("open workflow state lock: {error}"))?;
            check_regular(&file.metadata().map_err(|error| error.to_string())?)?;
            let result = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            result.map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => {
                    "local workflow state is in use; stop its workers before resetting".into()
                }
                std::fs::TryLockError::Error(error) => {
                    format!("lock local workflow state: {error}")
                }
            })?;
            files.push(file);
        }
        Ok(StateLock { _files: files })
    }
}

// Lock files stay in place after release so another opener cannot lock a new inode.
#[derive(Debug)]
pub(super) struct StateLock {
    _files: Vec<File>,
}

pub(super) fn journal_files(journal: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|part| suffix(journal, part))
        .collect()
}

pub(super) fn suffix(path: &Path, value: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(value);
    path.into()
}

pub(super) fn canonical_target(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "workflow state path must not be a symbolic link: {}",
                    path.display()
                ));
            }
            std::fs::canonicalize(path)
                .map_err(|error| format!("resolve workflow state path: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("workflow state path has no parent")?;
            let name = path
                .file_name()
                .ok_or("workflow state path must name a file or directory")?;
            Ok(canonical_target(parent)?.join(name))
        }
        Err(error) => Err(format!("inspect workflow state path: {error}")),
    }
}

pub(super) fn check_regular(metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_file() {
        return Err("workflow state must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("workflow state must not have hard links".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_lock_excludes_hosts_sharing_either_journal_or_objects() {
        let root = tempfile::tempdir().unwrap();
        let app = super::super::project_identity(root.path()).unwrap();
        let config = LocalConfig::default().resolve(root.path()).unwrap();
        let paths = StatePaths::new(root.path(), &config, &app).unwrap();
        let host = paths.lock(false).unwrap();
        assert!(paths.lock(false).is_ok());
        assert!(paths.lock(true).unwrap_err().contains("in use"));
        let other = LocalConfig {
            journal: root.path().join("other.sqlite"),
            ..config.clone()
        };
        let same_objects = StatePaths::new(root.path(), &other, &app).unwrap();
        assert!(same_objects.lock(true).unwrap_err().contains("in use"));
        let other = LocalConfig {
            objects: root.path().join("other-objects"),
            ..config
        };
        let same_journal = StatePaths::new(root.path(), &other, &app).unwrap();
        assert!(same_journal.lock(true).unwrap_err().contains("in use"));
        drop(host);
        let reset = paths.lock(true).unwrap();
        assert!(paths.lock(false).unwrap_err().contains("in use"));
        drop(reset);
        assert!(paths.lock(false).is_ok());
    }
}
