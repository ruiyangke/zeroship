use appbase_core::plugin::{Aggregation, MeterResource, Plugin, PluginContext};

/// SQLite-backed document database plugin.
///
/// In multi-tenant mode, each app gets its own database file
/// at `{data_dir}/db.sqlite` (data_dir is set by PluginContext).
/// In single-app mode, use `DbPlugin::with_path("appbase.db")` for a fixed path.
pub struct DbPlugin {
    /// If set, overrides the default `{data_dir}/db.sqlite` path.
    fixed_path: Option<String>,
}

impl DbPlugin {
    /// Create a db plugin that uses `{data_dir}/db.sqlite` per app.
    pub fn new() -> Self {
        Self { fixed_path: None }
    }

    /// Create a db plugin with a fixed database path (single-app mode).
    pub fn with_path(path: &str) -> Self {
        Self {
            fixed_path: Some(path.to_string()),
        }
    }
}

impl Plugin for DbPlugin {
    fn name(&self) -> &str {
        "db"
    }

    fn js_bridge(&self) -> &str {
        r#"
globalThis.db = {
  collection(name) {
    // TODO: re-implement with raw V8 ops
    return {
      insert: (doc) => { throw new Error("db plugin not yet implemented for raw V8"); },
      find: (filter) => { throw new Error("db plugin not yet implemented for raw V8"); },
      update: (id, updates) => { throw new Error("db plugin not yet implemented for raw V8"); },
      delete: (id) => { throw new Error("db plugin not yet implemented for raw V8"); },
    };
  },
};
"#
    }

    fn init(&self, _ctx: &mut PluginContext<'_>) {
        // TODO: re-implement with raw V8 ops
        // Will need to open SQLite connection and register V8 functions
    }

    fn meter_resources(&self) -> Vec<MeterResource> {
        vec![
            MeterResource {
                name: "db.reads".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
            MeterResource {
                name: "db.writes".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
        ]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid collection name: {0}")]
    InvalidName(String),
    #[error("Quota exceeded: {0}")]
    QuotaExceeded(#[from] appbase_core::plugin::QuotaDenied),
}

fn validate_collection_name(name: &str) -> Result<(), DbError> {
    if name.is_empty() || name.len() > 64 {
        return Err(DbError::InvalidName("must be 1-64 characters".into()));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(DbError::InvalidName(
            "only alphanumeric and underscore allowed".into(),
        ));
    }
    Ok(())
}
