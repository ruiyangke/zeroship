//! `Replication` — `#[v8_class]` namespace backing `env.db.replication`.
//!
//! One diagnostic method scoped to the calling app. Provisioning is owned by
//! Subscription.ready(), so this namespace cannot create an unowned slot.
//! The app scope is always the `app_id`
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

use zeroship_runtime::state::ResolveValue;


use crate::v8_bridge::{read_json_arg, runtime_state, setup_js_promise};
use crate::v8_classes::dispatch::settle;

/// `db.replication.watchdog()` dispatch.
///
/// Lived in a 48-line `replication_ops.rs` of its own until 2026-09-02, with
/// this as its only item and the method below as its only caller. That file
/// matched no arm of the tier census's map, so it fell to `CONTESTED` - a
/// bucket the census exempts from every rule, on the grounds that a module with
/// no assigned destination cannot violate one. The effect was that a live
/// `v8::PinScope` dispatch sat in the ENGINE-shaped half of the crate and the
/// instrument reported nothing. Being here makes it ADAPTER by path.
///
/// The body was also a hand-rolled `settle`: three arms building
/// `OpResult::JsValue` by hand, two of them byte-identical to `reject_op`.
fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        async move { crate::exec::replication_watchdog(&app_id).await },
        |rows| ResolveValue::String(crate::replication::watchdog_to_json(&rows)),
    )));

    promise
}

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
impl Replication {
    #[v8_constructor]
    fn new() -> Result<Replication, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `db.replication.watchdog()` → `Promise<SlotHealth[] JSON>`.
    /// Scoped to `self.app_id`; the underlying SQL filters
    /// `pg_replication_slots` by the per-app slot prefix so a tenant
    /// can never enumerate co-tenant slot names (cross-tenant info
    /// disclosure — sibling of the cross-app `setup` hijack closed
    /// at 309ed52f).
    #[v8_method]
    fn watchdog<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        // Parse `opts` defensively even though we don't read any field
        // today: this keeps `resolve_watchdog_app_id` unit-testable for
        // the "ignore any caller-supplied override" invariant and
        // mirrors the `setup` pattern.
        let opts_v = match read_json_arg(scope, Some(opts)) {
            Ok(v) => v,
            Err(e) => return crate::v8_bridge::throw_decode_error(scope, &e),
        };
        let app_id = resolve_watchdog_app_id(&self.app_id, &opts_v);
        replication_watchdog_dispatch(scope, app_id).into()
    }

}

/// Resolve the app_id used by `Replication::watchdog` for dispatch.
///
/// Returns `stamped` verbatim, ignoring any `appId` field in the
/// JS-supplied `opts` object. This security-critical resolution policy stays
/// unit-testable so a caller-controlled override is visible in review.
///
/// Sibling of the cross-app `setup` hijack closed at 309ed52f: prior
/// to this fix, `watchdog()` issued a cluster-wide
/// `pg_replication_slots` enumeration, letting App A discover every
/// co-tenant app's slot names (info disclosure).
#[inline]
fn resolve_watchdog_app_id(stamped: &str, _opts: &Value) -> String {
    // INVARIANT: never read app-id-shaped fields from `_opts`.
    stamped.to_string()
}

/// Mint a `Replication` v8_class instance with `app_id` stamped from
/// the parent `Db`. Cached on the Db wrapper so identity holds across
/// reads.
pub(crate) fn mint_replication<'s>(
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
    //! Regression guards for app-scoped replication diagnostics.

    use super::resolve_watchdog_app_id;
    use serde_json::json;

    // -----------------------------------------------------------------
    // Sibling regression guards for the CRITICAL cross-tenant scoping
    // gap on `watchdog` (security review r5, 2026-05-22). Before the fix,
    // it called the underlying dispatcher without any app_id parameter and
    // the SQL ran cluster-wide, exposing co-tenant slot names.
    //
    // The resolver helpers must always return the mint-time stamp, never reading
    // `opts.appId` regardless of shape.
    // -----------------------------------------------------------------

    #[test]
    fn watchdog_app_id_ignores_string_override() {
        let opts = json!({"appId": "victim_app"});
        assert_eq!(resolve_watchdog_app_id("app_a", &opts), "app_a");
    }

    #[test]
    fn watchdog_app_id_ignores_non_string_override() {
        for shape in [
            json!({"appId": 123}),
            json!({"appId": true}),
            json!({"appId": null}),
            json!({"appId": ["app_b"]}),
            json!({"appId": {"name": "app_b"}}),
        ] {
            assert_eq!(
                resolve_watchdog_app_id("app_a", &shape),
                "app_a",
                "override shape leaked through watchdog: {shape}"
            );
        }
    }

    #[test]
    fn watchdog_app_id_empty_opts_uses_stamped() {
        assert_eq!(resolve_watchdog_app_id("app_a", &json!({})), "app_a");
        assert_eq!(resolve_watchdog_app_id("app_a", &json!(null)), "app_a");
    }

}
