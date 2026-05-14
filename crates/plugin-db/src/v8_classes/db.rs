//! `Db` — native V8 wrapper that replaces the frozen `env.db` namespace
//! object with a `#[v8_class]` instance.
//!
//! Stage 2 of the db plugin nativization. The runtime's
//! `DbPlugin::build_instance` hook (landed in `a7261cf`) returns a
//! `Db` instance from this module; `build_env_object` then layers the
//! 27 existing `zeroship.db.*` flat callbacks on top via the
//! `NativeRegistrar` path. Existing user code accessing
//! `zeroship.db.find(...)` keeps working unchanged — the v8_class
//! shape replaces the frozen-namespace value but the API surface stays
//! the same.
//!
//! ## What this class adds
//!
//! - `collection(name)` — returns a [`Collection`] v8_class instance
//!   for the given collection name. Cached by `name`: subsequent calls
//!   for the same `name` return the same `Collection` JS object.
//!
//! The 27 flat callbacks (`find`, `findOne`, `insert`, …) are still
//! attached to this instance as own properties by the registrar — they
//! are NOT methods on the class. This is the documented split from the
//! Stage 2 plan: shipping the v8_class shell + a `collection()` cache
//! without porting every CRUD callback to a `#[v8_method]`. The SDK's
//! `db.users.find(...)` route via `env.db.collection("users").find(...)`
//! internally dispatches back to the underlying flat callback by
//! reading `env.db.find` and calling it with the collection name
//! prepended.
//!
//! ## Why a v8_class
//!
//! Future-facing methods (`collection`, eventually `subscribe`
//! returning a `Subscription` wrapper, `beginTransaction` returning a
//! `Transaction` wrapper) are declared as `#[v8_method]` here and
//! don't have to be wired through `NativeRegistrar`. The brand-check
//! that ships with `#[v8_class]` also gives us a free "is this object
//! my Db instance" guard for any future receiver-shape checks (e.g.
//! `instanceof Db`).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_method};

use crate::v8_classes::collection::mint_collection;

// ---------------------------------------------------------------------------
// Db state
// ---------------------------------------------------------------------------

/// Owned state for the `env.db` v8_class instance.
///
/// Field 0 of the wrapper holds a `Box<Db>` (this struct). The Weak
/// finalizer registered by `mint_db` drops the Box on GC. There are
/// no native resources to release in `Drop` — `app_id` is a String and
/// the `collection_cache` holds `v8::Global<v8::Object>` handles whose
/// own Weak counterparts (registered by `mint_collection`) reclaim
/// the wrapped Collection state.
pub struct Db {
    /// The app_id this Db belongs to. Captured at instance-build time
    /// from `SharedState.env_vars["APP_ID"]`. Avoids a per-callback
    /// slot lookup on the Collection's forwarded dispatch.
    pub(crate) app_id: RefCell<String>,
    /// Cache of `(collection_name -> Collection JS wrapper)`. Populated
    /// on the first `.collection(name)` call for each name; subsequent
    /// calls return the same Global so identity holds:
    /// `env.db.collection("users") === env.db.collection("users")`.
    pub(crate) collection_cache: RefCell<HashMap<String, v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("app_id", &self.app_id.borrow())
            .field("collection_cache_len", &self.collection_cache.borrow().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Db IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Db {
    /// `new Db()` — placeholder. Real instances are minted via
    /// [`mint_db`]; constructing one from JS produces a wrapper with
    /// an empty app_id and an empty cache. The macro requires this
    /// constructor to satisfy the install codegen.
    #[v8_constructor]
    fn new() -> Db {
        Db {
            app_id: RefCell::new(String::new()),
            collection_cache: RefCell::new(HashMap::new()),
        }
    }

    /// `db.collection(name)` — returns a [`Collection`] v8_class
    /// instance bound to this Db and the given collection name.
    ///
    /// Cached by `name`: subsequent calls with the same `name` return
    /// the same JS object (so `db.collection("users") ===
    /// db.collection("users")` holds). The cache lives for the
    /// lifetime of the Db wrapper; entries are dropped when the Db
    /// wrapper is GC'd (the `v8::Global` handles in the cache are
    /// dropped with the Box).
    #[v8_method]
    fn collection<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        wrapper: v8::Local<v8::Object>,
        name: String,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if name.is_empty() {
            return Err(OpError::type_error(
                "db.collection: name must be a non-empty string",
            ));
        }
        // Fast path: existing entry in the cache.
        if let Some(existing) = self.collection_cache.borrow().get(&name) {
            return Ok(v8::Local::new(scope, existing));
        }

        // Slow path: mint a new Collection and stash a Global in the
        // cache so the next call hits the fast path.
        let db_global = v8::Global::new(scope, wrapper);
        let obj = mint_collection(scope, name.clone(), db_global)?;
        let global = v8::Global::new(scope, obj);
        self.collection_cache
            .borrow_mut()
            .insert(name, global);
        Ok(obj)
    }
}

// ---------------------------------------------------------------------------
// mint_db — build a `Db` wrapper for a given app_id
// ---------------------------------------------------------------------------

/// Mint a `Db` v8_class instance with state stamped from `app_id`.
///
/// Called from `DbPlugin::build_instance` once per V8 isolate during
/// `build_env_object`. The returned object becomes the `env.db`
/// namespace value; the runtime then layers the 27 flat callbacks on
/// top via the `NativeRegistrar` returned by `DbPlugin::register`.
pub fn mint_db<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = Db::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = Db {
        app_id: RefCell::new(app_id.to_string()),
        collection_cache: RefCell::new(HashMap::new()),
    };
    let boxed: Box<Db> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Db>`; the
    // finalizer closure casts back to the same type and drops the Box
    // exactly once when V8 reclaims the wrapper. There are no native
    // resources to release; the Collection cache holds
    // `v8::Global<v8::Object>` handles that are dropped together with
    // the Box, and each Collection's own Weak finalizer reclaims its
    // state.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Db));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}
