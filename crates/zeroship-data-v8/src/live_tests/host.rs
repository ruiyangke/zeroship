//! Private adapter setup for tests in this crate.
use crate::{context, ctx_mut};
use std::rc::Rc;
use zeroship_data_orm::{connection::ConnectionFactory, encryption};

/// Column-key source currently supplied to the adapter's worker thread.
pub fn isolate_key_source() -> encryption::LocalKeySource {
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
pub fn supply_root_keys_for_tests(roots: &[(&str, &str)]) -> SuppliedRootKeysGuard {
    let keys = Rc::new(encryption::SuppliedRootKeys::new());
    for (key_id, hex) in roots {
        keys.insert_hex(key_id, hex)
            .unwrap_or_else(|e| panic!("fixture root key '{key_id}' must parse: {e:?}"));
    }
    ctx_mut(|c| c.set_supplied_root_keys(Some(Rc::clone(&keys))));
    SuppliedRootKeysGuard { _keys: keys }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct SuppliedRootKeysGuard {
    _keys: Rc<encryption::SuppliedRootKeys>,
}

impl Drop for SuppliedRootKeysGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.set_supplied_root_keys(None));
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
