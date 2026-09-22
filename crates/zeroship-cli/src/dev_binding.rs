//! The database binding the standalone development host runs under.
//!
//! A deployed app's binding is resolved by the control plane and delivered to
//! the worker. `zeroship dev` has no control plane, so it resolves one here
//! from the two facts a dev tier can actually know: the database the project
//! DECLARES, and an edge to it this machine owns.
//!
//! **The database id is read, never minted.** `databases.<label>.id` is a
//! required key of `zeroship.jsonc`, so a config that parses has already named
//! the database. That id is what the packed archive carries, what
//! `bind_runtime_descriptor` looks the binding up by, and - through
//! `zeroship_core::database_derivation::schema_name` - what the `SQLite` backend
//! renders into the file it attaches. A host that minted its own would be a
//! second identity for one database: `env.db` would find no binding for the
//! declared id, and the file the runtime attached would be one no other reader
//! could name.
//!
//! **The binding id IS minted, and persisted.** Nothing declares it and no
//! other reader has to predict it - it is this machine's edge to the declared
//! database. It is persisted rather than minted per process because a restart
//! is not a reissued edge.

use std::{fs::OpenOptions, io::Write, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use zeroship_core::{AppId, BindingId, DatabaseId};
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};

/// The capability a development binding is minted with.
///
/// Read-write, because `zeroship dev` is the creator's own database on the
/// creator's own machine and there is no control plane to have declared
/// anything else. It is a constant here rather than a field of the persisted
/// material: the file records the binding's IDENTITY, which a restart must not
/// change, and the dev tier has exactly one capability to mint. A stored
/// capability would be a value nothing on this tier can set and every reader
/// would have to have an opinion about when it was absent.
const DEV_CAPABILITY: zeroship_core::database_role::DatabaseCapability =
    zeroship_core::database_role::DatabaseCapability::ReadWrite;

#[derive(Serialize, Deserialize)]
struct DevBindingFile {
    binding_id: String,
}

/// Install this project's development binding for `app_id`.
///
/// `database` is the id the project declares for the app's primary database.
/// `None` is a project that declares no database at all, and the store then
/// stays EMPTY: `env.db` refuses, which is the honest answer for an app with
/// no database. Nothing is minted to fill the gap, because a minted id names a
/// schema the app never declared and a file no other reader can find.
///
/// # Errors
///
/// Reports a directory or file that cannot be read, and material that does not
/// parse. Malformed material is an error rather than a reason to mint a new
/// edge: a new binding id would abandon the role the existing one was granted.
pub(crate) fn load(
    directory: &Path,
    app_id: &AppId,
    database: Option<&DatabaseId>,
) -> Result<Arc<SuppliedAppBindings>, String> {
    let bindings = Arc::new(SuppliedAppBindings::new());
    let Some(database) = database else {
        return Ok(bindings);
    };
    let binding = load_binding_id(directory).map_err(|error| {
        format!(
            "cannot open the local database binding in {}: {error}",
            directory.display()
        )
    })?;
    bindings
        .supply(
            app_id.as_str(),
            ResolvedBinding {
                database: database.clone(),
                binding,
                capability: DEV_CAPABILITY,
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(bindings)
}

fn load_binding_id(directory: &Path) -> Result<BindingId, Box<dyn std::error::Error>> {
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
                binding_id: BindingId::mint().into_string(),
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
    Ok(BindingId::parse(&stored.binding_id)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARED: &str = "dbs_03evr3oqx1200cfkwyailh8l8";

    fn declared() -> DatabaseId {
        DatabaseId::parse(DECLARED).expect("the fixture is a canonical database id")
    }

    /// A restart reads the edge it wrote, and concurrent hosts agree.
    ///
    /// The binding role is granted per binding id, so a second process that
    /// minted its own would narrow to a role nothing granted.
    #[test]
    fn restarts_and_concurrent_hosts_keep_the_development_binding() {
        let directory = tempfile::tempdir().unwrap();
        let first = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = directory.path();
                    scope.spawn(move || load_binding_id(path).unwrap())
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
            load_binding_id(directory.path()).unwrap(),
            first[0],
            "a restart must read the binding the first start wrote"
        );

        // Control: a different project directory mints a different edge, so
        // the equality above is persistence rather than a constant.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(load_binding_id(other.path()).unwrap(), first[0]);
    }

    /// The installed binding names the DECLARED database, on every start.
    ///
    /// The store is keyed on the database id, so this is the value `env.db`
    /// looks itself up by. Two starts, because the edge is persisted while the
    /// database id is read each time: a host that had stashed the database
    /// alongside the edge would pass the first arm and the second would still
    /// read what the first wrote.
    #[test]
    fn the_installed_binding_names_the_declared_database() {
        let directory = tempfile::tempdir().unwrap();
        let app = zeroship_core::app_id::local_dev_app_id();
        for start in 0..2 {
            let bindings = load(directory.path(), &app, Some(&declared())).unwrap();
            let installed = bindings.live_bindings_for(app.as_str());
            assert_eq!(
                installed.keys().collect::<Vec<_>>(),
                vec![&declared()],
                "start {start} must bind the declared database"
            );
        }

        // Control: a second project declaring a DIFFERENT id binds that one,
        // so the arm above is reading the argument and not a constant.
        let other_directory = tempfile::tempdir().unwrap();
        let other = DatabaseId::parse("dbs_03evr3oqx1200mf301taes352").unwrap();
        assert_ne!(other, declared(), "the control needs two ids");
        let bindings = load(other_directory.path(), &app, Some(&other)).unwrap();
        assert_eq!(
            bindings
                .live_bindings_for(app.as_str())
                .keys()
                .collect::<Vec<_>>(),
            vec![&other]
        );
    }

    /// A project declaring no database installs no binding and mints nothing.
    #[test]
    fn a_project_with_no_declared_database_binds_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let app = zeroship_core::app_id::local_dev_app_id();
        let bindings = load(directory.path(), &app, None).unwrap();
        assert!(
            bindings.live_bindings_for(app.as_str()).is_empty(),
            "an app with no declared database has no edge"
        );
        assert!(
            !directory.path().join("dev-database-binding.json").exists(),
            "nothing is minted for a database that was never declared"
        );
    }
}
