//! `MaskedValue` - the V8 wrapper `v8_values::encode` mints from each
//! masked cell (`zeroship_data_orm::value::MaskedCell`) of a result
//! crossing back to JS.
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
//!   plaintext on success. Calls into `protection::unmask::dispatch_unmask` with the
//!   collection / row_pk / column bound to this instance.
//! - `unmask(cols, opts?)` — multi-column overload on the SAME method;
//!   discriminated at the V8 boundary by whether arg 0 is an array.
//!   Resolves with `Record<col, plaintext>`. Calls into
//!   `protection::unmask::dispatch_bulk_unmask` with a single-row `items` payload.
//! - `canUnmask(opts?)` — dry-run probe. Issues a real unmask with
//!   reason `"permission probe"` and treats `unmask_not_permitted` as
//!   `false`. The probe WRITES the audit row regardless of outcome.
//! - `toString()` / `toJSON()` - both return the masked string, so string
//!   coercion, template literals and `JSON.stringify` yield it too.

#![allow(unsafe_code)]

use crate::op_error::ToOpError;
use zeroship_data_orm::value::Value;
use zeroship_runtime::state::{OpError, OpResult, ResolveValue};
use zeroship_runtime_macros::v8_class;

use crate::v8_bridge::{decode_native, runtime_state, setup_js_promise};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::protection::unmask::{
    BulkUnmaskArgs, BulkUnmaskItem, UnmaskFieldArgs, dispatch_bulk_unmask, dispatch_unmask,
};

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
/// captured at mint time (`mint_masked_value`) from the read pipeline's
/// masked cell and never mutated.
#[derive(Debug)]
pub struct MaskedValue {
    /// The app-at-deploy identity this MaskedValue was minted under.
    /// Required so the `unmask` round-trip routes back to the right tenant
    /// schema AND resolves the column's mask/encryption metadata out of the
    /// deploy that produced the row; not exposed as a getter (per §4.1, no
    /// `app_id` on the public surface).
    pub(crate) binding: DbBinding,
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
    /// `column`) are re-nested into the `_meta` shape the SDK consumes.
    /// The binding stays internal — it never enters the
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
    /// by the shared `protection::unmask` module.
    ///
    /// Returns `Promise<string>` on the single-column path
    /// (resolves with the bare plaintext string) or
    /// `Promise<Record<col, string>>` on the multi-column path. Both
    /// shapes match the SDK ambient `declare class MaskedValue` directly
    /// — there is no JS wrapper unwrapping a `{ plaintext }` envelope.
    #[v8_method]
    fn unmask<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        arg0: v8::Local<v8::Value>,
        arg1: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        if arg0.is_array() {
            // Multi-column path: arg0 is `columns`, arg1 is `opts`.
            let cols_v = match decode_native(scope, arg0) {
                Ok(v) => v,
                Err(e) => return crate::v8_bridge::throw_decode_error(scope, &e),
            };
            let opts_v = if arg1.is_null_or_undefined() {
                Value::Object(zeroship_data_orm::value::Map::new())
            } else {
                match decode_native(scope, arg1) {
                    Ok(v) => v,
                    Err(e) => return crate::v8_bridge::throw_decode_error(scope, &e),
                }
            };
            self.dispatch_unmask_multi(scope, cols_v, opts_v).into()
        } else {
            // Single-column path: arg0 is `opts`.
            let opts_v = if arg0.is_null_or_undefined() {
                Value::Object(zeroship_data_orm::value::Map::new())
            } else {
                match decode_native(scope, arg0) {
                    Ok(v) => v,
                    Err(e) => return crate::v8_bridge::throw_decode_error(scope, &e),
                }
            };
            self.dispatch_unmask_single(scope, opts_v, /* probe = */ false)
                .into()
        }
    }

    /// `mv.canUnmask(opts?)` — dry-run permission probe.
    ///
    /// Issues a real unmask with reason `"permission probe"`; treats
    /// `unmask_not_permitted` as `false`, every other outcome (success,
    /// `unmask_value_null`, `unmask_not_found`, SQL errors) re-throws.
    /// The probe DOES write an audit row.
    #[v8_method]
    #[v8_name = "canUnmask"]
    fn can_unmask<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let mut opts_v = if opts.is_null_or_undefined() {
            Value::Object(zeroship_data_orm::value::Map::new())
        } else {
            match decode_native(scope, opts) {
                Ok(v) => v,
                Err(e) => return crate::v8_bridge::throw_decode_error(scope, &e),
            }
        };
        // Stamp the probe reason so the audit row records the dispatch
        // shape; user-supplied `reason` (if any) wins.
        if let Some(obj) = opts_v.as_object_mut() {
            obj.entry("reason".to_string())
                .or_insert_with(|| Value::String("permission probe".into()));
        }
        self.dispatch_unmask_single(scope, opts_v, /* probe = */ true)
            .into()
    }

    /// `mv.toString()` — yields the masked string. Same coercion-safe
    /// behaviour as the original TS class.
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
    /// `zeroship_data_orm::protection::unmask::dispatch_unmask_field` but binds the
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

        // DB-3: app JS must not be able to claim the reserved `auto` system
        // actor — strip it so an app handler cannot impersonate the platform.
        let sanitized = zeroship_data_orm::protection::unmask::sanitize_app_actor(
            opts_v.get("actor").cloned().filter(|v| !v.is_null()),
        );
        let reason = opts_v
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let args = UnmaskFieldArgs {
            collection: self.collection.clone(),
            row_pk: self.row_pk.clone(),
            column: self.column.clone(),
            actor: sanitized.actor,
            reason,
            rejected_claim: sanitized.rejected_claim,
        };
        let binding = self.binding.clone();
        // The route is captured HERE, on the adapter side, while the V8 frame
        // is live, and handed to the engine. The ciphertext read IS a statement,
        // and `row.ssn.unmask()` inside a `db.transaction(fn)` callback over a
        // row that transaction just wrote has to issue it on the transaction's
        // connection.
        let route = crate::startup_policy::require_finalized(scope)
            .and_then(|()| crate::tx_scope::capture_route(scope, &binding));

        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            // A failure here folds into the SAME error arm below rather than
            // returning early, so the `probe` branch keeps deciding what a
            // rejection means: `not_configured` is not `unmask_not_permitted`,
            // so `canUnmask()` still re-throws it instead of answering `false`.
            let outcome = match route {
                Ok(route) => match crate::tx_scope::bind_route(route).await {
                    Ok(route) => dispatch_unmask(&route, &binding, args).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            match outcome {
                Ok(result) => {
                    if probe {
                        // canUnmask: success → resolve with `true`.
                        OpResult::JsValue {
                            resolver,
                            value: ResolveValue::Bool(true),
                            request_id,
                        }
                    } else {
                        // The native method implements unmask(): Promise<T>;
                        // resolve with the declared plaintext type directly.
                        OpResult::JsValue {
                            resolver,
                            value: crate::v8_values::resolve(result.plaintext, binding),
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
                            zeroship_data_orm::error::DbError::Coded { code, .. }
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

        // DB-3: app JS must not be able to claim the reserved `auto` system
        // actor — strip it so an app handler cannot impersonate the platform.
        let sanitized = zeroship_data_orm::protection::unmask::sanitize_app_actor(
            opts_v.get("actor").cloned().filter(|v| !v.is_null()),
        );
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
            actor: sanitized.actor,
            reason,
            rejected_claim: sanitized.rejected_claim,
        };
        let binding = self.binding.clone();
        let row_pk = self.row_pk.clone();
        // Captured adapter-side, exactly as in
        // [`MaskedValue::dispatch_unmask_single`] above.
        let route = crate::startup_policy::require_finalized(scope)
            .and_then(|()| crate::tx_scope::capture_route(scope, &binding));

        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            // Bound adapter-side and folded into the error arm, exactly as
            // in [`MaskedValue::dispatch_unmask_single`] above.
            let outcome = match route {
                Ok(route) => match crate::tx_scope::bind_route(route).await {
                    Ok(route) => dispatch_bulk_unmask(&route, &binding, args).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            match outcome {
                Ok(result) => {
                    // Project to the per-column map for THIS row — the
                    // SDK's `MaskedValue.unmask(cols)` overload expects
                    // `Record<col, plaintext>`, not the wider
                    // `Record<row_pk, Record<col, plaintext>>` shape.
                    let cols_map = result.results.get(&row_pk).cloned().unwrap_or_default();
                    let mut payload = zeroship_data_orm::value::Map::new();
                    for (col, pt) in cols_map {
                        payload.insert(col, pt);
                    }
                    OpResult::JsValue {
                        resolver,
                        value: crate::v8_values::resolve(Value::Object(payload), binding),
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
/// six row-metadata fields. Called from `v8_values::encode` for each
/// masked cell of a database result, with the binding the result was read
/// through; nothing here reads a JavaScript object, so no creator code runs.
///
/// The minted object carries the full §4.1 internal-field set; no
/// fallible work happens after the Box is published, so a stray `?`
/// later would still drop the Box via the Weak finalizer.
pub(crate) fn mint_masked_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
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
    let prototype = original_prototype(scope, class_fn)?;
    obj.set_prototype(scope, prototype);

    let state = MaskedValue {
        binding,
        collection,
        row_pk,
        column,
        classification,
        masked_string,
    };
    MaskedValue::__zs_install(scope, obj, state)?;

    Some(obj)
}

/// The isolate's private key under which a context's `MaskedValue` function
/// keeps the prototype it was created with.
struct OriginalPrototypeKey(v8::Eternal<v8::Private>);

/// The prototype the context's `MaskedValue` function was created with.
///
/// Creator code can assign `MaskedValue.prototype` once it holds a minted value
/// (`value.constructor`), and an ordinary read of `prototype` would then hand
/// every later mint the creator's object. The class is not a global, so the
/// function is unreachable until this context's first mint; that mint reads the
/// pristine prototype and keeps it under a private key script cannot read or
/// write, and every later mint uses the kept one.
fn original_prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    class_fn: v8::Local<'s, v8::Function>,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = match scope.get_slot::<OriginalPrototypeKey>() {
        Some(slot) => slot.0.get(scope),
        None => None,
    };
    let key = match key {
        Some(key) => key,
        None => {
            let key = v8::Private::new(scope, None);
            let eternal = v8::Eternal::empty();
            eternal.set(scope, key);
            scope.set_slot(OriginalPrototypeKey(eternal));
            key
        }
    };
    if let Some(kept) = class_fn
        .get_private(scope, key)
        .filter(|kept| kept.is_object())
    {
        return Some(kept);
    }
    let prototype_key = v8::String::new(scope, "prototype")?;
    let prototype = class_fn.get(scope, prototype_key.into())?;
    class_fn.set_private(scope, key, prototype)?;
    Some(prototype)
}

#[cfg(test)]
mod tests {
    //! Brand / construction / serializer round-trip coverage for the
    //! MaskedValue v8_class. Network / SQL paths are exercised
    //! by the SDK suite and the SQLite integration target; these unit
    //! tests pin the V8-side invariants that don't need a backend.
    use super::*;

    use zeroship_runtime::init_v8;

    /// The native state of a `MaskedValue` wrapper.
    fn state_of<'a>(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> &'a MaskedValue {
        let object = v8::Local::<v8::Object>::try_from(value).unwrap();
        assert!(MaskedValue::is_instance(scope, object.into()));
        let field = object.get_internal_field(scope, 0).unwrap();
        let external = v8::Local::<v8::External>::try_from(field).unwrap();
        // The live branded wrapper owns this allocation until V8 finalizes it,
        // and the test isolate outlives every use of the reference.
        unsafe { &*external.value().cast::<MaskedValue>() }
    }

    /// One row of `people` read back through the ORM on a SQLite platform
    /// binding, so its `ssn` is the masked cell a real read produces.
    fn read_masked_row() -> Value {
        use zeroship_data_orm::orm::{Database, Output};
        const PLATFORM: &str = "platform";
        let directory = tempfile::tempdir().unwrap();
        crate::tests::fixtures::tables::create_sqlite_table(
            directory.path(),
            PLATFORM,
            &format!(
                r#"CREATE TABLE "{PLATFORM}".people (id TEXT PRIMARY KEY, ssn TEXT, __zs_raw__ssn TEXT);"#
            ),
        );
        let url = format!("sqlite:{}", directory.path().join("platform.sqlite").display());
        crate::tests::fixtures::parity::block_on(async move {
            let database = Database::connect(
                DbBinding::platform(
                    PLATFORM,
                    "platform-deploy",
                    zeroship_data_orm::sql::SchemaName::new(PLATFORM).unwrap(),
                ),
                zeroship_data_orm::ConnectOptions::new(
                    url,
                    zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
                ),
                zeroship_data_orm::schema::Schema::from_collections(vec![(
                    "people".into(),
                    zeroship_data_orm::value!({
                        "id": {"type": "string", "primaryKey": true, "required": true},
                        "ssn": {"type": "string",
                            "mask": {"kind": "last4", "classification": "spi"},
                            "storage": {"valueColumn": "ssn", "rawColumn": "__zs_raw__ssn"}}
                    }),
                )])
                .unwrap(),
            )
            .await
            .unwrap();
            let people = database.collection("people").unwrap();
            people
                .insert(zeroship_data_orm::value!({"id": "p1", "ssn": "123-45-6789"}))
                .await
                .unwrap();
            let Output::Rows(mut rows) = people
                .find(zeroship_data_orm::value!({}), zeroship_data_orm::value!({}))
                .await
                .unwrap()
            else {
                panic!("find returns rows");
            };
            assert_eq!(rows.len(), 1);
            rows.remove(0)
        })
    }

    /// A masked cell from a real read is minted against the binding the result
    /// carries rather than one any part of the row names.
    #[test]
    fn a_masked_cell_mints_against_the_binding_its_result_carries() {
        init_v8();
        let row = read_masked_row();
        assert!(row["ssn"].as_masked().is_some(), "{row:?}");
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, Default::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let key = v8::String::new(scope, "ssn").unwrap();
        for app in ["app_mint_first", "app_mint_second"] {
            let binding = crate::tests::fixtures::binding(app);
            let encoded = crate::v8_values::encode(scope, row.clone(), &binding).unwrap();
            let object = v8::Local::<v8::Object>::try_from(encoded).unwrap();
            let ssn = object.get(scope, key.into()).unwrap();
            let state = state_of(scope, ssn);
            assert_eq!(state.binding, binding);
            assert_eq!(
                (
                    state.collection.as_str(),
                    state.row_pk.as_str(),
                    state.column.as_str(),
                    state.classification.as_str(),
                    state.masked_string.as_str(),
                ),
                ("people", "p1", "ssn", "spi", "***-**-6789")
            );
        }
    }

    fn run_script(scope: &mut v8::PinScope, source: &str) -> String {
        let source = v8::String::new(scope, source).unwrap();
        let script = v8::Script::compile(scope, source, None).unwrap();
        script.run(scope).unwrap().to_rust_string_lossy(scope)
    }

    fn mint_global(scope: &mut v8::PinScope, name: &str, row_pk: &str) {
        let minted = mint_masked_value(
            scope,
            crate::tests::fixtures::binding("app_a"),
            "users".into(),
            row_pk.into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .expect("mint should succeed");
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), minted.into());
    }

    /// Creator code reaches the constructor through a minted value and can
    /// assign its `prototype`. Later mints keep the class's own prototype, so
    /// they stay native masked values with the native surface.
    #[test]
    fn reassigning_the_constructor_prototype_leaves_later_mints_native() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        mint_global(scope, "first", "usr_01");
        // The assignment takes effect, which is the control for what follows.
        assert_eq!(
            run_script(
                scope,
                "first.constructor.prototype = { evil: true }; \
                 String(first.constructor.prototype.evil === true)"
            ),
            "true"
        );
        mint_global(scope, "second", "usr_02");
        assert_eq!(
            run_script(
                scope,
                "JSON.stringify({ tag: Object.prototype.toString.call(second), \
                 unmask: typeof second.unmask, evil: 'evil' in second, \
                 shared: Object.getPrototypeOf(second) === Object.getPrototypeOf(first), \
                 pk: second._meta?.row_pk ?? null })"
            ),
            r#"{"tag":"[object MaskedValue]","unmask":"function","evil":false,"shared":true,"pk":"usr_02"}"#
        );
    }

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
            crate::tests::fixtures::binding("app_a"),
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
            crate::tests::fixtures::binding("app_a"),
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
            crate::tests::fixtures::binding("app_a"),
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
            crate::tests::fixtures::binding("app_a"),
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
        let func =
            v8::Local::<v8::Function>::try_from(fn_v).expect("toString should be a Function");
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
            crate::tests::fixtures::binding("app_a"),
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
}
