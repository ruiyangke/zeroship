//! `Collection` — native V8 wrapper for a single named collection.
//!
//! Stage 2 of the db plugin nativization. A `Collection` instance is
//! returned by [`super::db::Db::collection`]; each CRUD method on it
//! forwards to the same-named flat callback on the parent `Db` instance
//! with the collection name prepended.
//!
//! ## Why this is a thin dispatch wrapper
//!
//! The 27 flat callbacks on `env.db` (`find`, `findOne`, `insert`, …)
//! already encode all the SQL/query logic. Re-implementing each one as
//! a `#[v8_method]` on `Collection` would duplicate ~50 LOC of
//! arg-extraction + query-building + spawn-op per method (×16+
//! methods). Instead we forward via JS dispatch: each Collection
//! method reads the same-named property off the parent Db wrapper and
//! calls it with `[name, ...args]`. Zero duplicated SQL; one shared
//! source of truth.
//!
//! The dispatch is straight-through (no `Reflect.apply`), so error
//! propagation, Promise return, and capability gates from the
//! underlying callback all work unchanged.
//!
//! ## State
//!
//! - `name`: the collection name passed to `Db::collection(name)`.
//! - `db_obj`: a strong `v8::Global<v8::Object>` reference to the
//!   parent Db wrapper. The cycle this introduces (Db cache →
//!   Collection → Db) does NOT prevent GC: when JS code drops its
//!   reference to `env.db`, the Db wrapper's only remaining root is
//!   the runtime's environment object, and once that lets go, both
//!   wrappers are collected together. (See the README in
//!   `crates/runtime-macros` for the Weak-finalizer semantics.)

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_name};

// ---------------------------------------------------------------------------
// Collection state
// ---------------------------------------------------------------------------

