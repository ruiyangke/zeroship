//! V8 binding for key-value storage — `env.kv.*` native primitives.
//!
//! `env.kv` is a `#[v8_class]` instance (`Kv`) minted once per isolate by
//! [`KvBinding::build_instance`]. The instance carries a scoped Rust handle
//! issued from the host's configured KV store.
//!
//! Storage implementations, the backend contract, and typed errors live in
//! zeroship-kv. This crate owns V8 conversion, promises, isolate state,
//! and usage metering. Hosts pass a configured store to [`KvBinding`].
//!
//! Native API surface (wrapped by the `@zeroship/kv` SDK):
//! - `env.kv.get(key)` → Promise<string | null>
//! - `env.kv.set(key, value, {ttlMs?})` → Promise<{ ok: true }>
//! - `env.kv.delete(key)` → Promise<{ deleted: boolean }>
//! - `env.kv.incr(key, {by?, ttlMs?})` → Promise<number | bigint>
//! - `env.kv.setIfAbsent(key, value, {ttlMs?})` → Promise<{ stored: boolean }>
//! - `env.kv.expire(key, ttlMs)` → Promise<{ updated: boolean }>
//! - `env.kv.ttl(key)` → Promise<{ ttlMs: number | null } | null>
//! - `env.kv.persist(key)` → Promise<{ updated: boolean }>
//! - `env.kv.list(prefix?, {cursor?, limit?})` → Promise<{ keys, cursor }>

use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

mod dispatch;
mod error;
mod limits;
mod v8_class;

use v8_class::mint_kv;
use zeroship_kv::{KvStore, Namespace};

// ---------------------------------------------------------------------------
// KvBinding
// ---------------------------------------------------------------------------

pub struct KvBinding {
    store: KvStore,
    /// The process-wide usage meter. `Some` on the production worker (and
    /// dev `zeroship serve`); each kv op emits `kv_reads`/`kv_writes` into
    /// it in its success arm via a [`zeroship_metering::MeterHandle`].
    /// `None` in meter-less test harnesses (metering not under test).
    meter: Option<Arc<zeroship_metering::Meter>>,
}

impl std::fmt::Debug for KvBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvBinding")
            .field("store", &self.store)
            .finish()
    }
}

impl KvBinding {
    /// Bind a configured store to V8. The host owns backend selection and
    /// startup; each isolate receives an app-scoped Rust handle. The optional
    /// process meter records successful creator operations at dispatch.
    #[must_use]
    pub fn new(store: KvStore, meter: Option<Arc<zeroship_metering::Meter>>) -> Self {
        Self { store, meter }
    }
}

impl NativePlugin for KvBinding {
    fn namespace(&self) -> &str {
        "kv"
    }
    fn name(&self) -> &str {
        "kv"
    }

    /// No flat callbacks — the whole surface lives on the `Kv`
    /// v8_class minted by [`Self::build_instance`].
    fn register(&self, _r: &mut NativeRegistrar) {}

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        // Bind the process-wide meter to this isolate's server-injected
        // `app_id` so each op emits a per-app kv metric. `None` ⇒ no
        // metering (test harness): the dispatch layer simply skips the emit.
        let handle = self
            .meter
            .as_ref()
            .map(|m| zeroship_metering::MeterHandle::new(Arc::clone(m), app_id));
        let namespace = match Namespace::app(app_id) {
            Ok(namespace) => namespace,
            Err(error) => {
                let message = v8::String::new(scope, &error.to_string())?;
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return None;
            }
        };
        mint_kv(scope, self.store.namespace(namespace), handle)
    }
}
