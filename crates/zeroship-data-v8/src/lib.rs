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
#![deny(private_interfaces, private_bounds)]

use zeroship_data_orm::connection::ConnectionFactory;

use zeroship_runtime::plugin::{JavaScriptModule, NativePlugin, NativeRegistrar};

use crate::context::with_mut as ctx_mut;
use zeroship_data_orm::error::DbError;

// Private imports used to compose ORM operations with isolate state.
use zeroship_data_orm::cdc::broker;
#[cfg(test)]
use zeroship_data_orm::sql::mapping;
use zeroship_data_orm::{backend, descriptor, transaction, tx_route};

pub(crate) mod context;
pub mod op_error;
mod read_capture;
mod schema_projection;
mod startup_policy;
#[cfg(test)]
mod tests;
mod usage;
#[cfg(test)]
extern crate self as zeroship_data_v8;
pub(crate) mod v8_bridge;
pub mod v8_classes;

pub mod service;

// Async-scoped transaction marker. Read by `transaction` to tell a
// genuinely NESTED `transaction()` call from one that merely overlaps
// another in time; see the module docs for the defect that distinction
// closes.
pub(crate) mod tx_scope;

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
    project_keys: std::sync::Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>,
    cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
}
impl DbPlugin {
    pub(crate) fn new(
        connection: ConnectionFactory,
        cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
        meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
        project_keys: std::sync::Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>,
    ) -> Self {
        Self {
            connection,
            cdc_relay,
            meter,
            project_keys,
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

    fn host_javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: "zeroship:db/adapter",
            source: include_str!("../dist/adapter.js"),
        }]
    }

    fn prepare_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<Option<v8::Global<v8::Promise>>, String> {
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        // Rust is the single schema authority: decode and normalize once here,
        // then hand the JS adapter an already-decoded projection so it builds
        // collections with no re-validation or re-decode.
        let schema = descriptor_schemas(Some(descriptor))?;
        let projection = schema_projection::project_schema(&schema, descriptor)?;
        let json = serde_json::to_string(&projection).map_err(|error| error.to_string())?;
        let json = v8::String::new(scope, &json).ok_or("could not allocate DB descriptor")?;
        let descriptor = v8::json::parse(scope, json).ok_or("could not parse DB descriptor")?;
        zeroship_runtime::modules::invoke_module_export(
            scope,
            "zeroship:db/adapter",
            "installSchema",
            &[namespace.into(), descriptor],
        )
        .map(Some)
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

    fn bind_runtime_descriptor<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
        _namespace: v8::Local<'s, v8::Object>,
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
        startup_policy::initialize(scope, binding.clone());
        zeroship_data_orm::descriptor::install_collections(&binding, schemas)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn finalize_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _namespace: v8::Local<'s, v8::Object>,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        startup_policy::finalize(scope)
    }

    fn register(&self, _: &mut NativeRegistrar) {
        ctx_mut(|context| {
            context.set_supplied_project_keys(Some(self.project_keys.clone()));
            context.install_connection(self.connection.clone());
            context.set_cdc_relay(self.cdc_relay.clone());
            context.set_meter(self.meter.clone());
        });
    }
}

fn descriptor_schemas(
    descriptor: Option<&serde_json::Value>,
) -> Result<zeroship_data_orm::schema::Schema, String> {
    descriptor.map_or_else(
        || Ok(zeroship_data_orm::schema::Schema::default()),
        |descriptor| {
            zeroship_data_orm::schema::Schema::from_runtime_descriptor(
                &zeroship_data_orm::value::Value::from(descriptor.clone()),
            )
            .map_err(|error| error.to_string())
        },
    )
}

#[cfg(test)]
mod runtime_descriptor_binding_tests {
    use futures::FutureExt;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use zeroship_data_orm::value;
    use zeroship_runtime::{init_v8, EnvSnapshot, ModuleEntry, Runtime, RuntimeState, SharedState};

    use super::*;

    const APP: &str = "app_native_descriptor";
    const DEPLOY: &str = "deploy_native_descriptor";