pub struct Collection {
    /// The collection name (e.g. `"users"`). Passed as the first
    /// argument to every forwarded Db callback.
    pub(crate) name: RefCell<String>,
    /// The parent Db wrapper. Forwarded methods look up the matching
    /// callback on this object and call it with `[name, ...args]`.
    pub(crate) db_obj: RefCell<Option<v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collection")
            .field("name", &self.name.borrow())
            .field("db_obj", &self.db_obj.borrow().is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Forwarding helper
// ---------------------------------------------------------------------------

/// Forward a Collection method to the same-named callback on the
/// parent Db wrapper, with the collection name prepended to the
/// arg list.
///
/// Returns the result of the underlying call directly. On any failure
/// (missing parent, missing method, non-callable, or the underlying
/// call throwing) the V8 scope's pending-exception state is set and
/// we return `v8::undefined`. V8's callback dispatcher checks the
/// pending exception after the callback returns and propagates it to
/// JS regardless of `rv.set` — so returning `undefined` here is safe.
fn forward<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    instance: &Collection,
    method: &str,
    extra_args: &[v8::Local<v8::Value>],
) -> v8::Local<'s, v8::Value> {
    let db_global_opt = instance.db_obj.borrow().as_ref().cloned();
    let Some(db_global) = db_global_opt else {
        let msg = v8::String::new(
            scope,
            "Collection: parent Db wrapper is no longer reachable",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return v8::undefined(scope).into();
    };
    let db_local: v8::Local<v8::Object> = v8::Local::new(scope, &db_global);
    let method_key = v8::String::new(scope, method).unwrap();
    let method_val = match db_local.get(scope, method_key.into()) {
        Some(v) => v,
        None => {
            let msg = v8::String::new(
                scope,
                &format!("Collection.{method}: missing on parent Db"),
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return v8::undefined(scope).into();
        }
    };
    let method_fn: v8::Local<v8::Function> = match method_val.try_into() {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(
                scope,
                &format!("Collection.{method}: parent Db.{method} is not a function"),
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return v8::undefined(scope).into();
        }
    };

    // Build [name, ...extra_args]. Two-deep alloc is acceptable; this
    // is the per-CRUD-call shape and we already pay one promise alloc
    // per call inside the underlying callback.
    let name = instance.name.borrow().clone();
    let name_v = v8::String::new(scope, &name).unwrap();
    let mut call_args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(1 + extra_args.len());
    call_args.push(name_v.into());
    call_args.extend(extra_args.iter().copied());

    // `Function::call` returns None when the call threw; the exception
    // is already on the isolate, so we just need to return *something*.
    // v8::undefined() is the natural sentinel — the rv.set the macro
    // emits is overridden by V8's pending-exception propagation on the
    // way out of the callback.
    match method_fn.call(scope, db_local.into(), &call_args) {
        Some(v) => v,
        None => v8::undefined(scope).into(),
    }
}

// ---------------------------------------------------------------------------
// Collection IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Collection {
    /// Placeholder constructor for the macro. Real instances come from
    /// [`mint_collection`] via `Db::collection(name)`. A direct
    /// `new Collection()` from JS produces a wrapper with no parent
    /// Db, so every method throws.
    #[v8_constructor]
    fn new() -> Collection {
        Collection {
            name: RefCell::new(String::new()),
            db_obj: RefCell::new(None),
        }
    }

    /// `collection.name` — the collection's name. Useful for debugging
    /// + for SDK callers that want to read it without storing it
    /// alongside the wrapper.
    #[v8_getter]
    fn name(&self) -> String {
        self.name.borrow().clone()
    }

    // --- CRUD forwarders ---
    //
    // Each method is a thin dispatcher to the parent Db's same-named
    // flat callback with the collection name prepended. They all
    // return the value the underlying call returns (typically a
    // Promise), so error propagation and Promise semantics route
    // through unchanged.

    #[v8_method]
    #[v8_name = "findOne"]
    fn find_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "findOne", &rest)
    }

    #[v8_method]
    fn find<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "find", &rest)
    }

    #[v8_method]
    fn insert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "insert", &rest)
    }

    #[v8_method]
    #[v8_name = "insertMany"]
    fn insert_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "insertMany", &rest)
    }

    #[v8_method]
    #[v8_name = "updateOne"]
    fn update_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "updateOne", &rest)
    }

    #[v8_method]
    #[v8_name = "updateMany"]
    fn update_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "updateMany", &rest)
    }

    #[v8_method]
    #[v8_name = "deleteOne"]
    fn delete_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "deleteOne", &rest)
    }

    #[v8_method]
    #[v8_name = "deleteMany"]
    fn delete_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "deleteMany", &rest)
    }

    #[v8_method]
    fn upsert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "upsert", &rest)
    }

    #[v8_method]
    fn count<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "count", &rest)
    }

    #[v8_method]
    fn distinct<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "distinct", &rest)
    }

    #[v8_method]
    fn aggregate<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "aggregate", &rest)
    }

    #[v8_method]
    fn subscribe<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "subscribe", &rest)
    }

    /// `collection.openSubscription()` — returns the new v8_class
    /// Subscription wrapper directly. Equivalent to
    /// `env.db.openSubscription(name)` and `subscribePoll` /
    /// `subscribeClose` are NOT called: the wrapper closes its broker
    /// handle on `.close()` or GC.
    #[v8_method]
    #[v8_name = "openSubscription"]
    fn open_subscription<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        rest: Vec<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        forward(scope, self, "openSubscription", &rest)
    }
}

// ---------------------------------------------------------------------------
// mint_collection — build a Collection wrapper for a given Db parent
// ---------------------------------------------------------------------------

/// Mint a `Collection` v8_class instance bound to `db_global` with
/// the given collection `name`.
///
/// Called from `Db::collection(name)` on cache miss. Caller is
/// responsible for stashing the returned wrapper in `Db`'s
/// `collection_cache` so subsequent `.collection(name)` calls return
/// the same JS object.
pub fn mint_collection<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: String,
    db_global: v8::Global<v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let class_tmpl = Collection::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Collection instance allocation failed"))?;

    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Collection template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Collection prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Collection {
        name: RefCell::new(name),
        db_obj: RefCell::new(Some(db_global)),
    };
    let boxed: Box<Collection> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Collection));
        }),
    );
    std::mem::forget(weak);

    Ok(obj)
}
