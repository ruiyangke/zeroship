//! `Migrations` — `#[v8_class]` namespace backing `env.db.migrations`.
//!
//! Every method takes a `{ name, collection, ... }` spec object:
//!
//! - `.start(spec)` → mints a [`super::migration::Migration`] wrapper
//!   once the advisory lock + audit row are claimed. The wrapper drives
//!   `.fetchBatch()` / `.commitBatch()` for the run.
//! - `.status(spec)` → reads the audit-row state for a `(collection,
//!   name)` pair without touching the advisory lock. Safe to call
//!   from any worker.
//! - `.cancel(spec)` → transitions the audit row to `cancelled`. The
//!   active-runner thread observes the cancellation on its next
//!   `fetchBatch`.
//! - `.reset(spec)` → resets the audit row to `pending` (cursor → 0,
//!   dead-letter-pks → null). Use after a `cancelled` or `failed`
//!   terminal status to re-run from the start.
//!
//! All four methods accept the same `{ name, collection }` shape;
//! `.start` additionally honours `{ dryRun?, reset? }`.

#![allow(unsafe_code)]

use std::cell::RefCell;

use serde_json::Value;

use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

use crate::callbacks;

// ---------------------------------------------------------------------------
// Migrations state
// ---------------------------------------------------------------------------

/// Owned state for the `env.db.migrations` v8_class instance.
/// `app_id` is captured at mint time from the parent `Db` wrapper so
/// the method bodies don't have to re-read it per call.
pub struct Migrations {
    pub(crate) app_id: RefCell<String>,
}

impl std::fmt::Debug for Migrations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Migrations")
            .field("app_id", &self.app_id.borrow())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Migrations IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Migrations {
    #[v8_constructor]
    fn new() -> Migrations {
        Migrations {
            app_id: RefCell::new(String::new()),
        }
    }

    /// `env.db.migrations.start(spec)` → Promise<Migration>
    ///
    /// `spec` is `{ name, collection, dryRun?, reset? }`. Acquires the
    /// advisory lock + writes the audit row, then resolves with a
    /// `Migration` v8_class instance whose `.fetchBatch()` /
    /// `.commitBatch()` / `.status()` / `.cancel()` / `.reset()` drive
    /// the run. On error (lock already held, malformed spec, etc.)
    /// rejects with a structured envelope.
    #[v8_method]
    fn start<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        spec: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        super::migration::migration_start_with_spec(scope, spec).into()
    }

    /// `env.db.migrations.status(spec)` → Promise<status JSON>
    #[v8_method]
    fn status<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        spec: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id = self.app_id.borrow().clone();
        dispatch_by_spec(scope, app_id, spec, MigrationOp::Status).into()
    }

    /// `env.db.migrations.cancel(spec)` → Promise<{ok} JSON>
    #[v8_method]
    fn cancel<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        spec: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id = self.app_id.borrow().clone();
        dispatch_by_spec(scope, app_id, spec, MigrationOp::Cancel).into()
    }

    /// `env.db.migrations.reset(spec)` → Promise<{ok:true} JSON>
    #[v8_method]
    fn reset<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        spec: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id = self.app_id.borrow().clone();
        dispatch_by_spec(scope, app_id, spec, MigrationOp::Reset).into()
    }
}

// ---------------------------------------------------------------------------
// Shared dispatch for status / cancel / reset
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum MigrationOp {
    Status,
    Cancel,
    Reset,
}

/// Extract `(name, collection)` from a `{ name, collection }` spec
/// object, then run the matching `migrations::exec_*` against the
/// pool. Shared by `status` / `cancel` / `reset` because all three
/// take the same input shape; resolution differs by op (typed object
/// for status, void for cancel/reset).
fn dispatch_by_spec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
    spec: v8::Local<v8::Value>,
    op: MigrationOp,
) -> v8::Local<'s, v8::Promise> {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    // Parse the spec synchronously so we can reject early with a clear
    // TypeError instead of spawning an async failure path.
    let (name, collection) = match parse_name_and_collection(scope, spec) {
        Ok(pair) => pair,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let resolver_global = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match callbacks::ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(OpError::error(e)),
                    request_id,
                };
            }
        };
        let result = match op {
            MigrationOp::Status => {
                crate::migrations::exec_status(&pool, &app_id, &name, &collection).await
            }
            MigrationOp::Cancel => {
                crate::migrations::exec_cancel(&pool, &app_id, &name, &collection).await
            }
            MigrationOp::Reset => {
                crate::migrations::exec_reset(&pool, &app_id, &name, &collection).await
            }
        };
        let value = match result {
            Ok(json) => match op {
                MigrationOp::Status => ResolveValue::Json(json),
                MigrationOp::Cancel | MigrationOp::Reset => ResolveValue::Undefined,
            },
            Err(e) => ResolveValue::RejectError(e),
        };
        OpResult::JsValue { resolver: resolver_global, value, request_id }
    }));

    promise
}

fn parse_name_and_collection(
    scope: &mut v8::PinScope<'_, '_>,
    spec: v8::Local<v8::Value>,
) -> Result<(String, String), String> {
    if !spec.is_object() {
        return Err("migrations: spec must be an object".into());
    }
    let parsed = callbacks::v8_value_to_serde_json(scope, spec);
    let obj = parsed
        .as_object()
        .ok_or_else(|| "migrations: spec must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "migrations: spec.name must be a non-empty string".to_string())?
        .to_string();
    let collection = obj
        .get("collection")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "migrations: spec.collection must be a non-empty string".to_string())?
        .to_string();
    Ok((name, collection))
}

// ---------------------------------------------------------------------------
// mint_migrations — build the wrapper attached to `Db.migrations`
// ---------------------------------------------------------------------------

/// Mint a `Migrations` v8_class instance with `app_id` stamped from
/// the parent `Db`. Cached on the Db wrapper so
/// `env.db.migrations === env.db.migrations` holds.
pub fn mint_migrations<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let class_tmpl = Migrations::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Migrations instance allocation failed"))?;

    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Migrations template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Migrations prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Migrations {
        app_id: RefCell::new(app_id.to_string()),
    };
    let boxed: Box<Migrations> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Migrations));
        }),
    );
    std::mem::forget(weak);

    Ok(obj)
}
