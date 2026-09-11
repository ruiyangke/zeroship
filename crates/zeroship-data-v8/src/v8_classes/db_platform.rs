//! `DbPlatform` — the `#[v8_class]` capability handle holding the
//! platform-internal DB callables.
//!
//! Policy installation lives behind the runtime's V8 private symbol. Creator
//! JavaScript cannot resolve that symbol; bootstrap receives its resolver from
//! the runtime. The handle is bound to the app's immutable deployment identity.

#![allow(unsafe_code)]


use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_method};

use crate::v8_bridge::read_native_arg;
use crate::v8_classes::dispatch::dispatch_set_mask_policy_field;
use zeroship_data_orm::binding::DbBinding;

// ---------------------------------------------------------------------------
// DbPlatform state
// ---------------------------------------------------------------------------

/// Owned state for the platform-internal capability handle.
///
/// Field 0 of the wrapper holds a `Box<DbPlatform>`. The Weak finalizer
/// registered by [`mint_db_platform`] drops the Box on GC. There are no
/// native resources to release; `binding` is owned.
pub struct DbPlatform {
    /// The app-at-deploy identity this handle is scoped to. Stamped at mint
    /// time from the live `Db`; never mutated. Security-critical — the
    /// platform methods route to `"<app_id>".*` schemas, so a
    /// caller-supplied override is never honoured (see
    /// platform operations).
    pub(crate) binding: DbBinding,
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

    /// Install the app-at-deploy startup policy in memory.
    /// A different policy for the same deployment is rejected.
    #[v8_method]
    #[v8_name = "setMaskPolicy"]
    fn set_mask_policy<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        policy: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let policy_v = match read_native_arg(scope, Some(policy)) {
            Ok(v) => v,
            Err(e) => return Ok(crate::v8_bridge::throw_decode_error(scope, &e)),
        };
        Ok(dispatch_set_mask_policy_field(scope, &self.binding, policy_v).into())
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
    };
    let boxed: Box<DbPlatform> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<DbPlatform>`; the
    // finalizer casts back to the same type and drops the Box exactly
    // once when V8 reclaims the wrapper. There are no native resources
    // to release in Drop; the binding is dropped with the Box.
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
