//! V8 bindings for the shared data ORM, exposed through `env.db`.
//!
//! The host supplies a validated ORM connection factory. This crate captures
//! JavaScript arguments and request identity, delegates operations to the ORM,
//! and materializes native results as V8 values and promises. Subscription and
//! transaction wrappers release their ORM handles on close or garbage collection.
//!
//! Backend construction, pooling, SQL execution, protection policy and CDC
//! lifecycle live in the ORM. This adapter owns the isolate integration.

#![recursion_limit = "256"]

use std::rc::Rc;
use zeroship_data_orm::connection::ConnectionFactory;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

use crate::context::with_mut as ctx_mut;
use zeroship_data_orm::error::DbError;

// The test-helpers feature exposes adapter lifecycle fixtures. Database
// internals are accessed directly through the ORM and SQL crates.

// Private imports used to compose ORM operations with isolate state.
use zeroship_data_orm::cdc::{broker, read_set};
#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_orm::tx_lanes;
use zeroship_data_orm::{
    backend, descriptor, metrics, system_shape_charter, transaction, tx_route,
};
use zeroship_data_sql::compile;

pub(crate) mod context;
pub mod op_error;
/// Fixtures for embedding the adapter in integration tests.
#[cfg(any(test, feature = "test-helpers"))]
pub mod testing;
pub(crate) mod v8_bridge;
pub mod v8_classes;

pub mod service;

// Async-scoped transaction marker. Read by `transaction` to tell a
// genuinely NESTED `transaction()` call from one that merely overlaps
// another in time; see the module docs for the defect that distinction
// closes.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod tx_scope;
#[cfg(feature = "test-helpers")]
pub mod tx_scope;

/// The database plugin — registers `zeroship.db.*` methods.
///
/// **The prototype, not a per-runtime object.** One instance is minted by
/// [`service::DbService::new`] at composition and every runtime on every worker
/// thread clones the same `Arc`. There is deliberately no public constructor:
/// a `DbPlugin` is a view of validated service configuration, and minting one
/// beside the service would be a second, unvalidated configuration.
#[derive(Debug)]
pub struct DbPlugin {
    connection: ConnectionFactory,
    cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
    assignments: system_shape_charter::AssignmentPlan,
}
impl DbPlugin {
    pub(crate) fn new(
        connection: ConnectionFactory,
        cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
        meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
        assignments: system_shape_charter::AssignmentPlan,
    ) -> Self {
        Self {
            connection,
            cdc_relay,
            meter,
            assignments,
        }
    }
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str {
        "db"
    }

    fn name(&self) -> &str {
        "database"
    }

    /// Mint a `Db` v8_class instance as the namespace value for
    /// `env.db`. The runtime then attaches the Db-scoped entry points
    /// registered via [`Self::register`] on top. The `.collection(name)`
    /// `#[v8_method]` on the instance returns a `Collection` v8_class
    /// wrapper whose methods prepare ORM operations through the shared
    /// adapter dispatch.
    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        v8_classes::db::mint_db(scope, app_id)
    }

    fn bind_runtime_descriptor(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        app_id: &str,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        // The runtime validates the complete descriptor before invoking this
        // hook. Collect every field map before mutating the shared thread
        // context anyway, so a future validator change cannot publish a
        // partial schema on error.
        let schemas = descriptor_schemas(descriptor)?;
        // Same refusal as `mint_db`: an app id that is not a legal schema name
        // has no binding to key the descriptor under, so publish nothing rather
        // than key it under a schema that cannot be addressed.
        let binding = v8_classes::db::binding_for_isolate(scope, app_id)
            .ok_or_else(|| format!("app id {app_id:?} is not a legal database schema name"))?;
        zeroship_data_orm::descriptor::install_collections(&binding, schemas)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn register(&self, _: &mut NativeRegistrar) {
        ctx_mut(|context| {
            context.install_connection(self.connection.clone());
            context.set_cdc_relay(self.cdc_relay.clone());
        });
        metrics::stamp(self.meter.clone());
        system_shape_charter::stamp(Rc::new(self.assignments.clone()));
    }
}

