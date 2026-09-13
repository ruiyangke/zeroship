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
}

impl DeployedApp {
    pub async fn new() -> Self {
        let worker = Worker::new();
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
            source,
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
        reconcile_once(&self.worker.config, &versions, &self.worker.envs)
            .await
            .expect("reconcile");
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