    fn install_runtime_state(scope: &mut v8::PinScope<'_, '_>) {
        let mut env = HashMap::new();
        env.insert("APP_ID".to_string(), APP.to_string());
        env.insert("ZEROSHIP_DEPLOY_ID".to_string(), DEPLOY.to_string());
        let state: SharedState = std::rc::Rc::new(RefCell::new(RuntimeState::new(env, None, None)));
        scope.set_slot(state);
    }

    fn plugin() -> std::sync::Arc<DbPlugin> {
        service::DbService::new(service::DbServiceConfig {
            project_keys: crate::tests::fixtures::project_keys(),
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

    fn runtime_descriptor(collection: &str) -> String {
        runtime_descriptor_for(&[collection])
    }

    fn runtime_descriptor_for(collections: &[&str]) -> String {
        let collections = collections
            .iter()
            .map(|collection| {
                (
                    (*collection).to_string(),
                    serde_json::json!({
                        "fields": {
                            "id": { "type": "string", "required": true, "primaryKey": true },
                            "title": { "type": "string", "required": true }
                        },
                        "options": {
                            "softDelete": false,
                            "versioning": false,
                            "strictness": "strict"
                        },
                        "indexes": []
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "version": 2,
            "collections": collections
        })
        .to_string()
    }

    #[test]
    fn declared_collections_have_sdk_facades_during_creator_module_evaluation() {
        crate::tests::fixtures::reset_context();
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
import { env } from "zeroship";
const direct = env.db.__zeroship_workflow_app_state;
const property = Object.getOwnPropertyDescriptor(env.db, "__zeroship_workflow_app_state");
const query = direct?.find({});
globalThis.__nativeCollectionAtEvaluation = JSON.stringify({
    visible: direct != null,
    hasFind: typeof direct?.find === "function",
    hasSdkQueryBuilder: typeof query?.sort === "function",
    nativeLookupAvailable: typeof env.db.collection("__zeroship_workflow_app_state").find === "function",
    enumerable: property?.enumerable === true,
    readOnly: property?.writable === false,
});
export default { fetch() { return new Response("ok"); } };
"#
            .into(),
        }];
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(HashMap::from([("APP_ID".to_string(), APP.to_string())]))
            .plugins(vec![plugin() as std::sync::Arc<dyn NativePlugin>])
            .runtime_descriptor(Some(runtime_descriptor("__zeroship_workflow_app_state")))
            .build();

        runtime
            .initialize(&EnvSnapshot::empty())
            .now_or_never().expect("fixture startup must settle without I/O")
            .expect("native collection descriptor must initialize");
        let observed = runtime.with_scope(|scope| {
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__nativeCollectionAtEvaluation").unwrap();
            global
                .get(scope, key.into())
                .expect("creator module observation")
                .to_rust_string_lossy(scope)
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observed).unwrap(),
            serde_json::json!({
                "visible": true,
                "hasFind": true,
                "hasSdkQueryBuilder": true,
                "nativeLookupAvailable": true,
                "enumerable": true,
                "readOnly": true,
            })
        );
    }

    #[test]
    fn descriptor_name_collisions_remain_available_through_collection_lookup() {
        crate::tests::fixtures::reset_context();
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
import { env } from "zeroship";
const names = ["transaction", "constructor", "__platform", "declareMaskPolicy"];
globalThis.__dbCollisionCollections = JSON.stringify({
    nativeTransactionSurvives: typeof env.db.transaction === "function",
    policyDeclarationSurvives: typeof env.db.declareMaskPolicy === "function",
    collections: names.map((name) => ({
        name,
        hasFind: typeof env.db.collection(name).find === "function",
    })),
});
export default { fetch() { return new Response("ok"); } };
"#
            .into(),
        }];
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(HashMap::from([("APP_ID".to_string(), APP.to_string())]))
            .plugins(vec![plugin() as std::sync::Arc<dyn NativePlugin>])
            .runtime_descriptor(Some(runtime_descriptor_for(&[
                "transaction",
                "constructor",
                "__platform",
                "declareMaskPolicy",
            ])))
            .build();

        runtime
            .initialize(&EnvSnapshot::empty())
            .now_or_never().expect("fixture startup must settle without I/O")
            .expect("name collisions must not block descriptor installation");
        let observed = runtime.with_scope(|scope| {
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__dbCollisionCollections").unwrap();
            global
                .get(scope, key.into())
                .expect("creator module observation")
                .to_rust_string_lossy(scope)
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observed).unwrap(),
            serde_json::json!({
                "nativeTransactionSurvives": true,
                "policyDeclarationSurvives": true,
                "collections": [
                    {"name": "transaction", "hasFind": true},
                    {"name": "constructor", "hasFind": true},
                    {"name": "__platform", "hasFind": true},
                    {"name": "declareMaskPolicy", "hasFind": true},
                ],
            })
        );
    }

    #[test]
    fn validated_runtime_descriptor_makes_declared_collection_serveable() {
        crate::tests::fixtures::reset_context();
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
                        "id": { "type": "id", "idPrefix": "usr", "required": true, "primaryKey": true },
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
        let plugin = plugin();
        let namespace = plugin
            .build_instance(scope, APP)
            .expect("native db namespace");
        plugin
            .bind_runtime_descriptor(
                scope,
                APP,
                namespace,
                Some(&serde_json::to_value(&runtime_descriptor).unwrap()),
            )
            .expect("bind descriptor");

        let binding = zeroship_data_orm::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_data_orm::sql::SchemaName::new(APP).unwrap(),
        );
        let schema = descriptor::collection_schema(&binding, "users")
            .expect("declared collection must resolve before any read");
        assert_eq!(
            schema.as_ref(),
            zeroship_data_orm::schema::CollectionSchema::from_fields(
                &runtime_descriptor["collections"]["users"]["fields"]
            )
            .unwrap()
            .fields(),
            "native boot must publish the decoded collection contract"
        );
        let mut invalid = runtime_descriptor.clone();
        invalid["collections"]["users"]["fields"] = value!({
            "key": { "type": "string", "required": true, "primaryKey": true }
        });
        let replacement_namespace = plugin
            .build_instance(scope, APP)
            .expect("replacement native db namespace");
        let error = plugin
            .bind_runtime_descriptor(
                scope,
                APP,
                replacement_namespace,
                Some(&serde_json::to_value(&invalid).unwrap()),
            )
            .expect_err("renamed identity must fail at native installation");
        assert!(error.contains("id"), "{error}");
        assert_eq!(
            descriptor::collection_schema(&binding, "users").unwrap(),
            schema
        );
        let mut invalid_reference = runtime_descriptor;
        invalid_reference["collections"]["users"]["fields"]["owner_id"] = value!({
            "type":"string", "refTarget":"undeclared", "refColumn":"id", "relation":"owner"
        });
        let replacement_namespace = plugin.build_instance(scope, APP).unwrap();
        let error = plugin
            .bind_runtime_descriptor(
                scope,
                APP,
                replacement_namespace,
                Some(&serde_json::to_value(&invalid_reference).unwrap()),
            )
            .expect_err("missing relation targets must fail before creator evaluation");
        assert!(
            error.contains("reference target collection is not declared"),
            "{error}"
        );
        assert_eq!(
            descriptor::collection_schema(&binding, "users").unwrap(),
            schema
        );
    }

    #[test]
    fn schema_less_runtime_replaces_binding_with_an_empty_view() {
        crate::tests::fixtures::reset_context();
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
                    "fields": { "id": { "type": "id", "required": true, "primaryKey": true } },
                    "options": { "softDelete": false, "versioning": false },
                    "indexes": []
                }
            }
        });
        let plugin = plugin();
        let namespace = plugin
            .build_instance(scope, APP)
            .expect("native db namespace");
        plugin
            .bind_runtime_descriptor(
                scope,
                APP,
                namespace,
                Some(&serde_json::to_value(&runtime_descriptor).unwrap()),
            )
            .expect("bind descriptor");
        plugin
            .bind_runtime_descriptor(scope, APP, namespace, None)
            .expect("bind schema-less runtime");

        let binding = zeroship_data_orm::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_data_orm::sql::SchemaName::new(APP).unwrap(),
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
    use crate::tests::fixtures::set_database_url;

    fn set_fresh_db_url(url: &str) {
        ctx_mut(|c| c.clear_backend());
        set_database_url(url);
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

mod v8_values;
