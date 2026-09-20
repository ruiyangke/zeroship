//! Runtime input from the app's complete, verified module graph.

use std::sync::Arc;
use zeroship_bundle::{BlobStore, Manifest};

#[derive(Debug)]
pub struct AppExecutable {
    pub modules: Vec<zeroship_runtime::ModuleEntry>,
    pub descriptor: Option<String>,
}

#[expect(
    clippy::future_not_send,
    reason = "module loading stays on the worker's compio thread"
)]
pub async fn load_executable(
    manifest: &Manifest,
    blob_store: &Arc<dyn BlobStore>,
) -> Result<AppExecutable, String> {
    let executable = zeroship_bundle::LoadedWorker::load(
        manifest,
        blob_store.as_ref(),
        usize::try_from(zeroship_bundle::MAX_DECOMPRESSED_BYTES)
            .map_err(|_| "app executable budget is not representable")?,
    )
    .await
    .map_err(|error| format!("app executable load failed: {error}"))?;
    let modules = std::iter::once(executable.entry())
        .chain(
            executable
                .modules()
                .keys()
                .map(String::as_str)
                .filter(|name| *name != executable.entry()),
        )
        .map(|name| zeroship_runtime::ModuleEntry {
            specifier: name.into(),
            source: executable.modules()[name].clone(),
        })
        .collect();
    Ok(AppExecutable {
        modules,
        descriptor: descriptor_document(&executable),
    })
}

/// The runtime descriptor document for a loaded deployment: one entry per
/// database it declares, each carrying that database's schema.
///
/// `None` for a deployment that declares no database, which is the
/// schema-less case the runtime treats as "install nothing".
#[must_use]
pub fn descriptor_document(executable: &zeroship_bundle::LoadedWorker) -> Option<String> {
    let databases: Vec<_> = executable
        .databases()
        .iter()
        .map(|database| zeroship_runtime::databases::RuntimeDatabase {
            label: database.label.clone(),
            database_id: database.database_id.as_str().to_owned(),
            primary: database.primary,
            schema: database.schema.clone(),
        })
        .collect();
    (!databases.is_empty())
        .then(|| zeroship_runtime::databases::RuntimeDatabases::document(databases))
}


/// The primary database's schema, out of a stored descriptor DOCUMENT.
///
/// `AppExecutable` carries the document as text because that is what the
/// isolate is handed. A test that means "the schema I stored round-trips" has
/// to reach past the envelope, and comparing the whole document instead makes
/// the assertion fail the day another database joins the manifest - for a
/// reason that has nothing to do with what it is checking.
#[cfg(test)]
pub(crate) fn primary_schema_json(descriptor: Option<&str>) -> serde_json::Value {
    let document: serde_json::Value =
        serde_json::from_str(descriptor.expect("a descriptor is stored")).expect("valid JSON");
    document["databases"]
        .as_array()
        .expect("the document carries a databases array")
        .iter()
        .find(|entry| entry["primary"] == serde_json::Value::Bool(true))
        .expect("exactly one entry is primary")["schema"]
        .clone()
}
