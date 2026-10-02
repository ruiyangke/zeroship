//! Convert the testkit's plain data into this crate's types.
//!
//! `zeroship-data-testkit` speaks ids, strings and `serde_json::Value`, and its
//! surface never names a data-orm type. This module is the one place that knows
//! both: it builds a [`DbBinding`] through its public constructor, decodes a
//! field map into [`zeroship_data_orm::value::Value`] and composes the binding
//! store the isolate reads. Everything else in the fixtures module and its
//! callers sees the ORM types they always did.

use std::sync::Arc;
use zeroship_data_orm::binding::{DbBinding, DbRoute};
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};
use zeroship_data_testkit::data::{self as testkit, HarnessBinding};

/// The plain identity a creator [`DbBinding`] carries.
fn plain(binding: &DbBinding) -> HarnessBinding {
    let edge = binding
        .edge()
        .expect("a creator binding addresses a database");
    HarnessBinding::new(
        binding.app_id(),
        binding.deploy_token(),
        edge.database().clone(),
        edge.binding().clone(),
        edge.database_capability(),
    )
}

/// The ORM binding a plain identity composes.
fn to_binding(binding: &HarnessBinding) -> DbBinding {
    DbBinding::to_database(
        binding.app_id(),
        binding.deploy_token(),
        binding.database().clone(),
        binding.binding().clone(),
        binding.capability(),
    )
    .expect("a harness binding composes a legal role name")
}

pub(crate) use zeroship_data_testkit::data::harness_alias;
pub(crate) use zeroship_data_testkit::data::harness_database;
pub(crate) use zeroship_data_testkit::data::init_test_tracing;
pub(crate) use zeroship_data_testkit::data::tables;

#[must_use]
pub(crate) fn harness_binding(app_id: &str) -> DbBinding {
    to_binding(&testkit::harness_binding(app_id))
}

#[must_use]
pub(crate) fn harness_binding_at_deploy(app_id: &str, deploy_token: &str) -> DbBinding {
    to_binding(&testkit::harness_binding_at_deploy(app_id, deploy_token))
}

#[must_use]
pub(crate) fn harness_binding_at_deploy_for(
    binding: &DbBinding,
    deploy_token: &str,
) -> DbBinding {
    to_binding(&testkit::harness_binding_at_deploy_for(
        &plain(binding),
        deploy_token,
    ))
}

#[must_use]
pub(crate) fn harness_route(app_id: &str) -> DbRoute {
    let route = testkit::harness_route(app_id);
    DbRoute::new(route.app_id(), route.database().cloned())
}

pub(crate) async fn grant_all_runtime_table_columns(
    pool: &compio_postgres::Pool,
    binding: &DbBinding,
    table: &str,
) {
    testkit::grant_all_runtime_table_columns(pool, &plain(binding), table).await;
}

/// The binding store a harness hands the database service.
///
/// A worker installs what Control resolved; a harness installs what the
/// testkit composed, so an isolate minted for one of these apps reads the same
/// binding the fixture provisioned the cluster for. An app that is not listed
/// gets no `env.db`, which is the production refusal.
#[must_use]
pub(crate) fn harness_app_bindings<'a>(
    app_ids: impl IntoIterator<Item = &'a str>,
) -> Arc<SuppliedAppBindings> {
    let store = Arc::new(SuppliedAppBindings::new());
    for plain in testkit::harness_bindings(app_ids) {
        let binding = to_binding(&plain);
        let edge = binding
            .edge()
            .expect("a harness binding addresses a database");
        store
            .supply(binding.app_id(), ResolvedBinding::from(edge))
            .expect("a fresh store accepts its first binding");
    }
    store
}

pub(crate) mod roles {
    use super::{plain, DbBinding};
    use zeroship_data_orm::error::DbError;
    use zeroship_data_testkit::data::roles as testkit_roles;

    pub(crate) use zeroship_data_testkit::data::roles::BindingLadderOutcome;

    fn configured(binding: &DbBinding) -> Result<(), DbError> {
        if binding.database().is_none() || binding.session_role().is_none() {
            return Err(DbError::config(
                "binding_not_resolved",
                "a fixture ladder needs a binding that addresses a database and names a role",
            ));
        }
        Ok(())
    }

    pub(crate) fn set_local_role_sql(binding: &DbBinding) -> Result<String, DbError> {
        configured(binding)?;
        Ok(testkit_roles::set_local_role_sql(&plain(binding)))
    }

    pub(crate) async fn ensure_binding_ladder(
        pool: &compio_postgres::Pool,
        binding: &DbBinding,
    ) -> Result<BindingLadderOutcome, DbError> {
        configured(binding)?;
        testkit_roles::ensure_binding_ladder(pool, &plain(binding))
            .await
            .map_err(|error| zeroship_data_orm::backend::pg_error::coded_sql("fixture/roles", error))
    }

    pub(crate) async fn drop_binding_ladder(
        pool: &compio_postgres::Pool,
        binding: &DbBinding,
    ) -> Result<(), DbError> {
        configured(binding)?;
        testkit_roles::drop_binding_ladder(pool, &plain(binding))
            .await
            .map_err(|error| zeroship_data_orm::backend::pg_error::coded_sql("fixture/roles", error))
    }
}

pub(crate) mod schema {
    fn fields_json(fields: &zeroship_data_orm::value::Value) -> serde_json::Value {
        serde_json::to_value(fields).expect("the fixture field map encodes")
    }

    pub(crate) fn generated_fields(
        fields: zeroship_data_orm::value::Value,
    ) -> zeroship_data_orm::value::Value {
        let generated =
            zeroship_data_testkit::data::schema::generated_fields(fields_json(&fields));
        serde_json::from_value(generated).expect("the generated fixture fields decode")
    }
}
