//! Scope thread-local worker resources to a test case.

use super::*;

pub struct Kernel(std::marker::PhantomData<Rc<()>>);

impl Kernel {
    pub(crate) fn new(max_pinned: usize, url: &str, meter: Arc<zeroship_metering::Meter>) -> Self {
        Self::install(
            10,
            max_pinned,
            KernelConfig {
                control_url: "http://127.0.0.1:1".into(),
                control_key: String::new(),
                db_service: Some(database_service(url)),
                kv_store: None,
                storage_backend: None,
                meter,
            },
        )
    }

    pub(crate) fn install(max_size: usize, max_pinned: usize, config: KernelConfig) -> Self {
        assert!(
            CACHE.with(|slot| slot.borrow().is_none()),
            "a previous case retained its worker cache"
        );
        let kernel = Self(std::marker::PhantomData);
        init_cache(max_size, max_pinned, config);
        kernel
    }
}

#[compio::test]
async fn unwinding_releases_the_thread_kernel_and_its_database_service() {
    let service = RefCell::new(None);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _kernel = Kernel::new(
            1,
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
        CONTROL_URL.with(|slot| slot.borrow_mut().take());
        CONTROL_KEY.with(|slot| slot.borrow_mut().take());
        METER.with(|slot| slot.borrow_mut().take());
        LOADED_META.with(|slot| slot.borrow_mut().clear());
    }
}

pub(crate) fn database_service(url: &str) -> Arc<zeroship_data_v8::service::DbService> {
    zeroship_data_v8::service::DbService::new(zeroship_data_v8::service::DbServiceConfig {
        project_keys: Default::default(),
        connection: zeroship_data_orm::connection::ConnectionFactory::for_url(url)
            .expect("valid database configuration"),
        cdc_relay: None,
        meter: None,
    })
    .expect("test db service")
}
