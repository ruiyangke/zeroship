//! Private adapter setup for tests in this crate.
use crate::{context, ctx_mut};
use std::sync::Arc;
use zeroship_data_orm::{connection::ConnectionFactory, encryption};

/// Column-key source currently supplied to the adapter's worker thread.
pub(crate) fn key_source() -> encryption::ProjectKeySource {
    context::isolate_key_source()
}

pub(crate) fn set_database_url(url: &str) {
    let connection = ConnectionFactory::for_url(url).expect("valid fixture configuration");
    ctx_mut(|context| context.install_connection(connection));
}

pub(crate) fn reset_context() {
    ctx_mut(|c| *c = context::ThreadDbContext::new());
}

#[must_use]
pub(crate) fn supply_project_key(app_ids: &[&str], hex: &str) -> SuppliedProjectKeysGuard {
    let keys = Arc::new(encryption::SuppliedProjectKeys::new());
    keys.insert_hex("fixture_project", hex)
        .expect("fixture project key");
    for app_id in app_ids {
        keys.bind_app(app_id, "fixture_project")
            .expect("fixture app binding");
    }
    ctx_mut(|c| c.set_supplied_project_keys(Some(Arc::clone(&keys))));
    SuppliedProjectKeysGuard { _keys: keys }
}

#[derive(Debug)]
pub(crate) struct SuppliedProjectKeysGuard {
    _keys: Arc<encryption::SuppliedProjectKeys>,
}

impl Drop for SuppliedProjectKeysGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.set_supplied_project_keys(None));
    }
}

pub(crate) fn binding(app_id: impl Into<String>) -> zeroship_data_orm::binding::DbBinding {
    let app_id = app_id.into();
    let schema = zeroship_data_sql::SchemaName::new(&app_id).expect("fixture schema name");
    zeroship_data_orm::binding::DbBinding::new(
        app_id,
        zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
        schema,
    )
}
pub(crate) fn install_schema(
    binding: &zeroship_data_orm::binding::DbBinding,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    zeroship_data_orm::descriptor::install_collections(
        binding,
        vec![(
            collection.to_owned(),
            super::schema::generated_fields(schema),
        )],
    )
    .expect("install fixture descriptor");
}
pub(crate) fn install_cold_schema(
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    install_schema(&binding(app_id), collection, schema);
}

/// Share explicitly supplied keys with the real service composition.
pub(crate) fn project_keys() -> Arc<encryption::SuppliedProjectKeys> {
    context::with(|context| context.project_keys().unwrap_or_default())
}
