//! `Kv` — the `#[v8_class]` instance backing `env.kv`.
//!
//! `KvPlugin::build_instance` mints a `Kv` via [`mint_kv`] once per V8
//! isolate during `build_env_object`; the returned object becomes the
//! `env.kv` namespace value. Mirrors `plugin-db`'s `Db` v8_class.
//!
//! Each `#[v8_method]` first validates its arguments **synchronously**
//! (key shape, value type + size, option ranges) — a failure returns
//! `Err(OpError)`, which the macro throws as a JS `TypeError` before any
//! dispatch. It then calls a `crate::dispatch::dispatch_*` helper that
//! spawns the async backend op and returns the `Promise`.
//!
//! ## Why a v8_class
//!
//! The instance carries per-isolate state — the `Arc<dyn Backend>` and
//! the `app_id` stamped at mint time — so callbacks never read the
//! runtime slot for the backend (the legacy `thread_local! KV_BACKEND`
//! is gone) and never re-derive the app_id per call.

#![allow(unsafe_code)]

use std::sync::Arc;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_method, v8_name};

use crate::backend::Backend;
use crate::dispatch::{
    dispatch_delete, dispatch_expire, dispatch_get, dispatch_incr, dispatch_list,
    dispatch_persist, dispatch_set, dispatch_set_if_absent, dispatch_ttl,
};
use crate::limits::{
    resolve_list_limit, validate_delta, validate_key, validate_ttl_ms, validate_value,
};

// ---------------------------------------------------------------------------
// Kv state
// ---------------------------------------------------------------------------

/// Owned state for the `env.kv` v8_class instance. Field 0 of the
/// wrapper holds a `Box<Kv>`; the Weak finalizer registered by
/// [`mint_kv`] drops the Box on GC. The `Arc<dyn Backend>` clone is
/// released with the Box (no other native resource to free).
pub struct Kv {
    /// The active backend (redb / Redis). Cloned per dispatch
    /// into the spawned op future.
    pub(crate) backend: Arc<dyn Backend>,
    /// The app_id this Kv belongs to, stamped at mint time from
    /// `SharedState.env_vars["APP_ID"]`. Never mutated.
    pub(crate) app_id: String,
    /// Per-app metering handle (bound to `app_id` at mint time). `Some` on
    /// the worker / dev-serve vectors; each successful kv op emits
    /// `kv_reads` / `kv_writes` through it. `None` in meter-less test
    /// harnesses. Cloned per dispatch into the spawned op future so the
    /// emit happens in the op's Ok arm, after the backend returns.
    pub(crate) meter: Option<zeroship_metering::MeterHandle>,
}

impl std::fmt::Debug for Kv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kv").field("app_id", &self.app_id).finish()
    }
}

// ---------------------------------------------------------------------------
// Argument helpers (sync, in-body — the macro has no u64/options extractor)
// ---------------------------------------------------------------------------

/// Cheap JS-side type label for error messages.
fn js_type_name(v: v8::Local<v8::Value>) -> &'static str {
    if v.is_string() { "string" }
    else if v.is_number() { "number" }
    else if v.is_boolean() { "boolean" }
    else if v.is_function() { "function" }
    else if v.is_array() { "array" }
    else if v.is_null() { "null" }
    else if v.is_undefined() { "undefined" }
    else { "object" }
}

