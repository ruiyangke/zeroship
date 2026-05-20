//! `Replication` — `#[v8_class]` namespace backing `env.db.replication`.
//!
//! Three operator-only methods, mirroring the legacy flat callbacks
//! `replicationSetup` / `replicationWatchdog` /
//! `replicationDropAbandoned`. Apps don't call these directly; the
//! deploy orchestrator / control plane does.

#![allow(unsafe_code)]

use serde_json::Value;
use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name};

use crate::callbacks;

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
    /// Provisions the per-app publication + logical replication slot.
    /// `opts.appId` overrides the current app context (operator path).
    #[v8_method]
    fn setup<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        let app_id = opts_v
            .get("appId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.app_id.clone());
        callbacks::replication_setup_dispatch(scope, app_id).into()
    }

    /// `db.replication.watchdog()` → `Promise<SlotHealth[] JSON>`.
    #[v8_method]
    fn watchdog<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        callbacks::replication_watchdog_dispatch(scope).into()
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
        let opts_v = callbacks::read_json_arg(scope, Some(opts));
        let inactive_seconds = opts_v
            .get("inactiveSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        callbacks::replication_drop_abandoned_dispatch(scope, inactive_seconds).into()
    }
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
