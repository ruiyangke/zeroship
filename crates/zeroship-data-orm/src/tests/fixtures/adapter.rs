//! Convert the testkit's plain data into this crate's types.
//!
//! `zeroship-data-testkit` speaks ids, strings and `serde_json::Value`, and its
//! surface never names a data-orm type. This module is the one place that knows
//! both: it builds a [`DbBinding`] through its public constructor and decodes a
//! field map into [`crate::value::Value`]. Everything else in the fixtures
//! module and its callers sees the ORM types they always did.

use crate::binding::{DbBinding, DbRoute};
use crate::error::DbError;
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
pub(crate) use zeroship_data_testkit::data::test_app_id_from;
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
pub(crate) fn harness_binding_for_alias(alias: &str) -> DbBinding {
    to_binding(&testkit::harness_binding_for_alias(alias))
}

#[must_use]
pub(crate) fn harness_route(app_id: &str) -> DbRoute {
    let route = testkit::harness_route(app_id);
    DbRoute::new(route.app_id(), route.database().cloned())
}

#[must_use]
pub(crate) fn harness_capability_role(binding: &DbBinding) -> String {
    testkit::harness_capability_role(&plain(binding))
}

pub(crate) async fn grant_all_runtime_table_columns(
    pool: &compio_postgres::Pool,
    binding: &DbBinding,
    table: &str,
) {
    testkit::grant_all_runtime_table_columns(pool, &plain(binding), table).await;
}

pub(crate) async fn grant_runtime_select_columns(
    pool: &compio_postgres::Pool,
    binding: &DbBinding,
    table: &str,
    columns: &[&str],
) {
    testkit::grant_runtime_select_columns(pool, &plain(binding), table, columns).await;
}

pub(crate) mod roles {
    use super::{plain, DbBinding, DbError};
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
            .map_err(|error| crate::backend::pg_error::coded_sql("fixture/roles", error))
    }

    pub(crate) async fn drop_binding_ladder(
        pool: &compio_postgres::Pool,
        binding: &DbBinding,
    ) -> Result<(), DbError> {
        configured(binding)?;
        testkit_roles::drop_binding_ladder(pool, &plain(binding))
            .await
            .map_err(|error| crate::backend::pg_error::coded_sql("fixture/roles", error))
    }
}

pub(crate) mod schema {
    fn fields_json(fields: &crate::value::Value) -> serde_json::Value {
        serde_json::to_value(fields).expect("the fixture field map encodes")
    }

    pub(crate) fn fixture_table_sql(
        schema: &crate::sql::SchemaName,
        collection: &str,
        fields: &crate::value::Value,
        fks: &zeroship_migrate::schema::query::FkEmission<'_>,
    ) -> Result<String, zeroship_migrate::schema::query::QueryError> {
        zeroship_data_testkit::data::schema::fixture_table_sql(
            schema,
            collection,
            &fields_json(fields),
            fks,
        )
    }

    pub(crate) fn fixture_table_sql_sqlite(
        schema: &crate::sql::SchemaName,
        collection: &str,
        fields: &crate::value::Value,
        fks: &zeroship_migrate::schema::query::FkEmission<'_>,
    ) -> Result<String, zeroship_migrate::schema::query::QueryError> {
        zeroship_data_testkit::data::schema::fixture_table_sql_sqlite(
            schema,
            collection,
            &fields_json(fields),
            fks,
        )
    }

    pub(crate) fn fixture_indexes(
        schema: &crate::sql::SchemaName,
        collection: &str,
        fields: &crate::value::Value,
    ) -> Result<Vec<zeroship_migrate::schema::query::IndexSpec>, zeroship_migrate::schema::query::QueryError>
    {
        zeroship_data_testkit::data::schema::fixture_indexes(
            schema,
            collection,
            &fields_json(fields),
        )
    }

    pub(crate) fn generated_fields(fields: crate::value::Value) -> crate::value::Value {
        let generated =
            zeroship_data_testkit::data::schema::generated_fields(fields_json(&fields));
        serde_json::from_value(generated).expect("the generated fixture fields decode")
    }
}
