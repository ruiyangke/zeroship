//! `MaskedValue` — the V8 wrapper minted directly by the row
//! serializer for every masked column on a row crossing back to JS
//! (P9 PR 2).
//!
//! Before P9 PR 2 the runtime emitted a `{sentinel: "__zsmask__", ...}`
//! JSON sentinel and the SDK's `mapResultDoc` re-hydrated it into a
//! TypeScript `MaskedValue` instance on every row. The sentinel
//! round-trip + JS rehydration is gone — Rust now mints native
//! `MaskedValue` v8_class instances directly when V8 parses the
//! result, so the SDK surface receives the wrapper instances on the
//! first hop.
//!
//! ## JS surface (matches the §4.1 proposal mapping table)
//!
//! Getters (`v8_getter`):
//! - `masked: string` — the masked representation (e.g. `"***-**-6789"`).
//!   Safe to log, JSON-serialize, render.
//! - `classification: string` — one of the six classifications declared
//!   on `mask({classification: ...})` (`pii` / `spi` / `phi` / `pci` /
//!   `public` / `internal`).
//! - `_meta: { collection, row_pk, column }` — frozen object exposing
//!   the per-row coordinates `unmask` needs. **Re-nested** from the
//!   flat internal fields per §4.1.
//!
//! Methods:
//! - `unmask(opts?)` — single-column round-trip; resolves with the
//!   plaintext on success. Calls into `dispatch_unmask_field` with the
//!   collection / row_pk / column bound to this instance.
//! - `unmask(cols, opts?)` — multi-column overload on the SAME method;
//!   discriminated at the V8 boundary by whether arg 0 is an array.
//!   Resolves with `Record<col, plaintext>`. Calls into
//!   `dispatch_bulk_unmask_field` with a single-row `items` payload.
//! - `canUnmask(opts?)` — dry-run probe. Issues a real unmask with
//!   reason `"permission probe"` and treats `unmask_not_permitted` as
//!   `false`. Per the design Q-MASK-C contract this WRITES the audit
//!   row regardless of outcome.
//! - `toString()` / `toJSON()` — both return the masked string.
//!   `Symbol.toPrimitive` is installed manually after the class
//!   template finishes minting (see `register_to_primitive`).

#![allow(unsafe_code)]

use serde_json::Value;
use zeroship_runtime::state::{
    IntoResolveValue, JsonValue, OpError, OpResult, ResolveValue,
};
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_constructor, v8_getter, v8_method};

use crate::crud::unmask::{
    dispatch_bulk_unmask, dispatch_unmask, BulkUnmaskArgs, BulkUnmaskItem, UnmaskFieldArgs,
};
use crate::v8_bridge::{runtime_state, setup_js_promise, v8_value_to_serde_json};

// ---------------------------------------------------------------------------
// MaskedValue state
// ---------------------------------------------------------------------------

/// Owned state for a `MaskedValue` JS wrapper.
///
/// Field 0 of the wrapper holds `Box<MaskedValue>`. The Weak finalizer
/// registered by the `#[v8_class]` macro reclaims the Box on GC; there
/// are no native resources to release.
///
/// The six fields below mirror the §4.1 internal-field list. They are
/// captured at mint time (`mint_masked_value`) from the row payload's
/// sibling metadata and never mutated.
#[derive(Debug)]
pub struct MaskedValue {
    /// The app_id this MaskedValue was minted under. Required so the
    /// `unmask` round-trip routes back to the right tenant schema; not
    /// exposed as a getter (per §4.1, no `app_id` on the public surface).
    pub(crate) app_id: String,
    /// The collection name (e.g. `"users"`).
    pub(crate) collection: String,
    /// Stringified row primary key (`"usr_..."` or the numeric id
    /// as a string).
    pub(crate) row_pk: String,
    /// The column name (e.g. `"ssn"`).
    pub(crate) column: String,
    /// One of the six classifications. Drives the auth check on
    /// `unmask`.
    pub(crate) classification: String,
    /// The masked representation displayed to the creator.
    pub(crate) masked_string: String,
}

