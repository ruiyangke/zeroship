use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_core::app_id::AppId;

use super::super::tests::init_runtime;
use super::journal::Journal;
use crate::identity_fixture::service_auth;
use crate::sync::SharedEnvs;
use crate::test_database::Database;

pub(super) struct Fixture<'db> {
    pub app_id: AppId,
    pub blob_store: Arc<dyn BlobStore>,
    pub envs: SharedEnvs,
    pub logs: crate::logs::SharedLogs,
    pub config: Arc<crate::WorkerConfig>,
    pub meter: Arc<zeroship_metering::Meter>,
    pub journal: Journal<'db>,
    _kernel: crate::cache::fixture::Kernel,
    _storage: tempfile::TempDir,
}

impl Fixture<'_> {
    pub async fn run(max_pinned: usize, test: impl AsyncFnOnce(&Fixture<'_>)) {
        Database::migrated(async |database| {
            init_runtime();
            let app_id = AppId::mint();
            let meter = Arc::new(zeroship_metering::Meter::new());
            let worker_url = database.url_as("zeroship_worker");
            zeroship_worker::db_posture::validate_database_url(worker_url.as_str())
                .await
                .unwrap();
            let kernel =
                crate::cache::fixture::Kernel::new(max_pinned, worker_url.as_str(), meter.clone());
            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id.clone(),
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .unwrap();
            let storage = tempfile::tempdir().expect("workflow storage");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(storage.path().to_owned()).unwrap());
            let workflow_blob_store = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(storage.path().to_owned()).unwrap(),
            );
            let config = Arc::new(crate::WorkerConfig {
                service_auth: service_auth(),
                control_url: "http://127.0.0.1:1".into(),
                control_key: String::new(),
                db_url: Some(worker_url.into()),
                kv_store: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: max_pinned,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store: blob_store.clone(),
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: true,
            });
            let journal = Journal::new(database, &app_id).await;
            let fixture = Fixture {
                app_id,
                blob_store,
                envs,
                logs: crate::logs::new_store(),
                config,
                meter,
                journal,
                _kernel: kernel,
                _storage: storage,
            };
            test(&fixture).await;
        })
        .await;
    }

    pub async fn deploy(&self, mark: &str) -> String {
        let deploy_hash = deploy_workflow_fixture(&self.blob_store, &self.app_id, mark).await;
        self.journal.record_deploy(&deploy_hash).await;
        deploy_hash
    }
}

async fn deploy_workflow_fixture(
    blob_store: &Arc<dyn BlobStore>,
    app_id: &AppId,
    mark: &str,
) -> String {
    let source = include_str!("workflow.js").replace("__MARK__", mark);
    let zship = workflow_zship(source.as_bytes());
    zeroship_bundle::ingest(blob_store, app_id, &zship)
        .await
        .expect("ingest workflow deploy")
        .deploy_hash
}

fn append_tar_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, std::io::Cursor::new(bytes))
        .expect("append tar file");
}

fn workflow_zship(source: &[u8]) -> Vec<u8> {
    let source_hash = zeroship_bundle::sha256_hex(source);
    let manifest = serde_json::json!({
        "version": 1,
        "worker": {
            "entry": "index.js",
            "modules": { "index.js": source_hash },
        },
        "resources": {},
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0,
        "sourcemaps": {},
        "metadata": { "built_at": "2026-07-06T00:00:00Z" },
    });
    let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest json");
    let mut builder = tar::Builder::new(Vec::new());
    append_tar_file(&mut builder, "manifest.json", &manifest_bytes);
    append_tar_file(&mut builder, &format!("blobs/{source_hash}"), source);
    let tar_bytes = builder.into_inner().expect("tar bytes");
    zstd::stream::encode_all(std::io::Cursor::new(tar_bytes), 0).expect("zstd encode")
}

#[test]
fn pinned_loader_verifies_stored_manifest_content_and_app_scope() {
    use super::super::{load_pinned_workflow_on_demand, tests::test_worker_config};

    std::thread::spawn(|| {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            init_runtime();
            let root = tempfile::tempdir().unwrap();
            let config = test_worker_config(root.path());
            let app_id = AppId::mint();
            crate::cache::init_cache(
                4,
                4,
                crate::cache::KernelConfig {
                    control_url: config.control_url.clone(),
                    control_key: String::new(),
                    db_service: None,
                    kv_store: None,
                    storage_backend: None,
                    meter: Arc::new(zeroship_metering::Meter::new()),
                },
            );
            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            crate::sync::put_env_from_json(
                &envs,
                app_id.clone(),
                r#"{"vars":{},"secrets":{},"expose":[]}"#,
                0,
            )
            .unwrap();
            let original = deploy_workflow_fixture(&config.blob_store, &app_id, "original").await;
            let replacement =
                deploy_workflow_fixture(&config.blob_store, &app_id, "replacement").await;
            load_pinned_workflow_on_demand(&config, &envs, &app_id, &original)
                .await
                .unwrap();
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &original));

            let bytes = config
                .blob_store
                .get_manifest(&app_id, &replacement)
                .await
                .unwrap();
            let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            changed["deploy_hash"] = serde_json::json!(original);
            let path = root
                .path()
                .join("manifests")
                .join(app_id.as_str())
                .join(format!("{original}.json"));
            std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            let error = load_pinned_workflow_on_demand(&config, &envs, &app_id, &original)
                .await
                .unwrap_err();
            assert!(error.contains("manifest validation failed"));
            assert!(crate::cache::has_pinned_workflow_app(&app_id, &original));

            std::fs::remove_file(path).unwrap();
            assert!(
                load_pinned_workflow_on_demand(&config, &envs, &app_id, &original)
                    .await
                    .is_err()
            );
            assert!(
                load_pinned_workflow_on_demand(&config, &envs, &AppId::mint(), &replacement)
                    .await
                    .is_err()
            );
        });
    })
    .join()
    .unwrap();
}

pub(super) fn workflow_request(app_id: &AppId) -> serde_json::Value {
    workflow_request_for_run(app_id, "run_test")
}

pub(super) fn workflow_request_for_run(app_id: &AppId, run_id: &str) -> serde_json::Value {
    serde_json::json!({
        "runId": run_id,
        "appId": app_id.as_str(),
    })
}

pub(super) fn assert_workflow_ack(body: &[u8], run_id: &str) -> serde_json::Value {
    let result: serde_json::Value =
        serde_json::from_slice(body).expect("workflow advance ack JSON");
    assert_eq!(
        result["ack"], true,
        "workflow advance should ack: {result:?}"
    );
    assert_eq!(result["runId"], run_id);
    assert!(
        result["registrations"]
            .as_array()
            .is_some_and(|registrations| registrations
                .iter()
                .any(|registration| registration["runId"] == run_id)),
        "ack registrations should include dispatched run: {result:?}"
    );
    result
}

pub(super) fn assert_workflow_nack(body: &[u8], run_id: &str, kind: &str) -> serde_json::Value {
    let result: serde_json::Value =
        serde_json::from_slice(body).expect("workflow advance nack JSON");
    assert_eq!(
        result["nack"], true,
        "workflow advance should nack: {result:?}"
    );
    assert_eq!(result["runId"], run_id);
    assert_eq!(result["nackKind"], kind);
    result
}
