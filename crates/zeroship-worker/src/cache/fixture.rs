//! Scope thread-local worker resources to a test case.

use super::*;

pub struct Kernel(std::marker::PhantomData<Rc<()>>);

impl Kernel {
    pub(crate) fn new(url: &str, meter: Arc<zeroship_metering::Meter>) -> Self {
        Self::install(
            10,
            KernelConfig {
                workflows: ReadyApps::default(),
                db_service: Some(database_service(url)),
                kv_store: None,
                storage_backend: None,
                meter,
            },
        )
    }

    pub(crate) fn install(max_size: usize, config: KernelConfig) -> Self {
        assert!(
            CACHE.with(|slot| slot.borrow().is_none()),
            "a previous case retained its worker cache"
        );
        let kernel = Self(std::marker::PhantomData);
        init_cache(max_size, config);
        kernel
    }
}

#[compio::test]
async fn unwinding_releases_the_thread_kernel_and_its_database_service() {
    let service = RefCell::new(None);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _kernel = Kernel::new(
            "postgresql://fixture:fixture@localhost/unused",
            Arc::new(zeroship_metering::Meter::new()),
        );
        *service.borrow_mut() = DB_SERVICE.with(|slot| slot.borrow().as_ref().map(Arc::downgrade));
        assert!(!plugin_set().is_empty());
        panic!("intentional kernel fixture failure");
    }))
    .expect_err("propagate case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional kernel fixture failure")
    );
    assert!(CACHE.with(|slot| slot.borrow().is_none()));
    assert!(PLUGIN_SET.with(|slot| slot.borrow().is_none()));
    assert!(DB_SERVICE.with(|slot| slot.borrow().is_none()));
    assert!(service.into_inner().unwrap().upgrade().is_none());
}

impl Drop for Kernel {
    fn drop(&mut self) {
        CACHE.with(|slot| slot.borrow_mut().take());
        PLUGIN_SET.with(|slot| slot.borrow_mut().take());
        DB_SERVICE.with(|slot| slot.borrow_mut().take());
        KV_STORE.with(|slot| slot.borrow_mut().take());
        STORAGE_BACKEND.with(|slot| slot.borrow_mut().take());
        WORKFLOWS.with(|slot| slot.borrow_mut().take());
        METER.with(|slot| slot.borrow_mut().take());
        LOADED_META.with(|slot| slot.borrow_mut().clear());
    }
}

/// Resolve one database binding for `app_id`, the step
/// [`crate::sync::fetch_app_env_supplying`] performs before the worker builds
/// an isolate for an app Control serves a live binding for.
///
/// A host that installs the `db` namespace and resolves nothing for an app has
/// no `env.db` to hand it: `zeroship_data_v8` refuses to mint the handle, so
/// the deployment's startup mask policy has no binding to seal. A fixture that
/// means to exercise a working `env.db` therefore has to resolve a binding
/// first, exactly as the sync path does.
///
/// Returns the database's id, which a caller naming this database in a runtime
/// descriptor document needs.
pub(crate) fn bind_app(
    service: &zeroship_data_v8::service::DbService,
    app_id: &AppId,
) -> zeroship_core::DatabaseId {
    let database = zeroship_core::DatabaseId::mint();
    service
        .app_bindings()
        .supply(
            app_id.as_str(),
            zeroship_data_orm::resolved_bindings::ResolvedBinding {
                database: database.clone(),
                binding: zeroship_core::BindingId::mint(),
                capability: zeroship_core::database_role::DatabaseCapability::ReadWrite,
            },
        )
        .expect("a fresh store accepts this app's first binding");
    database
}

pub(crate) fn database_service(url: &str) -> Arc<zeroship_data_v8::service::DbService> {
    zeroship_data_v8::service::DbService::new(zeroship_data_v8::service::DbServiceConfig {
        app_bindings: Default::default(),
        project_keys: Default::default(),
        connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(url)
            .expect("valid database configuration"),
        cdc_relay: None,
        meter: None,
    })
    .expect("test db service")
}