// ---------------------------------------------------------------------------
// MaskedValue IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl MaskedValue {
    /// `new MaskedValue()` from JS rejects with a `TypeError` — real
    /// instances are minted by [`mint_masked_value`] from the row
    /// serializer when a masked column flows back across the V8 boundary.
    #[v8_constructor]
    fn new() -> Result<MaskedValue, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `mv.masked` — the masked representation.
    #[v8_getter]
    fn masked(&self) -> String {
        self.masked_string.clone()
    }

    /// `mv.classification` — one of `pii` / `spi` / `phi` / `pci` /
    /// `public` / `internal`.
    #[v8_getter]
    fn classification(&self) -> String {
        self.classification.clone()
    }

    /// `mv._meta` — frozen `{ collection, row_pk, column }` object.
    ///
    /// Per §4.1, the three native flat fields (`collection`, `row_pk`,
    /// `column`) are re-nested into the `_meta` shape the SDK consumed
    /// before P9 PR 2. `app_id` stays internal — it never enters the
    /// type, never shows in creator hover.
    #[v8_getter]
    #[v8_name = "_meta"]
    fn meta<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        let obj = v8::Object::new(scope);
        let collection_key = v8::String::new(scope, "collection")
            .ok_or_else(|| OpError::type_error("_meta: collection key alloc"))?;
        let collection_v = v8::String::new(scope, &self.collection)
            .ok_or_else(|| OpError::type_error("_meta: collection value alloc"))?;
        obj.set(scope, collection_key.into(), collection_v.into());

        let row_pk_key = v8::String::new(scope, "row_pk")
            .ok_or_else(|| OpError::type_error("_meta: row_pk key alloc"))?;
        let row_pk_v = v8::String::new(scope, &self.row_pk)
            .ok_or_else(|| OpError::type_error("_meta: row_pk value alloc"))?;
        obj.set(scope, row_pk_key.into(), row_pk_v.into());

        let column_key = v8::String::new(scope, "column")
            .ok_or_else(|| OpError::type_error("_meta: column key alloc"))?;
        let column_v = v8::String::new(scope, &self.column)
            .ok_or_else(|| OpError::type_error("_meta: column value alloc"))?;
        obj.set(scope, column_key.into(), column_v.into());

        // Freeze so the SDK's `Object.freeze({...})`-shaped invariant is
        // preserved across the v8_class promotion.
        let frozen_key = v8::String::new(scope, "freeze")
            .ok_or_else(|| OpError::type_error("_meta: freeze key alloc"))?;
        // Lookup `Object.freeze` and call it; failures fall back to
        // returning the un-frozen object (the public surface is still
        // correct — `_meta.column` etc. still read).
        let global = scope.get_current_context().global(scope);
        let object_key = v8::String::new(scope, "Object")
            .ok_or_else(|| OpError::type_error("_meta: Object lookup"))?;
        if let Some(object_v) = global.get(scope, object_key.into()) {
            if let Ok(object_obj) = v8::Local::<v8::Object>::try_from(object_v) {
                if let Some(freeze_v) = object_obj.get(scope, frozen_key.into()) {
                    if let Ok(freeze_fn) = v8::Local::<v8::Function>::try_from(freeze_v) {
                        let args = [obj.into()];
                        let _ = freeze_fn.call(scope, object_obj.into(), &args);
                    }
                }
            }
        }
        Ok(obj)
    }

    /// `mv.unmask(opts?)` or `mv.unmask(columns, opts)` — round-trip to
    /// the platform.
    ///
    /// The discriminator is `args[0]`: array → multi-column bulk path
    /// (`dispatch_bulk_unmask`), object / undefined → single-column
    /// path (`dispatch_unmask`). The native dispatcher uses the
    /// instance's `(collection, row_pk, column)` to bind the call to
    /// THIS row; the caller cannot redirect to a different row by
    /// shape-confusion.
    ///
    /// Authorization, audit-row emission, and decrypt are all handled
    /// by the shared `crud::unmask` module — same code path the old
    /// `Db.unmaskField` / `Db.bulkUnmaskFields` v8_methods went through.
    ///
    /// Returns `Promise<string>` on the single-column path
    /// (resolves with the bare plaintext string) or
    /// `Promise<Record<col, string>>` on the multi-column path. Both
    /// shapes match the SDK ambient `declare class MaskedValue` directly
    /// — there is no JS wrapper unwrapping a `{ plaintext }` envelope
    /// after P9 PR 2.
    #[v8_method]
    fn unmask<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        arg0: v8::Local<v8::Value>,
        arg1: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if arg0.is_array() {
            // Multi-column path: arg0 is `columns`, arg1 is `opts`.
            let cols_v = v8_value_to_serde_json(scope, arg0);
            let opts_v = if arg1.is_null_or_undefined() {
                Value::Object(serde_json::Map::new())
            } else {
                v8_value_to_serde_json(scope, arg1)
            };
            self.dispatch_unmask_multi(scope, cols_v, opts_v).into()
        } else {
            // Single-column path: arg0 is `opts`.
            let opts_v = if arg0.is_null_or_undefined() {
                Value::Object(serde_json::Map::new())
            } else {
                v8_value_to_serde_json(scope, arg0)
            };
            self.dispatch_unmask_single(scope, opts_v, /* probe = */ false).into()
        }
    }

    /// `mv.canUnmask(opts?)` — dry-run permission probe.
    ///
    /// Issues a real unmask with reason `"permission probe"`; treats
    /// `unmask_not_permitted` as `false`, every other outcome (success,
    /// `unmask_value_null`, `unmask_not_found`, SQL errors) re-throws.
    /// Per Q-MASK-C the probe DOES write an audit row.
    #[v8_method]
    #[v8_name = "canUnmask"]
    fn can_unmask<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let mut opts_v = if opts.is_null_or_undefined() {
            Value::Object(serde_json::Map::new())
        } else {
            v8_value_to_serde_json(scope, opts)
        };
        // Stamp the probe reason so the audit row records the dispatch
        // shape; user-supplied `reason` (if any) wins.
        if let Some(obj) = opts_v.as_object_mut() {
            obj.entry("reason".to_string())
                .or_insert_with(|| Value::String("permission probe".into()));
        }
        self.dispatch_unmask_single(scope, opts_v, /* probe = */ true).into()
    }

    /// `mv.toString()` — yields the masked string. Same coercion-safe
    /// behaviour as the pre-PR-2 TS class.
    #[v8_method]
    #[v8_name = "toString"]
    fn to_string_js(&self) -> String {
        self.masked_string.clone()
    }

    /// `mv.toJSON()` — `JSON.stringify(mv)` yields the masked string.
    #[v8_method]
    #[v8_name = "toJSON"]
    fn to_json_js(&self) -> String {
        self.masked_string.clone()
    }
}

