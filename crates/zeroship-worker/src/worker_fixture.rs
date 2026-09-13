use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, Manifest};
use zeroship_core::app_id::AppId;
use zeroship_core::types::AppRuntimeLimits;
use zeroship_core::usage_event::UsageEvent;
use zeroship_runtime::EnvSnapshot;

use crate::cache::fixture::Kernel;
use crate::cache::KernelConfig;
use crate::identity_fixture::{gateway_authorization, service_auth};
use crate::sync::SharedEnvs;

pub struct Worker {
    // Drop thread-local isolates and services before their backing storage.
    _kernel: Kernel,
    pub app_id: AppId,
    pub config: Arc<crate::WorkerConfig>,
    pub envs: SharedEnvs,
    pub logs: crate::logs::SharedLogs,
    pub meter: Arc<zeroship_metering::Meter>,
    pub storage: tempfile::TempDir,
}

impl Worker {
    pub fn new() -> Self {
        Self::with_kernel(10, empty_kernel(Arc::new(zeroship_metering::Meter::new())))
    }

    pub fn with_kernel(max_size: usize, kernel: KernelConfig) -> Self {
        zeroship_runtime::init::init_v8();
        let storage = tempfile::tempdir().expect("private worker storage");
        let blob_store: Arc<dyn BlobStore> = Arc::new(
            LocalDiskBlobStore::new(storage.path().to_owned()).expect("worker blob store"),
        );
        let workflow_blob_store = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(storage.path().to_owned())
                .expect("worker workflow blob store"),
        );
        let meter = kernel.meter.clone();
        let config = Arc::new(crate::WorkerConfig {
            service_auth: service_auth(),
            control_url: kernel.control_url.clone(),
            control_key: kernel.control_key.clone(),
            db_url: kernel
                .db_service
                .as_ref()
                .and_then(|service| service.connection().url().map(str::to_owned)),
            kv_store: kernel.kv_store.clone(),
            storage_backend: kernel.storage_backend.clone(),
            max_isolates: max_size,
            max_pinned_isolates_per_app: 4,
            poll_interval_secs: 60,
            shutdown_timeout_secs: 0,
            blob_store,
            workflow_blob_store,
            max_step_blob_bytes: 64 * 1024 * 1024,
            workflow_advance_unsigned: false,
        });
        let app_id = AppId::mint();
        let envs = SharedEnvs::default();
        crate::sync::put_env_from_json(
            &envs,
            app_id.clone(),
            r#"{"vars":{},"secrets":{},"expose":[]}"#,
            0,
        )
        .expect("seed worker environment");
        Self {
            _kernel: Kernel::install(max_size, 4, kernel),
            app_id,
            config,
            envs,
            logs: crate::logs::new_store(),
            meter,
            storage,
        }
    }

    pub async fn load(&self, source: &[u8], limits: AppRuntimeLimits, manifest: &Manifest) {
        crate::cache::load_app(
            self.app_id.clone(),
            crate::cache::test_modules(source),
            limits,
            Default::default(),
            None,
            None,
            manifest,
            &EnvSnapshot::empty(),
        )
        .await
        .expect("app loads");
    }

    pub fn configure(&self) -> impl FnOnce(&mut web::ServiceConfig) {
        let config = self.config.clone();
        let envs = self.envs.clone();
        let logs = self.logs.clone();
        move |app| {
            app.state(config).state(envs).state(logs);
            crate::handler::configure(app);
        }
    }

    pub async fn dispatch(&self, payload: Vec<u8>) -> Response {
        let app = test::init_service(web::App::new().configure(self.configure())).await;
        let request = test::TestRequest::post()
            .uri(&format!("/dispatch/{}", self.app_id.as_str()))
            .header("authorization", gateway_authorization())
            .set_payload(payload)
            .to_request();
        drop(self.meter.drain());
        let cpu_started = zeroship_runtime::init::thread_cpu_time();
        let response = test::call_service(&app, request).await;
        let status = response.status();
        let body = test::read_body(response).await.to_vec();
        let thread_cpu = zeroship_runtime::init::thread_cpu_time().saturating_sub(cpu_started);
        Response {
            app_id: self.app_id.clone(),
            status,
            body,
            events: self.meter.drain(),
            thread_cpu,
        }
    }
}

pub struct Response {
    pub app_id: AppId,
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub events: Vec<UsageEvent>,
    pub thread_cpu: std::time::Duration,
}

pub fn empty_kernel(meter: Arc<zeroship_metering::Meter>) -> KernelConfig {
    KernelConfig {
        control_url: "http://127.0.0.1:1".into(),
        control_key: String::new(),
        db_service: None,
        kv_store: None,
        storage_backend: None,
        meter,
    }
}

pub fn dispatch_frame(method: &str, url: &str, body: &[u8]) -> Vec<u8> {
    zeroship_core::dispatch_frame::encode_dispatch_frame(method, url, &[], body)
        .expect("dispatch frame")
}

pub fn usage_value(events: &[UsageEvent], app_id: &AppId, meter: &str) -> Option<u64> {
    events
        .iter()
        .find(|event| event.subject.app.as_ref() == Some(app_id) && event.meter == meter)
        .map(|event| event.value)
}