/// Extract the `value` argument as a string. The value MUST be a JS
/// string: `null`/`undefined` throw (a value is required), and any
/// non-string (number, object, array, boolean) throws too — coercing
/// them via `to_rust_string_lossy` would store garbage like
/// `"[object Object]"`, contradicting the `InvalidValue` "value not a
/// string" contract and breaking the SDK's `get` (whose `JSON.parse`
/// would throw on the round-trip). The empty string IS accepted. The SDK
/// always hands us `JSON.stringify(...)` (a string), so it's unaffected.
fn extract_value(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Result<String, OpError> {
    if value.is_null_or_undefined() {
        return Err(OpError::type_error(
            "kv: value must be provided (got null/undefined)",
        ));
    }
    if !value.is_string() {
        return Err(OpError::type_error(format!(
            "kv: value must be a string, got {}",
            js_type_name(value)
        )));
    }
    Ok(value.to_rust_string_lossy(scope))
}

/// Read an optional numeric field off an options object as `f64`.
/// Returns `Ok(None)` when `opts` is null/undefined or the field is
/// absent/null/undefined; `Err` when `opts` is a non-object or the
/// field is present but not a number.
fn opt_number(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
    field: &str,
) -> Result<Option<f64>, OpError> {
    if opts.is_null_or_undefined() {
        return Ok(None);
    }
    let obj = v8::Local::<v8::Object>::try_from(opts).map_err(|_| {
        OpError::type_error(format!("kv: options must be an object, got {}", js_type_name(opts)))
    })?;
    let key = v8::String::new(scope, field).unwrap();
    let Some(v) = obj.get(scope, key.into()) else {
        return Ok(None);
    };
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(OpError::type_error(format!(
            "kv: options.{field} must be a number, got {}",
            js_type_name(v)
        )));
    }
    Ok(v.number_value(scope))
}

/// Read an optional string field off an options object. Returns
/// `Ok(None)` when absent/null/undefined; `Err` when `opts` is a
/// non-object or the field is present but not a string.
fn opt_string(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
    field: &str,
) -> Result<Option<String>, OpError> {
    if opts.is_null_or_undefined() {
        return Ok(None);
    }
    let obj = v8::Local::<v8::Object>::try_from(opts).map_err(|_| {
        OpError::type_error(format!("kv: options must be an object, got {}", js_type_name(opts)))
    })?;
    let key = v8::String::new(scope, field).unwrap();
    let Some(v) = obj.get(scope, key.into()) else {
        return Ok(None);
    };
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    if !v.is_string() {
        return Err(OpError::type_error(format!(
            "kv: options.{field} must be a string, got {}",
            js_type_name(v)
        )));
    }
    Ok(Some(v.to_rust_string_lossy(scope)))
}

