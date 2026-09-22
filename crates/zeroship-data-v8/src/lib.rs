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
    app_bindings: std::sync::Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>,
    cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
    meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
}
impl DbPlugin {
    pub(crate) fn new(
        connection: ConnectionFactory,
        cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
        meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
        project_keys: std::sync::Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>,
        app_bindings: std::sync::Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>,
    ) -> Self {
        Self {
            connection,
            cdc_relay,
            meter,
            project_keys,
            app_bindings,
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

    /// Install every declared database's collections onto its own handle.
    ///
    /// One `installSchema` call per database, each against the handle that
    /// database's operations route through. The PRIMARY's handle is
    /// `namespace` itself - the same object `env.db` names - so the app's own
    /// collections land where a single-database app has always found them.
    fn prepare_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<Option<v8::Global<v8::Promise>>, String> {
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        let databases = zeroship_runtime::databases::databases_of(descriptor);
        let mut settled = None;
        for database in databases {
            let handle = if database.primary {
                namespace
            } else {
                let Some(handle) = companion_handle(scope, namespace, &database.label)? else {
                    continue;
                };
                handle
            };
            // Rust is the single schema authority: decode and normalize once
            // here, then hand the JS adapter an already-decoded projection so
            // it builds collections with no re-validation or re-decode.
            let schema = descriptor_schemas(Some(&database.schema))?;
            let projection = schema_projection::project_schema(&schema, &database.schema)?;
            let json = serde_json::to_string(&projection).map_err(|error| error.to_string())?;
            let json = v8::String::new(scope, &json).ok_or("could not allocate DB descriptor")?;
            let projected = v8::json::parse(scope, json).ok_or("could not parse DB descriptor")?;
            settled = zeroship_runtime::modules::invoke_module_export(
                scope,
                "zeroship:db/adapter",
                "installSchema",
                &[handle.into(), projected],
            )
            .map(Some)?;
        }
        Ok(settled)
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

    /// Publish each declared database's collections under the binding the
    /// host resolved FOR THAT DATABASE.
    ///
    /// The isolate composes no part of a binding: the database id, the edge id
    /// are control-plane facts, so this READS what the
    /// trusted host resolved and refuses a database it resolved nothing for.
    fn bind_runtime_descriptor<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
        _namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        // The startup mask policy is declared on `env.db`, so the PRIMARY's
        // binding is what seals it - including for a schema-less deployment,
        // which still has an `env.db` to declare against.
        if let Some(primary) = v8_classes::db::primary_binding_for_isolate(scope, app_id) {
            startup_policy::initialize(scope, primary);
        }
        let databases = descriptor
            .map(zeroship_runtime::databases::databases_of)
            .unwrap_or_default();
        if databases.is_empty() {
            // A schema-less deployment REPLACES whatever an earlier one
            // installed, on every binding the host resolved. Returning early
            // would leave a retired deploy's collections serveable under the
            // new one.
            for binding in v8_classes::db::bindings_for_isolate(scope, app_id) {
                zeroship_data_orm::descriptor::install_collections(
                    &binding,
                    zeroship_data_orm::schema::Schema::default(),
                )
                .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        // The runtime validates the complete document before invoking this
        // hook. Resolve every database's schema AND binding before mutating
        // the shared thread context, so a failure on the second cannot leave
        // the first published.
        let mut installs = Vec::with_capacity(databases.len());
        for database in &databases {
            let schemas = descriptor_schemas(Some(&database.schema))?;
            let id = zeroship_core::DatabaseId::parse(&database.database_id).map_err(|error| {
                format!("database {:?} has an unusable id: {error}", database.label)
            })?;
            let binding = v8_classes::db::binding_for_isolate(scope, app_id, &id)
                .ok_or_else(|| {
                    format!(
                        "app id {app_id:?} has no resolved binding for database {}",
                        database.database_id
                    )
                })?;
            installs.push((database.primary, binding, schemas));
        }
        for (_primary, binding, schemas) in installs {
            zeroship_data_orm::descriptor::install_collections(&binding, schemas)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// `env.databases` - one handle per database the deployment declares.
    ///
    /// The primary's entry is the SAME object as `env.db`, by identity, so
    /// `env.db === env.databases[primary]` holds and there is one concept and
    /// one code path. A deployment declaring no database publishes nothing.
    fn companion_namespaces<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
        namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<Vec<(&'static str, v8::Local<'s, v8::Value>)>, String> {
        let Some(descriptor) = descriptor else {
            return Ok(Vec::new());
        };
        let databases = zeroship_runtime::databases::databases_of(descriptor);
        if databases.is_empty() {
            return Ok(Vec::new());
        }
        let map = v8::Object::new(scope);
        for database in databases {
            let handle = if database.primary {
                namespace
            } else {
                let id = zeroship_core::DatabaseId::parse(&database.database_id).map_err(
                    |error| format!("database {:?} has an unusable id: {error}", database.label),
                )?;
                let binding =
                    v8_classes::db::binding_for_isolate(scope, app_id, &id).ok_or_else(|| {
                        format!(
                            "app id {app_id:?} has no resolved binding for database {}",
                            database.database_id
                        )
                    })?;
                v8_classes::db::mint_db_for_binding(scope, binding)
                    .ok_or_else(|| format!("could not mint a handle for {:?}", database.label))?
            };
            let key = v8::String::new(scope, &database.label)
                .ok_or_else(|| format!("could not allocate label {:?}", database.label))?;
            map.set(scope, key.into(), handle.into())
                .ok_or_else(|| format!("could not publish database {:?}", database.label))?;
        }
        Ok(vec![("databases", map.into())])
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
            context.set_app_bindings(Some(self.app_bindings.clone()));
            context.install_connection(self.connection.clone());
            context.set_cdc_relay(self.cdc_relay.clone());
            context.set_meter(self.meter.clone());
        });
    }
}

/// The `env.databases` handle already published for one label.
///
/// `companion_namespaces` runs before `prepare_runtime`, so the map is on
/// `env` by the time the adapter installs collections. `None` when the map is
/// absent, which is the schema-less case.
fn companion_handle<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    namespace: v8::Local<'s, v8::Object>,
    label: &str,
) -> Result<Option<v8::Local<'s, v8::Object>>, String> {
    let _ = namespace;
    let Ok(map) = zeroship_runtime::plugin::runtime_env_member(scope, "databases") else {
        return Ok(None);
    };
    let key = v8::String::new(scope, label)
        .ok_or_else(|| format!("could not allocate label {label:?}"))?;
    let value = map
        .get(scope, key.into())
        .ok_or_else(|| format!("env.databases is missing {label:?}"))?;
    value
        .try_into()
        .map(Some)
        .map_err(|_| format!("env.databases.{label} is not an object"))
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
        // The tests that call this drive the plugin directly rather than
        // through a Runtime, so `register` never runs and the thread would
        // carry no binding store at all.
        crate::tests::fixtures::supply_app_bindings([APP]);
    }

    fn plugin() -> std::sync::Arc<DbPlugin> {
        service::DbService::new(service::DbServiceConfig {
            app_bindings: crate::tests::fixtures::harness_app_bindings([APP]),
            project_keys: crate::tests::fixtures::project_keys(),
            connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(
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
            .runtime_descriptor(Some(crate::tests::fixtures::harness_descriptor_document(
                APP,
                &runtime_descriptor("__zeroship_workflow_app_state"),
            )))
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
            .runtime_descriptor(Some(crate::tests::fixtures::harness_descriptor_document(
                APP,
                &runtime_descriptor_for(&[
                    "transaction",
                    "constructor",
                    "__platform",
                    "declareMaskPolicy",
                ]),
            )))
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
                Some(&crate::tests::fixtures::harness_descriptor_value(
                    APP,
                    &serde_json::to_value(&runtime_descriptor).unwrap(),
                )),
            )
            .expect("bind descriptor");

        let binding = crate::tests::fixtures::harness_binding_at_deploy(APP, DEPLOY);
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
                Some(&crate::tests::fixtures::harness_descriptor_value(
                    APP,
                    &serde_json::to_value(&invalid).unwrap(),
                )),
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
                Some(&crate::tests::fixtures::harness_descriptor_value(
                    APP,
                    &serde_json::to_value(&invalid_reference).unwrap(),
                )),
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

    /// An app that declares TWO databases gets a handle for each, and the
    /// PRIMARY's handle is the SAME OBJECT as `env.db`.
    ///
    /// Object identity is the contract, not equality: `env.db` and
    /// `env.databases[primary]` are one handle, so there is one concept, one
    /// code path, and a single-database app sees no difference. The second
    /// database's handle carries its OWN binding, which is what makes its
    /// statements narrow to its own role rather than the primary's.
    #[test]
    fn a_two_database_app_gets_a_handle_per_database_and_the_primary_is_env_db() {
        crate::tests::fixtures::reset_context();
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        install_runtime_state(scope);

        // Two databases, both resolved by the host, exactly as Control serves
        // them: one set per app, each member keyed on its own database.
        let primary = crate::tests::fixtures::harness_binding(APP);
        let secondary = zeroship_data_orm::binding::DbBinding::to_database(
            APP,
            zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
            zeroship_core::DatabaseId::mint(),
            zeroship_core::BindingId::mint(),
            zeroship_core::database_role::DatabaseCapability::ReadWrite,
        )
        .expect("a minted database and edge compose a legal role name");
        assert_ne!(primary.database(), secondary.database());
        let store = std::sync::Arc::new(
            zeroship_data_orm::resolved_bindings::SuppliedAppBindings::new(),
        );
        for binding in [&primary, &secondary] {
            store
                .supply(
                    APP,
                    zeroship_data_orm::resolved_bindings::ResolvedBinding::from(
                        binding.edge().expect("a harness binding addresses a database"),
                    ),
                )
                .expect("the store accepts each database's binding");
        }
        crate::context::with_mut(|c| c.set_app_bindings(Some(store)));

        let schema_of = |collection: &str| {
            serde_json::json!({
                "version": 2,
                "collections": {
                    collection: {
                        "fields": {
                            "id": { "type": "id", "required": true, "primaryKey": true }
                        },
                        "options": { "softDelete": false, "versioning": false },
                        "indexes": []
                    }
                }
            })
        };
        let document = serde_json::json!({
            "version": 1,
            "databases": [
                {
                    "label": "main",
                    "database_id": primary.database().unwrap().as_str(),
                    "primary": true,
                    "schema": schema_of("users"),
                },
                {
                    "label": "analytics",
                    "database_id": secondary.database().unwrap().as_str(),
                    "primary": false,
                    "schema": schema_of("events"),
                },
            ]
        });

        // The document is what names the primary, so the host installs it the
        // way a real runtime does before any namespace is built.
        crate::v8_bridge::runtime_state(scope)
            .borrow_mut()
            .runtime_descriptor = Some(document.to_string());

        let plugin = plugin();
        let namespace = plugin
            .build_instance(scope, APP)
            .expect("native db namespace");
        plugin
            .bind_runtime_descriptor(scope, APP, namespace, Some(&document))
            .expect("bind the two-database document");

        // Each database's collections are installed under ITS OWN binding.
        let at_deploy = |binding: &zeroship_data_orm::binding::DbBinding| {
            crate::tests::fixtures::harness_binding_at_deploy_for(binding, DEPLOY)
        };
        descriptor::collection_schema(&at_deploy(&primary), "users")
            .expect("the primary's own collection resolves under the primary's binding");
        descriptor::collection_schema(&at_deploy(&secondary), "events")
            .expect("the second database's collection resolves under its own binding");
        assert!(
            descriptor::collection_schema(&at_deploy(&primary), "events").is_err(),
            "one database's collections must not resolve under the other's binding"
        );

        // `env.databases` carries both labels, and the primary's entry is the
        // same object `env.db` names.
        let companions = plugin
            .companion_namespaces(scope, APP, namespace, Some(&document))
            .expect("companions build");
        assert_eq!(companions.len(), 1);
        assert_eq!(companions[0].0, "databases");
        let map: v8::Local<v8::Object> = companions[0].1.try_into().expect("databases is an object");
        let main_key = v8::String::new(scope, "main").unwrap();
        let main_handle = map.get(scope, main_key.into()).expect("main handle");
        let analytics_key = v8::String::new(scope, "analytics").unwrap();
        let analytics_handle = map
            .get(scope, analytics_key.into())
            .expect("analytics handle");
        assert!(
            main_handle.strict_equals(namespace.into()),
            "env.databases[primary] must be env.db BY IDENTITY"
        );
        assert!(
            !analytics_handle.strict_equals(namespace.into()),
            "a second database is a second handle"
        );
        let analytics: v8::Local<v8::Object> =
            analytics_handle.try_into().expect("a Db handle is an object");
        assert!(
            v8_classes::db::Db::is_instance(scope, analytics.into()),
            "every member of env.databases is a Db"
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
                Some(&crate::tests::fixtures::harness_descriptor_value(
                    APP,
                    &serde_json::to_value(&runtime_descriptor).unwrap(),
                )),
            )
            .expect("bind descriptor");
        plugin
            .bind_runtime_descriptor(scope, APP, namespace, None)
            .expect("bind schema-less runtime");

        let binding = crate::tests::fixtures::harness_binding_at_deploy(APP, DEPLOY);
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
