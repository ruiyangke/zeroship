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

/// Create `directory` and its parents, owner-only where the system has POSIX
/// modes. On Windows the directory takes the access list it inherits.
pub fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(directory)
}

/// Make the entries created in `directory` durable.
///
/// The standard library offers no directory flush on Windows, so this does
/// nothing there, and the entry's durability is the file system's metadata
/// journal's.
pub fn sync_directory(directory: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let _ = directory;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(directory)?.sync_all()
    }
}

/// Open the published key for reading without following a link.
///
/// Unix refuses a symbolic link with `O_NOFOLLOW`. Windows opens the link
/// itself (`FILE_FLAG_OPEN_REPARSE_POINT`), which is not a regular file, so the
/// caller's regular-file check refuses it there.
fn open_unfollowed(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        /// `FILE_FLAG_OPEN_REPARSE_POINT` from the Windows SDK's `WinBase.h`.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

/// Whether `metadata` lets anyone but its owner reach the file. Windows has no
/// POSIX mode to read: there the key's privacy is the access list its
/// directory hands down, which this check does not inspect.
fn reachable_by_others(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o077 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn load_material(directory: &Path) -> Result<ProjectDataKey, Box<dyn std::error::Error>> {
    create_private_dir(directory)?;
    let path = directory.join("project-data-key.json");
    if !path.try_exists()? {
        let temporary = directory.join(format!(".project-data-key-{}", uuid::Uuid::new_v4()));
        struct Pending(std::path::PathBuf);
        impl Drop for Pending {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let pending = Pending(temporary);
        let material = ProjectDataKey::generate(ProjectId::mint());
        serde_json::to_writer(&mut file, &material)?;
        file.flush()?;
        file.sync_all()?;
        // Publish complete material without replacing a concurrent winner.
        match std::fs::hard_link(&pending.0, &path) {
            Ok(()) => sync_directory(directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file = open_unfollowed(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || reachable_by_others(&metadata) {
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
    fn malformed_keys_are_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        load_material(directory.path()).unwrap();
        let path = directory.path().join("project-data-key.json");
        std::fs::write(&path, b"interrupted write").unwrap();
        assert!(load_material(directory.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"interrupted write");
    }

    #[cfg(unix)]
    #[test]
    fn public_keys_are_refused_and_never_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        load_material(directory.path()).unwrap();
        let path = directory.path().join("project-data-key.json");
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_material(directory.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_material(directory.path()).is_ok(), "the control: an owner-only key loads");
    }

    /// A link where the key belongs is refused, whatever it points at: a
    /// project directory checked out or unpacked from elsewhere must not hand
    /// the host another file's bytes as its key. Windows lets an account
    /// without the symbolic-link privilege create no link, and the test then
    /// fails naming it rather than passing over nothing.
    #[test]
    fn a_linked_key_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        load_material(elsewhere.path()).unwrap();
        create_private_dir(directory.path()).unwrap();
        let target = elsewhere.path().join("project-data-key.json");
        let link = directory.path().join("project-data-key.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap_or_else(|error| {
            panic!("creating a file link needs the symbolic-link privilege: {error}")
        });
        assert!(load_material(directory.path()).is_err());
        assert!(load_material(elsewhere.path()).is_ok(), "the control: the target itself loads");
    }

    /// Publishing a key ends with a directory sync on every system, so a system
    /// that cannot flush a directory must not fail it.
    #[test]
    fn syncing_a_directory_succeeds_on_every_system() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("entry"), b"published").unwrap();
        sync_directory(directory.path()).unwrap();
    }
}
