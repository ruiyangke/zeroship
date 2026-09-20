//! The runtime descriptor document: one entry per database a deployment
//! declares.
//!
//! The host resolves `Manifest.runtime_descriptor` and each entry's blob, then
//! hands the runtime ONE document carrying all of them. The runtime validates
//! it and passes the parsed value to every native plugin lifecycle hook; the
//! database plugin is the one that interprets it.
//!
//! **The label is a name inside this app's own artifact.** It becomes the
//! member name on `env.databases` and nothing else: every server-side map keys
//! on `database_id`, because two co-resident apps both calling a database
//! `main` would compare equal.

use serde::{Deserialize, Serialize};

/// The document version. Bumped when the ENVELOPE changes; each entry's
/// `schema` carries its own version.
pub const DOCUMENT_VERSION: u64 = 1;

/// One database of a deployment, with its schema descriptor resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDatabase {
    /// The creator's local label, and the member name on `env.databases`.
    pub label: String,
    /// The database's typed id, as text. Every host-side map keys on it.
    pub database_id: String,
    /// Whether this database is `env.db`. Exactly one entry carries it.
    pub primary: bool,
    /// This database's v2 `RuntimeSchemaDescriptor`.
    pub schema: serde_json::Value,
}

/// The whole document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDatabases {
    pub version: u64,
    pub databases: Vec<RuntimeDatabase>,
}

impl RuntimeDatabases {
    /// The document for a deployment declaring exactly these databases.
    ///
    /// # Panics
    /// Never; serialization of owned JSON values cannot fail.
    #[must_use]
    pub fn document(databases: Vec<RuntimeDatabase>) -> String {
        serde_json::to_string(&Self {
            version: DOCUMENT_VERSION,
            databases,
        })
        .expect("owned JSON values serialize")
    }

    /// The document for a deployment declaring ONE database, which is
    /// therefore its primary. The convenience every single-database host and
    /// test needs, so neither hand-writes the envelope.
    ///
    /// # Errors
    /// Returns the parse error when `schema` is not valid JSON.
    pub fn single(label: &str, database_id: &str, schema: &str) -> Result<String, String> {
        let schema: serde_json::Value = serde_json::from_str(schema)
            .map_err(|error| format!("runtime: database schema is not valid JSON: {error}"))?;
        Ok(Self::document(vec![RuntimeDatabase {
            label: label.to_owned(),
            database_id: database_id.to_owned(),
            primary: true,
            schema,
        }]))
    }
}

/// Read the databases out of a validated document.
///
/// Returns an empty slice view for anything else, which cannot happen after
/// `validate`: the runtime rejects a malformed document before any plugin sees
/// it.
#[must_use]
pub fn databases_of(document: &serde_json::Value) -> Vec<RuntimeDatabase> {
    document
        .get("databases")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The primary database of a validated document, the one `env.db` reaches.
#[must_use]
pub fn primary_of(document: &serde_json::Value) -> Option<RuntimeDatabase> {
    databases_of(document)
        .into_iter()
        .find(|database| database.primary)
}
