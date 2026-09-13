//! Persistent project key for the standalone development host.

use std::{fs::OpenOptions, io::Write, path::Path, sync::Arc};
use zeroship_core::{project_data_key::ProjectDataKey, project_id::ProjectId, AppId};
use zeroship_data_orm::encryption::SuppliedProjectKeys;

pub(crate) fn load(directory: &Path, app_id: &AppId) -> Result<Arc<SuppliedProjectKeys>, String> {
    let material = load_material(directory).map_err(|error| {
        format!(
            "cannot open the local project key in {}: {error}",
            directory.display()
        )
    })?;
    let keys = Arc::new(SuppliedProjectKeys::new());
    keys.supply(app_id.as_str(), material.project_id.as_str(), *material.key())
        .map_err(|error| error.to_string())?;
    Ok(keys)
}

fn load_material(directory: &Path) -> Result<ProjectDataKey, Box<dyn std::error::Error>> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let path = directory.join("project-data-key.json");
    if !path.try_exists()? {
        let temporary = directory.join(format!(".project-data-key-{}", uuid::Uuid::new_v4()));
        struct Pending(std::path::PathBuf);
        impl Drop for Pending {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let pending = Pending(temporary);
        let material = ProjectDataKey::generate(ProjectId::mint());
        serde_json::to_writer(&mut file, &material)?;
        file.flush()?;
        file.sync_all()?;
        // Publish complete material without replacing a concurrent winner.
        match std::fs::hard_link(&pending.0, &path) {
            Ok(()) => std::fs::File::open(directory)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("project key must be a private regular file".into());
    }
    // Malformed material is an error. Never replace a key that may already
    // protect persisted rows with a newly generated one.
    Ok(serde_json::from_reader(file)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarts_and_concurrent_hosts_keep_the_project_key() {
        let directory = tempfile::tempdir().unwrap();
        let keys = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = directory.path();
                    scope.spawn(move || load_material(path).unwrap())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for key in &keys {
            assert_eq!(key.project_id, keys[0].project_id);
            assert_eq!(key.key(), keys[0].key());
        }
        assert_eq!(
            load_material(directory.path()).unwrap().key(),
            keys[0].key()
        );
        let other = tempfile::tempdir().unwrap();
        assert_ne!(load_material(other.path()).unwrap().key(), keys[0].key());
    }

    #[test]
    fn malformed_or_public_keys_are_never_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        load_material(directory.path()).unwrap();
        let path = directory.path().join("project-data-key.json");
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_material(directory.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, b"interrupted write").unwrap();
        assert!(load_material(directory.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"interrupted write");
    }
}
