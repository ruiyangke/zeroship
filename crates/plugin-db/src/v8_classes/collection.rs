//! `Collection` — native V8 wrapper for a single named collection.
//!
//! A `Collection` instance is returned by [`super::db::Db::collection`].
//! Each CRUD method on it decodes its V8 arguments directly into a
//! `serde_json::Value` (via [`crate::callbacks::v8_value_to_serde_json`])
//! and calls the shared `dispatch_*` helper in [`crate::callbacks`] —
//! no JSON.stringify / parse round-trip on the CRUD hot path.

#![allow(unsafe_code)]

use serde_json::Value;
use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_name};

use crate::callbacks;

// ---------------------------------------------------------------------------
// Collection state
// ---------------------------------------------------------------------------

pub struct Collection {
    /// The collection name (e.g. `"users"`). Used as the first argument
    /// to every dispatch helper. Never mutated after mint — plain
    /// `String`, no `RefCell`.
    pub(crate) name: String,
    /// The app_id captured at mint time so CRUD callbacks don't have
    /// to read the runtime slot for every dispatch. Never mutated.
    pub(crate) app_id: String,
}

impl std::fmt::Debug for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collection")
            .field("name", &self.name)
            .field("app_id", &self.app_id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Collection IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Collection {
    /// `new Collection()` from JS rejects — real instances come from
    /// [`mint_collection`] via `Db::collection(name)`, which stamps
    /// `name` + `app_id` onto the wrapper.
    #[v8_constructor]
    fn new() -> Result<Collection, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `collection.findOne(filter, opts?)` — fetch the first matching row.
    ///
    /// `opts.select` (string[]) projects to a subset of columns;
    /// `opts.orderBy` (Record<col, 1|-1>) deterministically picks
    /// which row to return when the filter matches several. `limit` /
    /// `offset` from `find`'s opts are ignored here.
    #[v8_method]
    #[v8_name = "findOne"]
    fn find_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        callbacks::dispatch_find_one(scope, &self.app_id, &self.name, filter_v, opts_v).into()
    }

    #[v8_method]
    fn find<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        callbacks::dispatch_find(scope, &self.app_id, &self.name, filter_v, opts_v).into()
    }

    #[v8_method]
    fn insert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        doc: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.insert")
        {
            return p.into();
        }
        let doc_v = callbacks::read_json_arg(scope, Some(doc));
        callbacks::dispatch_insert(scope, &self.app_id, &self.name, doc_v).into()
    }

    #[v8_method]
    #[v8_name = "insertMany"]
    fn insert_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        docs: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.insertMany")
        {
            return p.into();
        }
        let docs_v = callbacks::read_json_arg(scope, Some(docs));
        callbacks::dispatch_insert_many(scope, &self.app_id, &self.name, docs_v).into()
    }

    #[v8_method]
    #[v8_name = "updateOne"]
    fn update_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        update: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.updateOne")
        {
            return p.into();
        }
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let update_v = callbacks::read_json_arg(scope, Some(update));
        callbacks::dispatch_update_one(scope, &self.app_id, &self.name, filter_v, update_v).into()
    }

    #[v8_method]
    #[v8_name = "updateMany"]
    fn update_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        update: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.updateMany")
        {
            return p.into();
        }
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let update_v = callbacks::read_json_arg(scope, Some(update));
        callbacks::dispatch_update_many(scope, &self.app_id, &self.name, filter_v, update_v).into()
    }

    #[v8_method]
    #[v8_name = "deleteOne"]
    fn delete_one<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.deleteOne")
        {
            return p.into();
        }
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_delete_one(scope, &self.app_id, &self.name, filter_v).into()
    }

    #[v8_method]
    #[v8_name = "deleteMany"]
    fn delete_many<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if let Some(p) =
            callbacks::refuse_if_query_capability(scope, "ctx.db.deleteMany")
        {
            return p.into();
        }
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_delete_many(scope, &self.app_id, &self.name, filter_v).into()
    }

    /// `collection.upsert(doc, opts)` — insert or update on conflict.
    ///
    /// `opts.conflictFields` (string[]) names the ON CONFLICT target
    /// columns; rejects with `TypeError` when missing or empty.
    #[v8_method]
    fn upsert<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        doc: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if let Some(p) = callbacks::refuse_if_query_capability(scope, "ctx.db.upsert")
        {
            return Ok(p.into());
        }
        let doc_v = callbacks::read_json_arg(scope, Some(doc));
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        let conflict_v = opts_v
            .get("conflictFields")
            .cloned()
            .unwrap_or(Value::Null);
        let conflict_arr = match conflict_v.as_array() {
            Some(arr) => arr,
            None => {
                let detail = match &conflict_v {
                    Value::Null => "missing".to_string(),
                    Value::String(_) => "string".to_string(),
                    Value::Number(_) => "number".to_string(),
                    Value::Bool(_) => "boolean".to_string(),
                    Value::Object(_) => "object".to_string(),
                    Value::Array(_) => unreachable!(),
                };
                return Err(OpError::type_error(format!(
                    "upsert: opts.conflictFields must be an array of strings (got {detail})"
                )));
            }
        };
        if conflict_arr.is_empty() {
            return Err(OpError::type_error(
                "upsert: opts.conflictFields must be a non-empty array of strings",
            ));
        }
        Ok(callbacks::dispatch_upsert(scope, &self.app_id, &self.name, doc_v, conflict_v).into())
    }

    #[v8_method]
    fn count<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        callbacks::dispatch_count(scope, &self.app_id, &self.name, filter_v).into()
    }

    /// `collection.distinct(filter, opts)` — return the unique values
    /// of `opts.field` across rows matching `filter`. Filter-first to
    /// match the rest of the read surface.
    #[v8_method]
    fn distinct<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        filter: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let filter_v = callbacks::read_json_arg(scope, Some(filter));
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        let field = opts_v
            .get("field")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OpError::type_error(
                "distinct: opts.field must be a non-empty string",
            ))?
            .to_string();
        Ok(callbacks::dispatch_distinct(scope, &self.app_id, &self.name, &field, filter_v).into())
    }

    #[v8_method]
    fn aggregate<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        pipeline: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let pipeline_v = callbacks::read_json_arg(scope, Some(pipeline));
        callbacks::dispatch_aggregate(scope, &self.app_id, &self.name, pipeline_v).into()
    }

    /// `collection.openSubscription()` — returns a
    /// [`super::subscription::Subscription`] wrapper bound to this
    /// collection. Synchronous mint; broker entry released by the
    /// wrapper's Weak finalizer (or `.close()`).
    #[v8_method]
    #[v8_name = "openSubscription"]
    fn open_subscription<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let obj = super::subscription::mint_subscription(scope, &self.app_id, &self.name)?;
        Ok(obj.into())
    }
}

// ---------------------------------------------------------------------------
// mint_collection — build a Collection wrapper for a given Db parent
// ---------------------------------------------------------------------------

/// Mint a `Collection` v8_class instance bound to the given
/// `(app_id, name)` pair.
///
/// Called from `Db::collection(name)` and `Transaction::collection(name)`
/// on cache miss. Callers stash the returned wrapper in their own
/// `collection_cache` so subsequent `.collection(name)` calls return
/// the same JS object.
pub fn mint_collection<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: String,
    app_id: String,
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
        name,
        app_id,
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