/// Resolve an optional `ttlMs` option into validated milliseconds.
fn read_ttl_ms(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<Option<u64>, OpError> {
    match opt_number(scope, opts, "ttlMs")? {
        Some(n) => Ok(Some(validate_ttl_ms(n).map_err(crate::error::KvError::to_op_error)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Kv IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Kv {
    /// `new Kv()` from JS rejects — real instances are minted via
    /// [`mint_kv`] from `KvPlugin::build_instance`, which stamps the
    /// live backend + app_id onto the wrapper. A user-constructed Kv
    /// would have no backend and every method would panic.
    #[v8_constructor]
    fn new() -> Result<Kv, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `kv.get(key)` → Promise<string | null>.
    #[v8_method]
    fn get<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        Ok(dispatch_get(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key).into())
    }

    /// `kv.set(key, value, {ttlMs?})` → Promise<{ ok: true }>.
    #[v8_method]
    fn set<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
        value: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        let value = extract_value(scope, value)?;
        validate_value(&value).map_err(crate::error::KvError::to_op_error)?;
        let ttl_ms = read_ttl_ms(scope, opts)?;
        Ok(dispatch_set(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key, value, ttl_ms)
            .into())
    }

    /// `kv.delete(key)` → Promise<{ deleted: boolean }>.
    #[v8_method]
    fn delete<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        Ok(dispatch_delete(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key).into())
    }

    /// `kv.incr(key, {by?=1, ttlMs?})` → Promise<number> (BigInt if
    /// `|n|>2^53`). `ttlMs` sets the expiry only when the key is created
    /// this call (fixed-window rate limit).
    #[v8_method]
    fn incr<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        let delta = match opt_number(scope, opts, "by")? {
            Some(n) => validate_delta(n).map_err(crate::error::KvError::to_op_error)?,
            None => 1,
        };
        let ttl_ms = read_ttl_ms(scope, opts)?;
        Ok(dispatch_incr(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key, delta, ttl_ms)
            .into())
    }

    /// `kv.setIfAbsent(key, value, {ttlMs?})` → Promise<{ stored: boolean }>.
    #[v8_method]
    #[v8_name = "setIfAbsent"]
    fn set_if_absent<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
        value: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        let value = extract_value(scope, value)?;
        validate_value(&value).map_err(crate::error::KvError::to_op_error)?;
        let ttl_ms = read_ttl_ms(scope, opts)?;
        Ok(dispatch_set_if_absent(
            scope,
            Arc::clone(&self.backend),
            self.app_id.clone(),
            self.meter.clone(),
            key,
            value,
            ttl_ms,
        )
        .into())
    }

    /// `kv.expire(key, ttlMs)` → Promise<{ updated: boolean }>.
    #[v8_method]
    fn expire<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
        ttl_ms: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        if !ttl_ms.is_number() {
            return Err(OpError::type_error(format!(
                "kv.expire: ttlMs must be a number, got {}",
                js_type_name(ttl_ms)
            )));
        }
        let ms = ttl_ms
            .number_value(scope)
            .ok_or_else(|| OpError::type_error("kv.expire: ttlMs must be a number"))?;
        let ms = validate_ttl_ms(ms).map_err(crate::error::KvError::to_op_error)?;
        Ok(dispatch_expire(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key, ms).into())
    }

    /// `kv.ttl(key)` → Promise<{ ttlMs: number | null }> for an existing
    /// key, or `null` for a missing key.
    #[v8_method]
    fn ttl<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        Ok(dispatch_ttl(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key).into())
    }

    /// `kv.persist(key)` → Promise<{ updated: boolean }> (removes TTL).
    #[v8_method]
    fn persist<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        key: String,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        validate_key(&key).map_err(crate::error::KvError::to_op_error)?;
        Ok(dispatch_persist(scope, Arc::clone(&self.backend), self.app_id.clone(), self.meter.clone(), key).into())
    }

    /// `kv.list(prefix?, {cursor?, limit?})` → Promise<{ keys: string[],
    /// cursor: string | null }>. `prefix` is a literal (no glob);
    /// `cursor` is opaque + backend-specific; `cursor: null` resolves =
    /// end of listing.
    #[v8_method]
    fn list<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        prefix: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // prefix defaults to "" (list everything for the app).
        let prefix = if prefix.is_null_or_undefined() {
            String::new()
        } else if prefix.is_string() {
            prefix.to_rust_string_lossy(scope)
        } else {
            return Err(OpError::type_error(format!(
                "kv.list: prefix must be a string, got {}",
                js_type_name(prefix)
            )));
        };
        let cursor = opt_string(scope, opts, "cursor")?;
        let limit = resolve_list_limit(opt_number(scope, opts, "limit")?);
        Ok(dispatch_list(
            scope,
            Arc::clone(&self.backend),
            self.app_id.clone(),
            self.meter.clone(),
            prefix,
            cursor,
            limit,
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// mint_kv — build a `Kv` wrapper for a given backend + app_id
// ---------------------------------------------------------------------------

/// Mint a `Kv` v8_class instance with state stamped from `backend` +
/// `app_id`. Called from `KvPlugin::build_instance` once per V8 isolate
/// during `build_env_object`. The returned object becomes the `env.kv`
/// namespace value. Mirrors `plugin-db`'s `mint_db`.
pub fn mint_kv<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: &str,
    meter: Option<zeroship_metering::MeterHandle>,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = Kv::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = Kv { backend, app_id: app_id.to_string(), meter };
    let boxed: Box<Kv> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Kv>`; the
    // finalizer casts back to the same type and drops the Box exactly
    // once when V8 reclaims the wrapper. The only native resource is the
    // `Arc<dyn Backend>` clone, released with the Box.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Kv));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}
