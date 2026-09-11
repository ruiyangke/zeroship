//! V8 binding for the scoped Rust object-storage service.
//!
//! The host supplies a configured store. Each isolate captures its app namespace,
//! meter and download registry at construction. Callbacks cannot select another
//! app or replace another isolate's backend.

use std::rc::Rc;
use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_storage::{Namespace, Storage, StorageStore};

mod callbacks;
pub mod limits;
mod live_streams;

zeroship_core::declare_env_consumer!(
    /// Limits for V8 object-storage download handles.
    pub StorageV8Consumer,
    target = "zeroship-storage-v8",
    scope = "storage_v8");

#[derive(Clone)]
pub(crate) struct StorageContext {
    pub storage: Storage,
    pub meter: Option<zeroship_metering::MeterHandle>,
    pub streams: Rc<live_streams::LiveStreams>,
}

/// Registers `env.storage` over a host-owned store.
/// Storage configuration and Rust operations live in `zeroship-storage`.
pub struct StorageBinding {
    store: StorageStore,
    meter: Option<Arc<zeroship_metering::Meter>>,
}

impl std::fmt::Debug for StorageBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageBinding")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl StorageBinding {
    #[must_use]
    pub fn new(store: StorageStore, meter: Option<Arc<zeroship_metering::Meter>>) -> Self {
        Self { store, meter }
    }
}

impl NativePlugin for StorageBinding {
    fn namespace(&self) -> &str {
        "storage"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("put", callbacks::put);
        r.add("get", callbacks::get);
        r.add("delete", callbacks::delete);
        r.add("list", callbacks::list);
        r.add("putStream", callbacks::put_stream);
        r.add("getStream", callbacks::get_stream);
        r.add("readChunk", callbacks::read_chunk);
        r.add("cancelStream", callbacks::cancel_stream);
    }

    fn bind_runtime_descriptor(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        app_id: &str,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        let has_identity = scope
            .get_slot::<zeroship_runtime::state::SharedState>()
            .is_some_and(|state| state.borrow().env_vars.contains_key("APP_ID"));
        if !has_identity {
            return Err("storage: the host must supply APP_ID".into());
        }
        Namespace::app(app_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let namespace = match Namespace::app(app_id) {
            Ok(namespace) => namespace,
            Err(error) => {
                let message = v8::String::new(scope, &error.to_string())?;
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return None;
            }
        };
        scope.set_slot(StorageContext {
            storage: self.store.namespace(namespace),
            meter: self
                .meter
                .as_ref()
                .map(|meter| zeroship_metering::MeterHandle::new(Arc::clone(meter), app_id)),
            streams: Rc::new(live_streams::LiveStreams::new(app_id)),
        });
        Some(v8::Object::new(scope))
    }
}
