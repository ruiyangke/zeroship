use super::*;
use crate::worker_fixture::{dispatch_frame, Worker};
use sha2::{Digest, Sha256};
use zeroship_bundle::LocalDiskBlobStore;

pub struct Blobs {
    pub store: Arc<dyn BlobStore>,
    _directory: tempfile::TempDir,
}

impl Blobs {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("private blob storage");
        let store = Arc::new(LocalDiskBlobStore::new(directory.path().to_owned()).unwrap());
        Self {
            store,
            _directory: directory,
        }
    }

    pub async fn put(&self, bytes: &[u8]) -> String {
        write_blob(&self.store, bytes).await
    }
}

/// The binding store this thread's kernel installed.
///
/// Thread state, not the case's: every isolate on this thread resolves into
/// the one store, which is the property the cases below measure.
pub fn binding_store() -> Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings> {
    crate::cache::app_bindings().expect("this case's kernel installed a database service")
}

pub async fn write_blob(store: &Arc<dyn BlobStore>, bytes: &[u8]) -> String {
    let hash = hex::encode(Sha256::digest(bytes));
    store
        .put_blob(&hash, bytes)
        .await
        .expect("store content-addressed fixture blob");
    hash
}

pub struct DeployedApp {
    pub worker: Worker,
    pub version: AppVersionInfo,
    /// The control plane this case's reconcile addresses. `Worker::new`
    /// points at a port nothing listens on, which is what every case that
    /// must make no control call wants; a case exercising a control read
    /// replaces it with the address of a stub it started.
    pub control_url: String,
}

impl DeployedApp {
    pub async fn new() -> Self {
        Self::on(Worker::new()).await
    }

    /// The same deployed app on a worker whose kernel the case chose - a
    /// database service, so the app has a binding store to resolve into.
    pub async fn with_worker(worker: Worker) -> Self {
        Self::on(worker).await
    }

    async fn on(worker: Worker) -> Self {
        let source = br#"
            export default {
              fetch(req, env) {
                return new Response(
                  [env.API_TOKEN, env.SIGNING_SECRET, process.env.API_TOKEN].join("|")
                );
              }
            }
        "#;
        let blob_hash = write_blob(&worker.config.blob_store, source).await;
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "worker": { "entry": "index.js", "modules": { "index.js": blob_hash } },
        }))
        .expect("deployment manifest");
        let case = Self {
            control_url: worker.config.control_url.clone(),
            worker,
            version: AppVersionInfo {
                deploy_hash: Some("initial-deploy".into()),
                plan_id: "starter".into(),
                runtime: AppRuntimeLimits::default(),
                env_version: 1,
                manifest: Some(manifest),
                net_policy: AppNetPolicy::default(),
            },
        };
        case.set_environment(1, "var-old", "sec-old");
        let env = get_env(&case.worker.envs, &case.worker.app_id).unwrap();
        cache::load_app(
            case.worker.app_id.clone(),
            cache::test_modules(source),
            case.version.runtime.clone(),
            case.version.net_policy.clone(),
            case.version.deploy_hash.as_deref(),
            None,
            case.version.manifest.as_ref().unwrap(),
            &env.snapshot,
        )
        .await
        .expect("initial app loads");
        cache::set_loaded_meta(
            case.worker.app_id.clone(),
            cache::LoadedMeta {
                deploy_hash: case.version.deploy_hash.clone(),
                env_version: case.version.env_version,
                net_policy: case.version.net_policy.clone(),
            },
        );
        case
    }

    pub fn set_environment(&self, version: i64, token: &str, secret: &str) {
        let env = serde_json::json!({
            "vars": { "API_TOKEN": token },
            "secrets": { "SIGNING_SECRET": secret },
            "expose": [],
        });
        put_env_from_json(
            &self.worker.envs,
            self.worker.app_id.clone(),
            &env.to_string(),
            version,
        )
        .expect("seed environment refresh");
    }

    pub async fn reconcile(&self) {
        let versions = VersionMap::from([(self.worker.app_id.clone(), self.version.clone())]);
        reconcile_once(&self.config(), &versions, &self.worker.envs)
            .await
            .expect("reconcile");
    }

    /// This case's worker configuration, addressing [`Self::control_url`].
    pub fn config(&self) -> crate::WorkerConfig {
        crate::WorkerConfig {
            service_auth: self.worker.config.service_auth.clone(),
            control_url: self.control_url.clone(),
            control_key: self.worker.config.control_key.clone(),
            kv_store: None,
            storage_backend: None,
            max_isolates: self.worker.config.max_isolates,
            poll_interval_secs: self.worker.config.poll_interval_secs,
            shutdown_timeout_secs: self.worker.config.shutdown_timeout_secs,
            blob_store: self.worker.config.blob_store.clone(),
        }
    }

    /// The single edge the host has resolved for this app.
    pub fn resolved_binding(&self) -> zeroship_data_orm::resolved_bindings::ResolvedBinding {
        let bindings = binding_store().bindings_for(self.worker.app_id.as_str(), "d");
        let [binding] = bindings.as_slice() else {
            panic!("this case's app holds exactly one binding");
        };
        binding
            .edge()
            .expect("a resolved binding carries its edge")
            .into()
    }

    pub async fn body(&self) -> Vec<u8> {
        let response = self
            .worker
            .dispatch(dispatch_frame("GET", "http://example.test/env-probe", b""))
            .await;
        assert_eq!(
            response.status,
            ntex::http::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.body
    }
}
