//! `Replication` — `#[v8_class]` namespace backing `env.db.replication`.
//!
//! Three methods scoped to the calling app, mirroring the legacy flat
//! callbacks `replicationSetup` / `replicationWatchdog` /
//! `replicationDropAbandoned`. The app scope is always the `app_id`
//! stamped on the wrapper at mint time — it cannot be overridden from
//! JS. The v8_class runs inside the tenant isolate; there is no
//! reliable operator-vs-tenant distinction at this layer, so an
//! `opts.appId` override would be a cross-app hijack vector (App A
//! provisioning slots/publications for victim App B). Operator-shaped
//! provisioning belongs in the control plane, not here.

#![allow(unsafe_code)]

use serde_json::Value;
use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name};

use crate::replication_ops::{
    replication_drop_abandoned_dispatch, replication_setup_dispatch, replication_watchdog_dispatch,
};
use crate::v8_bridge::read_json_arg;

pub struct Replication {
    /// app_id stamped at mint time from the parent Db wrapper. Never
    /// mutated; plain `String`, no `RefCell`.
    pub(crate) app_id: String,
}

impl std::fmt::Debug for Replication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replication")
            .field("app_id", &self.app_id)
            .finish()
    }
}

#[v8_class]
#[allow(dead_code)]
impl Replication {
    #[v8_constructor]
    fn new() -> Result<Replication, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `db.replication.setup(opts?)` → `Promise<SetupOutcome JSON>`.
    /// Provisions the per-app publication + logical replication slot
    /// for the calling app. The app scope is always `self.app_id` (the
    /// id stamped on the wrapper at mint time, sourced from the
    /// isolate's `APP_ID` env var). `opts` is reserved for forward
    /// compatibility — any `appId` field is intentionally ignored to
    /// prevent cross-app provisioning from tenant JS.
    #[v8_method]
    fn setup<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        // Parse `opts` so any future fields can be plumbed through, but
        // route through `resolve_setup_app_id` which deliberately
        // discards any caller-supplied `appId` (see module docs for the
        // cross-app hijack rationale, and the `setup_app_id_*` unit
        // tests for the regression guard).
        let opts_v = read_json_arg(scope, Some(opts));
        let app_id = resolve_setup_app_id(&self.app_id, &opts_v);
        replication_setup_dispatch(scope, app_id).into()
    }

    /// `db.replication.watchdog()` → `Promise<SlotHealth[] JSON>`.
    #[v8_method]
    fn watchdog<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        replication_watchdog_dispatch(scope).into()
    }

    /// `db.replication.dropAbandoned(opts?)` → `Promise<string[] JSON>`.
    /// `opts.inactiveSeconds` (default 3600) is the threshold; returns
    /// the names of dropped slots.
    #[v8_method]
    #[v8_name = "dropAbandoned"]
    fn drop_abandoned<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let opts_v = read_json_arg(scope, Some(opts));
        let inactive_seconds = opts_v
            .get("inactiveSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        replication_drop_abandoned_dispatch(scope, inactive_seconds).into()
    }
}

/// Resolve the app_id used by `Replication::setup` for dispatch.
///
/// Returns `stamped` verbatim, ignoring any `appId` field in the
/// JS-supplied `opts` object. Lifted out of the `#[v8_method]` body
/// (a) so the security-critical resolution policy is unit-testable
/// without V8 plumbing and (b) so a future contributor restoring
/// caller-controlled overrides has to delete this helper (and its
/// tests) — making the regression visible in review.
#[inline]
fn resolve_setup_app_id(stamped: &str, _opts: &Value) -> String {
    // INVARIANT: never read app-id-shaped fields from `_opts`. The
    // v8_class executes inside the tenant isolate; any caller-supplied
    // override is a cross-app hijack vector. See module docs.
    stamped.to_string()
}

/// Mint a `Replication` v8_class instance with `app_id` stamped from
/// the parent `Db`. Cached on the Db wrapper so identity holds across
/// reads.
pub fn mint_replication<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let class_tmpl = Replication::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Replication instance allocation failed"))?;

    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Replication template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Replication prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Replication {
        app_id: app_id.to_string(),
    };
    let boxed: Box<Replication> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Replication));
        }),
    );
    std::mem::forget(weak);

    Ok(obj)
}

#[cfg(test)]
mod tests {
    //! Regression guards for the cross-app replication hijack fix
    //! (security review r2, 2026-05-22). Prior to the fix,
    //! `Replication::setup` honoured an `opts.appId` override from JS,
    //! letting App A provision a publication/slot for any victim app
    //! on the same worker. The fix routes app-id resolution through
    //! [`super::resolve_setup_app_id`], which always returns the
    //! mint-time `self.app_id`.

    use super::resolve_setup_app_id;
    use serde_json::json;

    #[test]
    fn setup_app_id_ignores_string_override() {
        // App A's wrapper is stamped with "app_a". A malicious JS
        // caller supplies `{appId: "victim_app"}`. The dispatch MUST
        // still target "app_a".
        let opts = json!({"appId": "victim_app"});
        assert_eq!(resolve_setup_app_id("app_a", &opts), "app_a");
    }

    #[test]
    fn setup_app_id_ignores_non_string_override() {
        // Defence in depth: numbers, booleans, nested objects, arrays,
        // null — none of these should ever produce a different app_id
        // from the stamped one.
        for shape in [
            json!({"appId": 123}),
            json!({"appId": true}),
            json!({"appId": null}),
            json!({"appId": ["app_b"]}),
            json!({"appId": {"name": "app_b"}}),
        ] {
            assert_eq!(
                resolve_setup_app_id("app_a", &shape),
                "app_a",
                "override shape leaked through: {shape}"
            );
        }
    }

    #[test]
    fn setup_app_id_empty_opts_uses_stamped() {
        // Common case: no opts supplied at all.
        assert_eq!(resolve_setup_app_id("app_a", &json!({})), "app_a");
        assert_eq!(resolve_setup_app_id("app_a", &json!(null)), "app_a");
    }

    #[test]
    fn setup_app_id_preserves_unicode_stamped_id() {
        // Stamped id is taken verbatim — no normalisation, no
        // sanitisation at this layer. Callers above already vetted it.
        let stamped = "app_测试_🛡";
        assert_eq!(resolve_setup_app_id(stamped, &json!({"appId": "x"})), stamped);
    }
}