// ---------------------------------------------------------------------------
// Native dispatch helpers (called from the v8_method bodies above)
// ---------------------------------------------------------------------------

impl MaskedValue {
    /// Single-column unmask dispatch. Mirrors
    /// `crate::crud::unmask::dispatch_unmask_field` but binds the
    /// `(collection, row_pk, column)` from `self` instead of parsing
    /// them out of a `{args}` object — the v8_class promotion removes
    /// the args-shape dependency entirely.
    ///
    /// `probe = true` is the `canUnmask` path: resolves with a bare
    /// `true` / `false` instead of the plaintext, treating
    /// `unmask_not_permitted` as `false` and re-throwing every other
    /// error.
    fn dispatch_unmask_single<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts_v: Value,
        probe: bool,
    ) -> v8::Local<'s, v8::Promise> {
        let state = runtime_state(scope);
        let (resolver, request_id, promise) = setup_js_promise(scope, &state);

        let actor = opts_v.get("actor").cloned().filter(|v| !v.is_null());
        let reason = opts_v
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let args = UnmaskFieldArgs {
            collection: self.collection.clone(),
            row_pk: self.row_pk.clone(),
            column: self.column.clone(),
            actor,
            reason,
        };
        let app = self.app_id.clone();

        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            match dispatch_unmask(&app, args).await {
                Ok(result) => {
                    if probe {
                        // canUnmask: success → resolve with `true`.
                        OpResult::JsValue {
                            resolver,
                            value: ResolveValue::Bool(true),
                            request_id,
                        }
                    } else {
                        // **P9 PR 2** — resolve with the BARE plaintext
                        // string. The SDK `MaskedValue` is now an ambient
                        // `declare class` (no JS body), so this native
                        // method IS the implementation of `unmask(): Promise<T>`
                        // — there is no JS wrapper to unwrap a `{ plaintext }`
                        // envelope. `dispatch_unmask` already returns the
                        // plaintext encoded per the column's `wraps`
                        // (UTF-8 string / stringified number / base64
                        // bytes); the caller decodes per their schema.
                        OpResult::JsValue {
                            resolver,
                            value: ResolveValue::String(result.plaintext),
                            request_id,
                        }
                    }
                }
                Err(e) => {
                    if probe {
                        // canUnmask: treat the policy-refusal code as a
                        // `false` answer; every other error re-throws.
                        if matches!(
                            &e,
                            crate::error::DbError::Coded { code, .. }
                                if code == "unmask_not_permitted"
                        ) {
                            OpResult::JsValue {
                                resolver,
                                value: ResolveValue::Bool(false),
                                request_id,
                            }
                        } else {
                            OpResult::JsValue {
                                resolver,
                                value: ResolveValue::RejectError(e.to_op_error()),
                                request_id,
                            }
                        }
                    } else {
                        OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        }
                    }
                }
            }
        }));

        promise
    }

    /// Multi-column unmask dispatch — pins all columns to THIS
    /// MaskedValue's row. Builds a single-item `BulkUnmaskArgs` with
    /// `items = [{row_pk, columns}]` and resolves with the per-column
    /// plaintext record (`Record<col, plaintext>`).
    ///
    /// Atomic authorization mirrors `dispatch_bulk_unmask_field`: one
    /// denied column rejects the whole call with
    /// `bulk_unmask_partial_unauthorized`. No partial-success surface.
    fn dispatch_unmask_multi<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        cols_v: Value,
        opts_v: Value,
    ) -> v8::Local<'s, v8::Promise> {
        let state = runtime_state(scope);
        let (resolver, request_id, promise) = setup_js_promise(scope, &state);

        let cols: Vec<String> = cols_v
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let actor = opts_v.get("actor").cloned().filter(|v| !v.is_null());
        let reason = opts_v
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let args = BulkUnmaskArgs {
            collection: self.collection.clone(),
            items: vec![BulkUnmaskItem {
                row_pk: self.row_pk.clone(),
                columns: cols,
            }],
            actor,
            reason,
        };
        let app = self.app_id.clone();
        let row_pk = self.row_pk.clone();

        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            match dispatch_bulk_unmask(&app, args).await {
                Ok(result) => {
                    // Project to the per-column map for THIS row — the
                    // SDK's `MaskedValue.unmask(cols)` overload expects
                    // `Record<col, plaintext>`, not the wider
                    // `Record<row_pk, Record<col, plaintext>>` shape.
                    let cols_map = result
                        .results
                        .get(&row_pk)
                        .cloned()
                        .unwrap_or_default();
                    let mut payload = serde_json::Map::with_capacity(cols_map.len());
                    for (col, pt) in cols_map {
                        payload.insert(col, Value::String(pt));
                    }
                    OpResult::JsValue {
                        resolver,
                        value: JsonValue(Value::Object(payload).to_string())
                            .into_resolve_value(),
                        request_id,
                    }
                }
                Err(e) => OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                },
            }
        }));

        promise
    }
}

