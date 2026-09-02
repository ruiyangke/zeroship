//! `DbPlatform` — the `#[v8_class]` capability handle holding the
//! platform-internal DB callables.
//!
//! ## Why this class exists
//!
//! The platform-internal entry points - `setMaskPolicy` and the `replication`
//! sub-namespace — do not live directly on the `Db` v8_class
//! (`env.db`). Living there would make them directly reachable from creator JS
//! and would surface them in IDE hover on the published `@zeroship/types`
//! surface.
//!
//! Instead they live behind a single `DbPlatform` handle that is set on
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
//! The `set_mask_policy` pipeline and the `Replication` v8_class delegate to
//! their app-scoped native implementations; this wrapper is their private
//! capability carrier.
//!
//! ## App scoping
//!
//! `app_id` is stamped at mint time (in `db.rs::mint_db`, from the live
//! `Db`'s own `app_id`). The handle is therefore bound to exactly one
//! tenant; there is no caller-supplied app-id override on any method

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

use zeroship_data_core::binding::DbBinding;
use crate::crud::dispatch_set_mask_policy_field;
use crate::v8_bridge::read_json_arg;

// ---------------------------------------------------------------------------
// DbPlatform state
// ---------------------------------------------------------------------------

/// Owned state for the platform-internal capability handle.
///
/// Field 0 of the wrapper holds a `Box<DbPlatform>`. The Weak finalizer
/// registered by [`mint_db_platform`] drops the Box on GC. There are no
/// native resources to release — `binding` is owned and the cached
/// `replication_obj` holds a `v8::Global<v8::Object>` handle whose own
/// Weak counterpart reclaims the wrapped state.
pub struct DbPlatform {
    /// The app-at-deploy identity this handle is scoped to. Stamped at mint
    /// time from the live `Db`; never mutated. Security-critical — the
    /// platform methods route to `"<app_id>".*` schemas, so a
    /// caller-supplied override is never honoured (see
    /// platform operations).
    pub(crate) binding: DbBinding,
    /// Cache of the `Replication` namespace wrapper minted on first
    /// access of `__platform.replication`. Stable identity.
    pub(crate) replication_obj: RefCell<Option<v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for DbPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlatform")
            .field("binding", &self.binding)
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

    /// `__platform.setMaskPolicy(policy)` — persist the
    /// per-app mask policy and refresh the in-process cache. Moved off
    /// `Db`; [`dispatch_set_mask_policy_field`] is unchanged.
    #[v8_method]
    #[v8_name = "setMaskPolicy"]
    fn set_mask_policy<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        policy: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let policy_v = match read_json_arg(scope, Some(policy)) {
            Ok(v) => v,
            Err(e) => return Ok(crate::v8_bridge::throw_decode_error(scope, &e)),
        };
        Ok(dispatch_set_mask_policy_field(scope, self.binding.app_id(), policy_v).into())
    }

    /// `__platform.replication` — the [`super::replication::Replication`]
    /// namespace (`watchdog`) scoped to this app. Cached on first access.
    /// Moved off `Db`.
    #[v8_getter]
    fn replication<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.replication_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = super::replication::mint_replication(scope, self.binding.app_id())?;
        let global = v8::Global::new(scope, obj);
        *self.replication_obj.borrow_mut() = Some(global);
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
    binding: DbBinding,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = DbPlatform::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = DbPlatform {
        binding,
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
    // Global is dropped with the Box (its own Weak finalizer
    // reclaims the wrapped Replication state).
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
