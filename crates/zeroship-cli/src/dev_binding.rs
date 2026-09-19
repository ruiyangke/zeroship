//! The database binding the standalone development host runs under.
//!
//! A deployed app's binding is resolved by the control plane and delivered to
//! the worker. `zeroship dev` has no control plane, so it mints one and keeps
//! it beside the project key.
//!
//! **It is persisted rather than minted per process, and that is the whole
//! point.** The database id names the physical schema, so a binding that
//! changed on every start would put each run's tables in a new schema and
//! leave the previous run's data unreachable.

use std::{fs::OpenOptions, io::Write, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use zeroship_core::{AppId, BindingId, DatabaseId};
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};

/// The epoch a development binding is minted at.
///
/// The dev tier has no migration service advancing epochs, so it stays at the
/// first one and the role name it composes never rotates.
const DEV_EPOCH: u32 = 1;

#[derive(Serialize, Deserialize)]
struct DevBindingFile {
    database_id: String,
    binding_id: String,
    epoch: u32,
}

/// Load, or mint and persist, this project's development binding and install
/// it for `app_id`.
///
/// # Errors
///
/// Reports a directory or file that cannot be read, and material that does not
/// parse. Malformed material is an error rather than a reason to mint a new
/// binding: a new database id would abandon the schema the existing rows are
/// in.
pub(crate) fn load(
    directory: &Path,
    app_id: &AppId,
) -> Result<Arc<SuppliedAppBindings>, String> {
    let material = load_material(directory).map_err(|error| {
        format!(
            "cannot open the local database binding in {}: {error}",
            directory.display()
        )
    })?;
    let bindings = Arc::new(SuppliedAppBindings::new());
    bindings
        .supply(app_id.as_str(), material)
        .map_err(|error| error.to_string())?;
    Ok(bindings)
}

fn load_material(directory: &Path) -> Result<ResolvedBinding, Box<dyn std::error::Error>> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let path = directory.join("dev-database-binding.json");
    if !path.try_exists()? {
        let temporary = directory.join(format!(".dev-database-binding-{}", uuid::Uuid::new_v4()));
        struct Pending(std::path::PathBuf);
        impl Drop for Pending {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let pending = Pending(temporary);
        serde_json::to_writer(
            &mut file,
            &DevBindingFile {
                database_id: DatabaseId::mint().into_string(),
                binding_id: BindingId::mint().into_string(),
                epoch: DEV_EPOCH,
            },
        )?;
        file.flush()?;
        file.sync_all()?;
        // Publish complete material without replacing a concurrent winner.
        match std::fs::hard_link(&pending.0, &path) {
            Ok(()) => std::fs::File::open(directory)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file = OpenOptions::new().read(true).open(&path)?;
    let stored: DevBindingFile = serde_json::from_reader(file)?;
    Ok(ResolvedBinding {
        database: DatabaseId::parse(&stored.database_id)?,
        binding: BindingId::parse(&stored.binding_id)?,
        epoch: stored.epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restart reads the binding it wrote, and concurrent hosts agree.
    ///
    /// The schema is derived from the database id, so a second process that
    /// minted its own would address a different schema and see no rows.
    #[test]
    fn restarts_and_concurrent_hosts_keep_the_development_binding() {
        let directory = tempfile::tempdir().unwrap();
        let first = std::thread::scope(|scope| {
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
        for observed in &first {
            assert_eq!(
                observed, &first[0],
                "concurrent hosts must agree on one development binding"
            );
        }
        assert_eq!(
            load_material(directory.path()).unwrap(),
            first[0],
            "a restart must read the binding the first start wrote"
        );

        // Control: a different project directory mints a different binding, so
        // the equality above is persistence rather than a constant.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(load_material(other.path()).unwrap(), first[0]);
    }
}