fn descriptor_schemas(
    descriptor: Option<&serde_json::Value>,
) -> Result<Vec<(String, zeroship_data_sql::value::Value)>, String> {
    let Some(descriptor) = descriptor else {
        return Ok(Vec::new());
    };
    let collections = descriptor
        .get("collections")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "descriptor has no object `collections` field".to_string())?;

    collections
        .iter()
        .map(|(name, collection)| {
            let fields = collection
                .get("fields")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    format!("descriptor collection {name:?} has no object `fields` field")
                })?;
            Ok((
                name.clone(),
                zeroship_data_sql::value::to_value(fields).map_err(|e| e.to_string())?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod runtime_descriptor_binding_tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use zeroship_data_sql::value;
    use zeroship_runtime::{RuntimeState, SharedState, init_v8};

    use super::*;

    const APP: &str = "app_native_descriptor";
    const DEPLOY: &str = "deploy_native_descriptor";

    fn install_runtime_state(scope: &mut v8::PinScope<'_, '_>) {
        let mut env = HashMap::new();
        env.insert("APP_ID".to_string(), APP.to_string());
        env.insert("ZEROSHIP_DEPLOY_ID".to_string(), DEPLOY.to_string());
        let state: SharedState = Rc::new(RefCell::new(RuntimeState::new(env, None, None)));
        scope.set_slot(state);
    }

    fn plugin() -> std::sync::Arc<DbPlugin> {
        service::DbService::new(service::DbServiceConfig {
            connection: zeroship_data_orm::connection::ConnectionFactory::for_url(
                "sqlite:descriptor-test.sqlite",
            )
            .expect("valid database configuration"),
            cdc_relay: None,
            meter: None,
        })
        .expect("db service")
        .plugin()
    }

    #[test]
    fn validated_runtime_descriptor_makes_declared_collection_serveable() {
        crate::testing::reset_context_for_tests();
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        install_runtime_state(scope);

        let runtime_descriptor = value!({
            "version": 2,
            "collections": {
                "users": {
                    "fields": {
                        "id": { "type": "id", "idPrefix": "usr" },
                        "email": { "type": "string", "required": true }
                    },
                    "options": {
                        "softDelete": false,
                        "versioning": true,
                        "strictness": "strict"
                    },
                    "indexes": []
                }
            }
        });
        plugin()
            .bind_runtime_descriptor(
                scope,
                APP,
                Some(&serde_json::to_value(&runtime_descriptor).unwrap()),
            )
            .expect("bind descriptor");

        let binding = zeroship_data_orm::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_data_sql::SchemaName::new(APP).unwrap(),
        );
        let schema = descriptor::collection_schema(&binding, "users")
            .expect("declared collection must resolve before any read");
        assert_eq!(
            schema.as_ref(),
            &runtime_descriptor["collections"]["users"]["fields"],
            "native boot must publish the descriptor's field map verbatim"
        );
    }

    #[test]
    fn schema_less_runtime_replaces_binding_with_an_empty_view() {
        crate::testing::reset_context_for_tests();
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        install_runtime_state(scope);

        let runtime_descriptor = value!({
            "version": 2,
            "collections": {
                "stale": {
                    "fields": { "id": { "type": "id" } },
                    "options": { "softDelete": false, "versioning": false },
                    "indexes": []
                }
            }
        });
        let plugin = plugin();
        plugin
            .bind_runtime_descriptor(
                scope,
                APP,
                Some(&serde_json::to_value(&runtime_descriptor).unwrap()),
            )
            .expect("bind descriptor");
        plugin
            .bind_runtime_descriptor(scope, APP, None)
            .expect("bind schema-less runtime");

        let binding = zeroship_data_orm::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_data_sql::SchemaName::new(APP).unwrap(),
        );
        let error = descriptor::collection_schema(&binding, "stale")
            .expect_err("schema-less binding must declare no collection");
        assert!(
            format!("{error:?}").contains("collection_not_declared"),
            "missing descriptor entry must remain a loud typed refusal: {error:?}"
        );
        assert!(
            descriptor::declared_collections(&binding).is_empty(),
            "schema-less transaction view source must be empty"
        );
    }
}

