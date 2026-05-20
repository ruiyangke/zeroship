//! `Collection` — native V8 wrapper for a single named collection.
//!
//! A `Collection` instance is returned by [`super::db::Db::collection`].
//! Each CRUD method on it decodes its V8 arguments directly into a
//! `serde_json::Value` (via [`crate::callbacks::v8_value_to_serde_json`])
//! and calls the shared `dispatch_*` helper in [`crate::callbacks`] —
//! no JSON.stringify / parse round-trip on the CRUD hot path.
//!
//! ## State
//!
//! - `name`: the collection name passed to `Db::collection(name)`.
//! - `db_obj`: a strong `v8::Global<v8::Object>` reference to the
//!   parent Db wrapper. The CRUD v8_methods don't read it (they call
//!   `callbacks::dispatch_*` directly); it's kept so
//!   `openSubscription` — the one method still routed via the parent —
//!   can locate the same-named callback on `env.db`. The reference
//!   cycle (Db cache → Collection → Db) doesn't prevent GC: once JS
//!   drops `env.db`, the only remaining root is the runtime env
//!   object; releasing that frees both wrappers together. (See the
//!   `crates/runtime-macros` README for Weak-finalizer semantics.)

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_name};

use crate::callbacks;

// ---------------------------------------------------------------------------
// Collection state
// ---------------------------------------------------------------------------

pub struct Collection {
    /// The collection name (e.g. `"users"`). Used as the first argument
    /// to every dispatch helper.
    pub(crate) name: RefCell<String>,
    /// The parent Db wrapper. Retained so the subscription methods
    /// (which still forward through JS) can find the matching callback
    /// on the parent Db.
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
// Forwarding helper (still used by `subscribe` / `openSubscription`)
// ---------------------------------------------------------------------------

/// Forward a Collection method to the same-named callback on the
/// parent Db wrapper, with the collection name prepended to the
/// arg list.
///
/// Now only used by the subscription methods — every CRUD method has
/// been converted to a direct native dispatch.
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

    // --- CRUD methods ---
    //
    // Each method walks its V8 args directly into `serde_json::Value`
    // (no JSON.stringify / JSON.parse round trip) and calls into the
    // shared `dispatch_*` helper in `crate::callbacks`. The flat
    // callbacks on `env.db` (still registered for SDK back-compat)
    // call the same helpers.

    #[v8_method]
    #[v8_name = "findOne"]
    fn find_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_find_one(scope, &app_id, &collection, filter_v).into()
    }

    #[v8_method]
    fn find<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        callbacks::dispatch_find(scope, &app_id, &collection, filter_v, opts_v).into()
    }

    #[v8_method]
    fn insert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        doc: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.insert")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let doc_v = callbacks::read_json_arg(scope, Some(doc));
        callbacks::dispatch_insert(scope, &app_id, &collection, doc_v).into()
    }

    #[v8_method]
    #[v8_name = "insertMany"]
    fn insert_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        docs: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.insertMany")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let docs_v = callbacks::read_json_arg(scope, Some(docs));
        callbacks::dispatch_insert_many(scope, &app_id, &collection, docs_v).into()
    }

    #[v8_method]
    #[v8_name = "updateOne"]
    fn update_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        update: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.updateOne")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let update_v = callbacks::read_json_arg(scope, Some(update));
        callbacks::dispatch_update_one(scope, &app_id, &collection, filter_v, update_v).into()
    }

    #[v8_method]
    #[v8_name = "updateMany"]
    fn update_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        update: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.updateMany")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let update_v = callbacks::read_json_arg(scope, Some(update));
        callbacks::dispatch_update_many(scope, &app_id, &collection, filter_v, update_v).into()
    }

    #[v8_method]
    #[v8_name = "deleteOne"]
    fn delete_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.deleteOne")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_delete_one(scope, &app_id, &collection, filter_v).into()
    }

    #[v8_method]
    #[v8_name = "deleteMany"]
    fn delete_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.deleteMany")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_delete_many(scope, &app_id, &collection, filter_v).into()
    }

    #[v8_method]
    fn upsert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        doc: v8::Local<v8::Value>,
        conflict_fields: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        if let Some(p) = callbacks::refuse_if_query_capability(scope, "ctx.db.upsert")
        {
            return p.into();
        }
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let doc_v = callbacks::read_json_arg(scope, Some(doc));
        let conflict_v = callbacks::read_json_arg(scope, Some(conflict_fields));
        callbacks::dispatch_upsert(scope, &app_id, &collection, doc_v, conflict_v).into()
    }

    #[v8_method]
    fn count<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_count(scope, &app_id, &collection, filter_v).into()
    }

    #[v8_method]
    fn distinct<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        field: String,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_distinct(scope, &app_id, &collection, &field, filter_v).into()
    }

    #[v8_method]
    fn aggregate<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        pipeline: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let collection = self.name.borrow().clone();
        let state = callbacks::runtime_state(scope);
        let app_id = callbacks::app_id_for(&state);
        let pipeline_v = callbacks::read_json_arg(scope, Some(pipeline));
        callbacks::dispatch_aggregate(scope, &app_id, &collection, pipeline_v).into()
    }

    /// `collection.openSubscription()` — convenience forwarder to the
    /// parent Db's `openSubscription(name)` entry point. Returns the
    /// Subscription v8_class wrapper whose Weak finalizer closes the
    /// broker handle on GC. The handle-id `subscribe`/`subscribePoll`/
    /// `subscribeClose` flat callbacks were removed when plugin-db
    /// consolidated to the v2 surface.
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
