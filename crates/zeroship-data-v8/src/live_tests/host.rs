//! Private adapter setup for tests in this crate.
use crate::{context, ctx_mut};
use std::rc::Rc;
use zeroship_data_orm::{connection::ConnectionFactory, encryption};

/// Column-key source currently supplied to the adapter's worker thread.
pub fn isolate_key_source() -> encryption::ProjectKeySource {
    context::isolate_key_source()
}

#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    let connection = ConnectionFactory::for_url(url).expect("valid fixture configuration");
    ctx_mut(|context| context.install_connection(connection));
}

#[doc(hidden)]
pub fn reset_context_for_tests() {
    ctx_mut(|c| *c = context::ThreadDbContext::new());
}

#[doc(hidden)]
#[must_use]
pub fn supply_project_key_for_tests(app_ids: &[&str], hex: &str) -> SuppliedProjectKeysGuard {
    let keys = Rc::new(encryption::SuppliedProjectKeys::new());
    keys.insert_hex("fixture_project", hex)
        .expect("fixture project key");
    for app_id in app_ids {
        keys.bind_app(app_id, "fixture_project")
            .expect("fixture app binding");
    }
    ctx_mut(|c| c.set_supplied_project_keys(Some(Rc::clone(&keys))));
    SuppliedProjectKeysGuard { _keys: keys }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct SuppliedProjectKeysGuard {
    _keys: Rc<encryption::SuppliedProjectKeys>,
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
        vec![(collection.to_owned(), schema)],
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