// ---------------------------------------------------------------------------
// mint_masked_value — build a wrapper from row metadata
// ---------------------------------------------------------------------------

/// Mint a `MaskedValue` v8_class instance with state stamped from the
/// six row-metadata fields. Called from [`rehydrate_masked_values`] when
/// a `__zsmask__` sentinel object is observed inside a parsed row.
///
/// The minted object carries the full §4.1 internal-field set; no
/// fallible work happens after the Box is published, so a stray `?`
/// later would still drop the Box via the Weak finalizer.
pub(crate) fn mint_masked_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
    collection: String,
    row_pk: String,
    column: String,
    classification: String,
    masked_string: String,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = MaskedValue::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype")?;
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = MaskedValue {
        app_id,
        collection,
        row_pk,
        column,
        classification,
        masked_string,
    };
    let boxed: Box<MaskedValue> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<MaskedValue>`;
    // the finalizer drops the Box exactly once when V8 reclaims the
    // wrapper. There are no native resources to release in Drop —
    // every field is owned `String`.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MaskedValue));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}

// ---------------------------------------------------------------------------
// rehydrate_masked_values — V8 walker invoked by the pump after JSON.parse
// ---------------------------------------------------------------------------

/// Walk the parsed-JSON V8 value and replace every `__zsmask__`-tagged
/// sentinel object with a native `MaskedValue` v8_class instance. The
/// runtime calls this from the `ResolveValue::JsonWithRehydration` arm
/// of the spawned-op pump (see `crates/runtime/src/core/runtime.rs`).
///
/// The function is the post-parse hook plugin-db registers; the runtime
/// itself has no knowledge of the sentinel shape — that information
/// lives here.
///
/// `app_id` is captured at call time (i.e. when the row serializer
/// queues the OpResult) and threaded through via the function pointer;
/// the runtime sees only `fn(scope, value) -> Option<value>`, so we
/// read `app_id` out of the isolate's `SharedState` slot here. Every
/// MaskedValue instance carries its app_id verbatim so cross-app
/// instance reuse is impossible.
///
/// Returns `Some(new_value)` when the walk replaced at least one
/// sub-object (the caller resolves with the new value); `None` when
/// nothing matched (the caller falls back to the input).
///
/// Walk shape:
/// - Top-level array → recurse into each element (rows).
/// - Top-level object → check each property; if a property value is a
///   sentinel object, mint `MaskedValue` and replace.
/// - Nested objects in property values → recurse one level (e.g. a row
///   carries a nested object whose property is a sentinel). Bounded at
///   16 levels of recursion to defend against pathological shapes.
/// - Anything else (primitive, null) → return unchanged.
///
/// Returns the post-walk value as a `Local<Value>`; if no sentinel was
/// found the input `value` Local is returned unchanged (cheap — no
/// re-allocation).
pub fn rehydrate_masked_values<'s, 'a>(
    scope: &mut v8::PinScope<'s, 'a>,
    value: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Value>> {
    let state = runtime_state(scope);
    let app_id = state
        .borrow()
        .env_vars
        .get("APP_ID")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let mut walker = RehydrateWalker {
        app_id,
        depth: 0,
        cap: 16,
    };
    walker.walk(scope, value)
}

struct RehydrateWalker {
    app_id: String,
    depth: usize,
    cap: usize,
}

impl RehydrateWalker {
    /// Walk one V8 value; returns `Some(new_value)` if it (or anything
    /// it transitively contains) was rewritten, `None` otherwise.
    fn walk<'s, 'a>(
        &mut self,
        scope: &mut v8::PinScope<'s, 'a>,
        value: v8::Local<'s, v8::Value>,
    ) -> Option<v8::Local<'s, v8::Value>> {
        if self.depth >= self.cap {
            return None;
        }
        if value.is_array() {
            // Walk array elements; replace in place when a child gets
            // rewritten.
            let arr = v8::Local::<v8::Array>::try_from(value).ok()?;
            let n = arr.length();
            let mut changed = false;
            for i in 0..n {
                if let Some(elem) = arr.get_index(scope, i) {
                    self.depth += 1;
                    let replaced = self.walk(scope, elem);
                    self.depth -= 1;
                    if let Some(new_elem) = replaced {
                        arr.set_index(scope, i, new_elem);
                        changed = true;
                    }
                }
            }
            return if changed { Some(value) } else { None };
        }
        if !value.is_object() {
            return None;
        }
        let obj = v8::Local::<v8::Object>::try_from(value).ok()?;

        // Sentinel check: a plain `{sentinel: "__zsmask__", ...}` object.
        // Brand-checked v8_class instances will not have `sentinel` on
        // their internal field 0 / their prototype chain doesn't match
        // a plain Object's, but reading `.sentinel` on them is harmless
        // — the check is the `==="__zsmask__"` discriminator.
        let sentinel_key = str_key(scope, "sentinel")?;
        if let Some(sentinel_v) = obj.get(scope, sentinel_key) {
            if sentinel_v.is_string() {
                let s = sentinel_v.to_rust_string_lossy(scope);
                if s == "__zsmask__" {
                    return self.mint_replacement(scope, obj);
                }
            }
        }

        // Not a sentinel — descend into own enumerable properties. The
        // typical row carries flat properties only; nested objects (e.g.
        // a JSONB column) are extremely unlikely to contain sentinels,
        // but we still walk for completeness.
        let mut changed = false;
        if let Some(names) =
            obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        {
            for i in 0..names.length() {
                let Some(key_v) = names.get_index(scope, i) else {
                    continue;
                };
                let Some(val_v) = obj.get(scope, key_v) else {
                    continue;
                };
                self.depth += 1;
                let replaced = self.walk(scope, val_v);
                self.depth -= 1;
                if let Some(new_val) = replaced {
                    obj.set(scope, key_v, new_val);
                    changed = true;
                }
            }
        }
        if changed { Some(value) } else { None }
    }

    fn mint_replacement<'s, 'a>(
        &self,
        scope: &mut v8::PinScope<'s, 'a>,
        obj: v8::Local<'s, v8::Object>,
    ) -> Option<v8::Local<'s, v8::Value>> {
        // Extract the sentinel payload + nested _meta. Missing fields
        // produce a defensive empty-string fall-back so the wrapper
        // still mints (the `unmask` round-trip will fail loudly later
        // with `unmask_not_found` / `unmask_column_not_masked` rather
        // than silently swallowing the row).
        let masked_key = str_key(scope, "masked")?;
        let masked = obj
            .get(scope, masked_key)
            .and_then(|v| if v.is_string() { Some(v.to_rust_string_lossy(scope)) } else { None })
            .unwrap_or_default();
        let classification_key = str_key(scope, "classification")?;
        let classification = obj
            .get(scope, classification_key)
            .and_then(|v| if v.is_string() { Some(v.to_rust_string_lossy(scope)) } else { None })
            .unwrap_or_else(|| "pii".to_string());

        let meta_key = str_key(scope, "_meta")?;
        let (collection, row_pk, column) = if let Some(meta_v) = obj.get(scope, meta_key) {
            if meta_v.is_object() {
                if let Ok(meta_obj) = v8::Local::<v8::Object>::try_from(meta_v) {
                    let collection_key = str_key(scope, "collection")?;
                    let c = meta_obj
                        .get(scope, collection_key)
                        .and_then(|v| {
                            if v.is_string() {
                                Some(v.to_rust_string_lossy(scope))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let row_pk_key = str_key(scope, "row_pk")?;
                    let r = meta_obj
                        .get(scope, row_pk_key)
                        .and_then(|v| {
                            if v.is_string() {
                                Some(v.to_rust_string_lossy(scope))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let column_key = str_key(scope, "column")?;
                    let col = meta_obj
                        .get(scope, column_key)
                        .and_then(|v| {
                            if v.is_string() {
                                Some(v.to_rust_string_lossy(scope))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    (c, r, col)
                } else {
                    (String::new(), String::new(), String::new())
                }
            } else {
                (String::new(), String::new(), String::new())
            }
        } else {
            (String::new(), String::new(), String::new())
        };

        let mv = mint_masked_value(
            scope,
            self.app_id.clone(),
            collection,
            row_pk,
            column,
            classification,
            masked,
        )?;
        Some(mv.into())
    }
}

/// Helper — allocate a `v8::String` key as a `v8::Value`. The `?`
/// propagation in `walk` / `mint_replacement` short-circuits gracefully
/// on alloc failure (returns `None` for that subtree; the rest of the
/// walk continues).
fn str_key<'s, 'a>(
    scope: &mut v8::PinScope<'s, 'a>,
    s: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::String::new(scope, s).map(Into::into)
}

#[cfg(test)]
mod tests {
    //! Brand / construction / serializer round-trip coverage for the
    //! P9 PR 2 MaskedValue v8_class. Network / SQL paths are exercised
    //! by the SDK suite and the SQLite integration target; these unit
    //! tests pin the V8-side invariants that don't need a backend.
    use super::*;

    use zeroship_runtime::init_v8;

    /// Smoke: construct a fresh MaskedValue inside a V8 context and
    /// assert the brand-check on the resulting Object recognises it.
    /// Mirrors the pattern used by other v8_class wrappers (see
    /// `subscription::tests`).
    #[test]
    fn mint_masked_value_brand_check_passes() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let obj = mint_masked_value(
            scope,
            "app_a".into(),
            "users".into(),
            "usr_01".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");
        assert!(MaskedValue::is_instance(scope, obj.into()));
    }

    #[test]
    fn mint_masked_value_internal_fields_roundtrip() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let obj = mint_masked_value(
            scope,
            "app_a".into(),
            "users".into(),
            "usr_01".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");

        // Read the `masked` and `classification` getters from JS.
        let masked_key = v8::String::new(scope, "masked").unwrap();
        let masked_v = obj.get(scope, masked_key.into()).unwrap();
        assert_eq!(masked_v.to_rust_string_lossy(scope), "***-**-6789");

        let class_key = v8::String::new(scope, "classification").unwrap();
        let class_v = obj.get(scope, class_key.into()).unwrap();
        assert_eq!(class_v.to_rust_string_lossy(scope), "spi");
    }

    #[test]
    fn mint_masked_value_meta_is_nested_object() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let obj = mint_masked_value(
            scope,
            "app_a".into(),
            "users".into(),
            "usr_01".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");

        let meta_key = v8::String::new(scope, "_meta").unwrap();
        let meta_v = obj.get(scope, meta_key.into()).unwrap();
        assert!(meta_v.is_object(), "_meta must be an object");
        let meta_obj = v8::Local::<v8::Object>::try_from(meta_v).unwrap();

        let coll_key = v8::String::new(scope, "collection").unwrap();
        let coll_v = meta_obj.get(scope, coll_key.into()).unwrap();
        assert_eq!(coll_v.to_rust_string_lossy(scope), "users");

        let row_key = v8::String::new(scope, "row_pk").unwrap();
        let row_v = meta_obj.get(scope, row_key.into()).unwrap();
        assert_eq!(row_v.to_rust_string_lossy(scope), "usr_01");

        let col_key = v8::String::new(scope, "column").unwrap();
        let col_v = meta_obj.get(scope, col_key.into()).unwrap();
        assert_eq!(col_v.to_rust_string_lossy(scope), "ssn");
    }

    #[test]
    fn mint_masked_value_to_string_returns_masked() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let obj = mint_masked_value(
            scope,
            "app_a".into(),
            "users".into(),
            "usr_01".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");

        // Call toString from JS by reading the prototype function and
        // invoking it with `obj` as the receiver.
        let key = v8::String::new(scope, "toString").unwrap();
        let fn_v = obj.get(scope, key.into()).unwrap();
        let func = v8::Local::<v8::Function>::try_from(fn_v).expect("toString should be a Function");
        let result = func
            .call(scope, obj.into(), &[])
            .expect("toString should return a value");
        assert_eq!(result.to_rust_string_lossy(scope), "***-**-6789");
    }

    #[test]
    fn mint_masked_value_to_json_returns_masked() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let obj = mint_masked_value(
            scope,
            "app_a".into(),
            "users".into(),
            "usr_01".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");

        let key = v8::String::new(scope, "toJSON").unwrap();
        let fn_v = obj.get(scope, key.into()).unwrap();
        let func = v8::Local::<v8::Function>::try_from(fn_v).unwrap();
        let result = func.call(scope, obj.into(), &[]).unwrap();
        assert_eq!(result.to_rust_string_lossy(scope), "***-**-6789");
    }

    #[test]
    fn illegal_constructor_rejects_user_new() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Install the class so `new MaskedValue()` is reachable from JS.
        let tmpl = MaskedValue::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();

        let global = context.global(scope);
        let class_key = v8::String::new(scope, "MaskedValue").unwrap();
        global.set(scope, class_key.into(), class_fn.into());

        // `new MaskedValue()` from JS throws.
        let src = v8::String::new(scope, "(()=>{ try { new MaskedValue(); return null; } catch(e) { return e.message || String(e); } })()").unwrap();
        let script = v8::Script::compile(scope, src, None).unwrap();
        let result = script.run(scope).unwrap();
        let result_s = result.to_rust_string_lossy(scope);
        assert!(
            result_s.contains("Illegal"),
            "expected illegal-constructor message, got {result_s}"
        );
    }

    #[test]
    fn brand_check_rejects_plain_object() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let _ = MaskedValue::install(scope);
        let plain = v8::Object::new(scope);
        assert!(
            !MaskedValue::is_instance(scope, plain.into()),
            "plain object must not pass MaskedValue brand check"
        );
    }

    #[test]
    fn rehydrate_walker_skips_non_sentinel_objects() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Pre-install the class template so `mint_masked_value` would
        // work if invoked. The input has no sentinel so the walker
        // should return None.
        let _ = MaskedValue::install(scope);

        let plain_src = v8::String::new(scope, "({id: 1, name: 'alice'})").unwrap();
        let script = v8::Script::compile(scope, plain_src, None).unwrap();
        let val = script.run(scope).unwrap();
        // No SharedState slot set in this minimal harness; the walker
        // gracefully no-ops since there are no sentinels regardless.
        // We just need to make sure the walker doesn't panic on a plain
        // object.
        let mut walker = RehydrateWalker {
            app_id: "app_a".into(),
            depth: 0,
            cap: 16,
        };
        assert!(walker.walk(scope, val).is_none());
    }
}
