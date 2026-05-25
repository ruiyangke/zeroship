//! `DbPlatform` — the `#[v8_class]` capability handle holding the
//! platform-internal DB callables (P9 PR 4).
//!
//! ## Why this class exists
//!
//! Before P9 PR 4 the platform-internal entry points — `registerModel`,
//! `setMaskPolicy`, `startReplicationConsumer`, and the `migrations` /
//! `replication` sub-namespaces — lived directly on the `Db` v8_class
//! (`env.db`). They were therefore directly reachable from creator JS
//! (`env.db.registerModel(...)`) and showed up in IDE hover on the
//! published `@zeroship/types` surface.
//!
//! P9 §8 moves them behind a single `DbPlatform` handle that is set on
//! the `Db` object under a **V8 private symbol** (`ZS_PLATFORM`, minted
//! once per isolate by the runtime — see
//! `crates/runtime/src/core/init.rs`). A `v8::Private` is a Rust-only
//! construct: it is NOT a `v8::Symbol`, cannot be used as a property key
//! from JS, and is invisible to `Object.keys` /
//! `Object.getOwnPropertyNames` / `Object.getOwnPropertySymbols` /
//! `for..in` / `JSON.stringify`. The only readers are Rust
//! (`get_private`) and `@zeroship/bootstrap`'s `runtime-entry`, which
//! the runtime hands a resolver to (and which creator code cannot
//! import). See the module doc on `db.rs::mint_db` for the wiring.
//!
//! ## What this class adds
//!
//! Every method delegates to the SAME dispatch helper the `Db` method
//! used to call — only the JS carrier moved. The `register_model` /
//! `set_mask_policy` / `start_replication_consumer` pipelines and the
//! `Migrations` / `Replication` v8_classes are unchanged; this wrapper
//! is a thin, app-scoped re-home of the five entry points.
//!
//! ## App scoping
//!
//! `app_id` is stamped at mint time (in `db.rs::mint_db`, from the live
//! `Db`'s own `app_id`). The handle is therefore bound to exactly one
//! tenant; there is no caller-supplied app-id override on any method
//! (the `startReplicationConsumer` hardening from security-review r2 is
//! preserved verbatim — see [`super::db`]'s `resolve_consumer_app_id`).

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

use crate::crud::dispatch_set_mask_policy_field;
use crate::register_model::register_model_dispatch;
use crate::replication_ops::start_replication_consumer_dispatch;
use crate::v8_bridge::read_json_arg;

// ---------------------------------------------------------------------------
// DbPlatform state
// ---------------------------------------------------------------------------

/// Owned state for the platform-internal capability handle.
///
/// Field 0 of the wrapper holds a `Box<DbPlatform>`. The Weak finalizer
/// registered by [`mint_db_platform`] drops the Box on GC. There are no
/// native resources to release — `app_id` is a `String` and the cached
/// `migrations_obj` / `replication_obj` hold `v8::Global<v8::Object>`
/// handles whose own Weak counterparts reclaim the wrapped state.
pub struct DbPlatform {
    /// The app_id this handle is scoped to. Stamped at mint time from
    /// the live `Db`'s `app_id`; never mutated. Security-critical — the
    /// platform methods route to `"<app_id>".*` schemas, so a
    /// caller-supplied override is never honoured (see
    /// `start_replication_consumer`).
    pub(crate) app_id: String,
    /// Cache of the `Migrations` namespace wrapper minted on first
    /// access of `__platform.migrations`. Stable identity so
    /// `__platform.migrations === __platform.migrations` holds.
    pub(crate) migrations_obj: RefCell<Option<v8::Global<v8::Object>>>,
    /// Cache of the `Replication` namespace wrapper minted on first
    /// access of `__platform.replication`. Stable identity.
    pub(crate) replication_obj: RefCell<Option<v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for DbPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlatform")
            .field("app_id", &self.app_id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// DbPlatform IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl DbPlatform {
    /// `new DbPlatform()` from JS rejects with `TypeError("illegal
    /// constructor")`. Real instances are minted only by
    /// [`mint_db_platform`] (invoked from `db.rs::mint_db`) and stamped
    /// with the live `app_id`; a user-constructed handle would carry an
    /// empty app_id and target a non-existent schema.
    #[v8_constructor]
    fn new() -> Result<DbPlatform, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `__platform.registerModel(collection, schema, indexes?)` — DDL
    /// orchestrator entry. Idempotent. Moved off `Db` in P9 PR 4; the
    /// [`register_model_dispatch`] pipeline is unchanged — only the JS
    /// carrier relocated behind the capability handle.
    #[v8_method]
    #[v8_name = "registerModel"]
    fn register_model<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        collection: String,
        schema: v8::Local<v8::Value>,
        indexes: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if collection.is_empty() {
            return Err(OpError::type_error(
                "db.registerModel: collection must be a non-empty string",
            ));
        }
        let schema_v = read_json_arg(scope, Some(schema));
        let indexes_v = if indexes.is_null_or_undefined() {
            serde_json::Value::Array(Vec::new())
        } else {
            read_json_arg(scope, Some(indexes))
        };
        Ok(register_model_dispatch(
            scope,
            &self.app_id,
            &collection,
            schema_v,
            indexes_v,
        )
        .into())
    }

