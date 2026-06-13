//! `Meter` v8_class — the object backing `env.meter`.
//!
//! `MeterPlugin::build_instance` mints a `MeterHandle` via [`mint_meter`]
//! once per V8 isolate; the returned object becomes the `env.meter`
//! namespace value. Mirrors `plugin-kv`'s `Kv` v8_class, but every method
//! is SYNCHRONOUS: an increment is a lock-free atomic bump on the shared
//! per-worker [`Meter`], not an async backend op, so there is no Promise,
//! no `spawned_ops`, no dispatch module. The method returns the metric's
//! new running total directly.
//!
//! The instance carries the shared `Arc<Meter>` and the isolate's
//! `app_id` (stamped at mint time from `SharedState.env_vars["APP_ID"]`),
//! so a callback never re-derives the app per call and the platform's
//! per-app scoping is structural — user code cannot meter another app.

#![allow(unsafe_code)]

use std::sync::Arc;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_method, v8_name};

use crate::meter::Meter;

/// Largest count an increment may apply in one call. A single
/// `env.meter.increment(metric, n)` can't add more than this — a defense
/// against an app inflating its own bill via one absurd call, and it keeps
/// the value inside the `f64`-exact integer range so the JS return is
/// lossless.
const MAX_INCREMENT: f64 = 9_007_199_254_740_991.0; // Number.MAX_SAFE_INTEGER

/// Max metric-name length. SDK metric names are short identifiers; a cap
/// bounds the `custom` map key size.
const MAX_METRIC_LEN: usize = 128;

/// Owned state for the `env.meter` v8_class instance. Field 0 of the
/// wrapper holds a `Box<MeterHandle>`; the Weak finalizer drops the Box on
/// GC, releasing the `Arc<Meter>` clone.
pub struct MeterHandle {
    pub(crate) meter: Arc<Meter>,
    pub(crate) app_id: String,
}

impl std::fmt::Debug for MeterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterHandle").field("app_id", &self.app_id).finish()
    }
}

/// Cheap JS-side type label for error messages.
fn js_type_name(v: v8::Local<v8::Value>) -> &'static str {
    if v.is_string() {
        "string"
    } else if v.is_number() {
        "number"
    } else if v.is_boolean() {
        "boolean"
    } else if v.is_function() {
        "function"
    } else if v.is_array() {
        "array"
    } else if v.is_null() {
        "null"
    } else if v.is_undefined() {
        "undefined"
    } else {
        "object"
    }
}

/// Validate a metric name: a non-empty, reasonably-short string. Names are
/// otherwise unrestricted (an SDK may use `db.reads`, `emails.sent`, etc.).
fn validate_metric(name: &str) -> Result<(), OpError> {
    if name.is_empty() {
        return Err(OpError::type_error("meter: metric must be a non-empty string"));
    }
    if name.len() > MAX_METRIC_LEN {
        return Err(OpError::range_error(format!(
            "meter: metric name exceeds {MAX_METRIC_LEN} bytes"
        )));
    }
    Ok(())
}

#[v8_class]
#[allow(dead_code)]
impl MeterHandle {
    /// `new env.meter.constructor()` rejects — real instances are minted
    /// via [`mint_meter`] from `MeterPlugin::build_instance`, which stamps
    /// the shared `Arc<Meter>` + app_id. A user-constructed handle would
    /// have no meter and every method would panic.
    #[v8_constructor]
    fn new() -> Result<MeterHandle, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `meter.increment(metric, n?=1)` → number (the metric's new running
    /// total this period). Synchronous — bumps the shared per-worker meter
    /// for THIS isolate's app and returns immediately. `n` defaults to 1;
    /// it must be a finite, non-negative integer ≤ `Number.MAX_SAFE_INTEGER`.
    #[v8_method]
    fn increment(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        metric: String,
        n: v8::Local<v8::Value>,
    ) -> Result<f64, OpError> {
        validate_metric(&metric)?;
        let count = read_count(scope, n)?;
        // `count` is a validated non-negative integer ≤ 2^53, so the cast
        // is exact; the return total stays inside the f64-exact range until
        // it would itself exceed 2^53 (period totals reset every ~month, so
        // this is not reachable in practice).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let new_total = self.meter.increment(&self.app_id, &metric, count as u64);
        #[allow(clippy::cast_precision_loss)]
        Ok(new_total as f64)
    }
}

/// Read the optional count argument. Absent / null / undefined → 1. Must be
/// a finite, non-negative integer within `[0, MAX_INCREMENT]`.
fn read_count(scope: &mut v8::PinScope<'_, '_>, n: v8::Local<v8::Value>) -> Result<f64, OpError> {
    if n.is_null_or_undefined() {
        return Ok(1.0);
    }
    if !n.is_number() {
        return Err(OpError::type_error(format!(
            "meter.increment: count must be a number, got {}",
            js_type_name(n)
        )));
    }
    let v = n
        .number_value(scope)
        .ok_or_else(|| OpError::type_error("meter.increment: count must be a number"))?;
    if !v.is_finite() {
        return Err(OpError::range_error("meter.increment: count must be a finite number"));
    }
    if v.fract() != 0.0 {
        return Err(OpError::range_error("meter.increment: count must be an integer"));
    }
    if v < 0.0 {
        return Err(OpError::range_error("meter.increment: count must not be negative"));
    }
    if v > MAX_INCREMENT {
        return Err(OpError::range_error(
            "meter.increment: count exceeds the maximum (2^53 - 1)",
        ));
    }
    Ok(v)
}

/// Mint a `MeterHandle` v8_class instance stamped with the shared `Arc<Meter>`
/// + `app_id`. Called from `MeterPlugin::build_instance` once per V8 isolate
/// during `build_env_object`. Mirrors `plugin-kv`'s `mint_kv`.
pub fn mint_meter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    meter: Arc<Meter>,
    app_id: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = MeterHandle::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = MeterHandle { meter, app_id: app_id.to_string() };
    let boxed: Box<MeterHandle> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<MeterHandle>`; the
    // finalizer casts back to the same type and drops the Box exactly once
    // when V8 reclaims the wrapper. The only native resource is the
    // `Arc<Meter>` clone, released with the Box.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MeterHandle));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}