/// Install project keys and app bindings supplied by the trusted worker host.
/// Existing backends retain their source; install this before initializing them.
pub fn set_project_keys(keys: std::rc::Rc<zeroship_data_orm::encryption::SuppliedProjectKeys>) {
    context::with_mut(|context| context.set_supplied_project_keys(Some(keys)));
}

/// Open the ORM connection registered for this worker thread, if configured.
pub async fn initialize_backend() -> Result<(), DbError> {
    if let Some(connection) = context::with(|context| context.connection()) {
        connection.ensure(context::isolate_key_source()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod backend_init_tests {
    use super::{context, ctx_mut, initialize_backend};
    use crate::testing::set_db_url_for_tests;

    fn set_fresh_db_url(url: &str) {
        ctx_mut(|c| c.clear_backend());
        set_db_url_for_tests(url);
    }

    /// Eight concurrent cold inits open exactly ONE backend.
    ///
    /// The count is the assertion. "A usable backend is installed" - which is
    /// all this test asserted until the open counter existed - passes with no
    /// singleflight at all: eight sequential opens leave one installed too,
    /// because each overwrites the last. Remove `begin_backend_init` and this
    /// arm reports 8.
    ///
    /// It is also the liveness proof for [`zeroship_data_orm::connection::backend_open_count`]
    /// itself. The worker's "building the plugin set opens no pool" guard reads
    /// that counter and asserts it did NOT move; a counter wired to nothing
    /// satisfies that forever. This arm shows it moves when a backend really is
    /// opened.
    #[compio::test]
    async fn concurrent_sqlite_lazy_init_shares_one_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", dir.path().join("cold-init.sqlite").display());
        set_fresh_db_url(&url);
        let opens_before = zeroship_data_orm::connection::backend_open_count();

        const CONCURRENCY: usize = 8;
        let handles = (0..CONCURRENCY)
            .map(|_| compio::runtime::spawn(async { initialize_backend().await }))
            .collect::<Vec<_>>();

        for handle in handles {
            handle
                .await
                .expect("join concurrent init task")
                .expect("sqlite init should succeed");
        }

        assert!(
            context::with(|c| c.backend().is_some()),
            "concurrent init calls should leave a usable backend installed"
        );
        assert_eq!(
            zeroship_data_orm::connection::backend_open_count() - opens_before,
            1,
            "{CONCURRENCY} concurrent cold inits must open ONE backend, not one each",
        );
    }
}

/// The workflow journal schema name is derived TWICE, in two crates that
/// deliberately do not depend on each other, and until this module existed the
/// only thing holding them in agreement was a comment.
///
/// `zeroship-migrate-server` WRITES the schema
/// (`provisioning::workflow_journal_schema_name`, called at
/// `provisioning.rs:327` and `apply.rs:1526`); `zeroship-workflow` READS
/// it (`store::pg::app_schema_for`, called at `store/pg.rs:81`). The writer's
/// own doc says why they are separate: "this crate does not depend on that one,
/// so the derivation is duplicated rather than shared."
///
/// THIS LIVES IN plugin-db, WHICH IS NEITHER OF THEM, and that is not an
/// accident: plugin-db is the only crate that already dev-depends on both
/// (`Cargo.toml` :121 and :141, the latter noting "DEV-only ... so no cycle"),
/// so the check costs no new edge in the dependency graph.
///
/// WHY AN EQUALITY TEST AND NOT AN INTEGRATION TEST. The obvious alternative -
/// provision through the writer, then read through the reader - is what
/// `tests/integration.rs` looks like it does and does NOT: it computes the name
/// with the READER, then creates and drops that schema as its own fixture, so a
/// drift in the writer alone leaves it green. A test that builds its own
/// precondition cannot detect a disagreement between two producers.
#[cfg(test)]
mod journal_schema_derivations_agree {
    use uuid::Uuid;

    /// Both derivations must produce the same schema name for the same app.
    ///
    /// Asserted over several ids rather than one, because the shapes that could
    /// diverge are formatting choices - hyphenation, case, prefix - and a single
    /// fixed uuid can hide a difference that only some byte patterns expose.
    #[test]
    fn the_writer_and_the_reader_name_the_same_schema() {
        let ids = [
            Uuid::nil(),
            Uuid::max(),
            Uuid::parse_str("0198f0a1-0000-7000-8000-0123456789ab").expect("fixed uuid parses"),
            Uuid::new_v4(),
        ];
        for id in ids {
            let writer = zeroship_migrate_server::provisioning::workflow_journal_schema_name(&id);
            let reader = zeroship_workflow::store::pg::app_schema_for(&id);
            assert_eq!(
                writer, reader,
                "the migration service provisions the workflow journal schema as \
                 {writer} while the workflow plugin reads {reader}; a deploy would \
                 write its journal where nothing looks for it"
            );
        }
    }
}

/// `reset_context_for_tests` must clear ALL THREE thread-locals.
///
/// The context was one struct until 2026-09-02. It is now three owners with
/// three thread-locals - the adapter context, the engine.s lanes, and
/// data-core.s descriptor store - so "reset" became three calls, any one of
/// which could be dropped without an existing test noticing. One test per
/// store, because a single test asserting all three would pass while two of
/// them regressed.
///
/// **Both halves live in ONE test on purpose.** The obvious shape - claim in
/// test A, assert clean in test B - proves nothing here: libtest gives every
/// `#[test]` its own OS thread even under `--test-threads=1` (measured
/// 2026-09-01), so B would read a fresh thread-local and pass whatever the
/// helper does. Such a guard is green by construction. The real hazard is a
/// MID-TEST reset - scenario setup between phases, and the `ContextReset` drop
/// guard in `transaction/mod.rs` - and that is what this reproduces.
#[cfg(test)]
mod reset_clears_every_thread_local {
    /// Deleting `tx_lanes::reset_for_tests()` from `reset_context_for_tests`
    /// must fail this.
    ///
    /// `TxLanes` has TWO stores and both are asserted, because they have
    /// different lifetimes and a partial reset could plausibly clear one: the
    /// lane map is emptied by ordinary retirement, whereas the withdrawal
    /// tombstone is documented to outlive its lane and to be cleared only by
    /// the next `admit_transaction`. The tombstone is therefore the residue
    /// most likely to survive a reset that looks correct.
    #[test]
    fn a_mid_test_reset_drops_a_claim_and_a_withdrawal_tombstone() {
        let app = "app_reset_guard";

        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "an unclaimed app claims on a fresh thread"
        );
        // `tx_claimed_by`, not `has_tx_for`: claiming opens the lane, and
        // `has_tx_for` additionally requires the BEGIN to have landed a
        // session. The claim without a session is exactly the window this
        // helper has to clean up, so it is the one to assert on.
        assert!(crate::tx_lanes::with(|l| l.tx_claimed_by(app)));

        crate::tx_lanes::with_mut(|l| l.withdraw_tx_session(app));
        assert!(crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)));

        crate::testing::reset_context_for_tests();

        assert!(
            !crate::tx_lanes::with(|l| l.tx_claimed_by(app)),
            "reset_context_for_tests left a transaction claim behind: the lane \
             thread-local was not reset"
        );
        assert!(
            !crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)),
            "reset_context_for_tests left a withdrawal tombstone behind: the \
             next phase's session would be destroyed on return instead of parked"
        );
        // Re-claiming is the stronger statement, and it is the one a later
        // phase of a multi-phase test actually makes: `tx_claimed_by` could
        // read false off a half-cleared lane that still refuses a new claim.
        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "the app is claimable again after a reset"
        );
    }

    /// The descriptor store is the THIRD thread-local the reset must clear, and
    /// the one furthest from the helper: it moved to `zeroship-data-core` on
    /// 2026-09-02, so a reset that forgot it would leave a stale schema visible
    /// to the next phase of a test - the exact L24 shape, where serving a read
    /// against the wrong descriptor is what drops the projection allowlist.
    #[test]
    fn a_mid_test_reset_drops_an_installed_descriptor() {
        let binding = zeroship_data_orm::binding::DbBinding::cold_start("app_reset_schema");

        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &binding,
                "users",
                zeroship_data_sql::value!({ "email": { "type": "string" } }),
            );
        });
        assert!(zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_some());

        crate::testing::reset_context_for_tests();

        assert!(
            zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_none(),
            "reset_context_for_tests left a descriptor entry behind: the schema \
             thread-local in data-core was not reset"
        );
    }
}

mod v8_values;