    /// `__platform.setMaskPolicy(policy)` — **P5.5 PR 5**. Persist the
    /// per-app mask policy and refresh the in-process cache. Moved off
    /// `Db` in P9 PR 4; [`dispatch_set_mask_policy_field`] is unchanged.
    #[v8_method]
    #[v8_name = "setMaskPolicy"]
    fn set_mask_policy<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        policy: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let policy_v = read_json_arg(scope, Some(policy));
        Ok(dispatch_set_mask_policy_field(scope, &self.app_id, policy_v).into())
    }

    /// `__platform.startReplicationConsumer(opts?)` — provisions the
    /// per-app publication + slot and spawns the supervised WAL
    /// consumer. Idempotent. Moved off `Db` in P9 PR 4.
    ///
    /// Always scoped to `self.app_id`. Any app-id-shaped `opts` value is
    /// intentionally ignored — honouring a JS-supplied app_id here would
    /// let App A spawn a WAL consumer against App B's stream. Resolution
    /// runs through [`super::db::resolve_consumer_app_id`] (the
    /// security-critical policy lives in one unit-tested place; this
    /// handle calls the same function).
    #[v8_method]
    #[v8_name = "startReplicationConsumer"]
    fn start_replication_consumer<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id = super::db::resolve_consumer_app_id(&self.app_id, scope, opts);
        start_replication_consumer_dispatch(scope, app_id).into()
    }

    /// `__platform.replication` — the [`super::replication::Replication`]
    /// namespace (`setup` / `watchdog` / `dropAbandoned`) scoped to this
    /// app. Cached on first access. Moved off `Db` in P9 PR 4.
    #[v8_getter]
    fn replication<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.replication_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = super::replication::mint_replication(scope, &self.app_id)?;
        let global = v8::Global::new(scope, obj);
        *self.replication_obj.borrow_mut() = Some(global);
        Ok(obj)
    }

    /// `__platform.migrations` — the [`super::migrations::Migrations`]
    /// namespace (`start` / `status` / `cancel` / `reset`). Cached on
    /// first access. Moved off `Db` in P9 PR 4.
    #[v8_getter]
    fn migrations<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.migrations_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = super::migrations::mint_migrations(scope, &self.app_id)?;
        let global = v8::Global::new(scope, obj);
        *self.migrations_obj.borrow_mut() = Some(global);
        Ok(obj)
    }
}

// ---------------------------------------------------------------------------
// mint_db_platform — build a `DbPlatform` wrapper for a given app_id
// ---------------------------------------------------------------------------

/// Mint a `DbPlatform` v8_class instance scoped to `app_id`.
///
/// Called once per `Db` instance from [`super::db::mint_db`], which then
/// stows the returned object on the `Db` under the `ZS_PLATFORM` private
/// symbol. The handle is unreachable from creator JS (private-symbol
/// slot); only Rust and the bootstrap runtime-entry resolver read it.
pub(crate) fn mint_db_platform<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = DbPlatform::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = DbPlatform {
        app_id: app_id.to_string(),
        migrations_obj: RefCell::new(None),
        replication_obj: RefCell::new(None),
    };
    let boxed: Box<DbPlatform> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<DbPlatform>`; the
    // finalizer casts back to the same type and drops the Box exactly
    // once when V8 reclaims the wrapper. There are no native resources
    // to release in Drop — `app_id` is an owned String and the cached
    // Globals are dropped with the Box (their own Weak finalizers
    // reclaim the wrapped Migrations / Replication state).
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut DbPlatform));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}
