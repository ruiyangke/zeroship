use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;
use zeroship_bundle::{
    ingest, sha256_hex, verify_deployment_manifest, BlobStore, ExecutableError, LoadedWorker,
    LocalDiskBlobStore, Manifest,
};

fn archive(manifest: &Value, blobs: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut append = |path: &str, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, bytes).unwrap();
    };
    append("manifest.json", &serde_json::to_vec(manifest).unwrap());
    for (hash, bytes) in blobs {
        append(&format!("blobs/{hash}"), bytes);
    }
    zstd::encode_all(tar.into_inner().unwrap().as_slice(), 0).unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<dyn BlobStore>,
    app: Uuid,
    manifest: Manifest,
    manifest_json: String,
    hash: String,
    budget: usize,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(directory.path().into()).unwrap());
        let app = Uuid::now_v7();
        let entry = b"import value from './part.js'; export default value;";
        let part = b"export default 'original';";
        let descriptor = br#"{"version":2,"collections":{}}"#;
        let entry_hash = sha256_hex(entry);
        let part_hash = sha256_hex(part);
        let descriptor_hash = sha256_hex(descriptor);
        let raw = json!({
            "version":1,
            "worker":{"entry":"entry.js", "modules":{"entry.js":entry_hash, "part.js":part_hash}},
            "runtime_descriptor":{"hash":descriptor_hash},
            "metadata":{"built_at":"fixture"},
            "build_metadata":{"origin":"creator"},
        });
        let packed = archive(
            &raw,
            &[
                (&entry_hash, entry),
                (&part_hash, part),
                (&descriptor_hash, descriptor),
            ],
        );
        let installed = ingest(&store, &app, &packed).await.unwrap();
        let bytes = store
            .get_manifest(&app, &installed.deploy_hash)
            .await
            .unwrap();
        let manifest = verify_deployment_manifest(&bytes, &installed.deploy_hash).unwrap();
        let budget = serde_json::to_vec(manifest.worker.as_ref().unwrap())
            .unwrap()
            .len()
            + entry.len()
            + part.len()
            + descriptor.len();
        Self {
            _directory: directory,
            store,
            app,
            manifest,
            manifest_json: installed.manifest_json,
            hash: installed.deploy_hash,
            budget,
        }
    }
}

#[compio::test]
async fn normal_deployments_load_their_own_modules_and_descriptor() {
    let fixture = Fixture::new().await;
    let loaded = LoadedWorker::load(&fixture.manifest, fixture.store.as_ref(), fixture.budget)
        .await
        .unwrap();
    assert_eq!(loaded.entry(), "entry.js");
    assert_eq!(loaded.modules()["part.js"], "export default 'original';");
    assert_eq!(
        loaded.runtime_descriptor(),
        Some(&json!({"version":2,"collections":{}}))
    );

    let mut newer: Value = serde_json::from_str(&fixture.manifest_json).unwrap();
    let replacement = b"export default 'replacement';";
    let replacement_hash = sha256_hex(replacement);
    newer["worker"]["modules"]["part.js"] = json!(replacement_hash);
    let entry_hash = newer["worker"]["modules"]["entry.js"].as_str().unwrap();
    let descriptor_hash = newer["runtime_descriptor"]["hash"].as_str().unwrap();
    let entry = fixture.store.get_blob(entry_hash).await.unwrap();
    let descriptor = fixture.store.get_blob(descriptor_hash).await.unwrap();
    let installed = ingest(
        &fixture.store,
        &fixture.app,
        &archive(
            &newer,
            &[
                (entry_hash, &entry),
                (descriptor_hash, &descriptor),
                (&replacement_hash, replacement),
            ],
        ),
    )
    .await
    .unwrap();
    assert_ne!(installed.deploy_hash, fixture.hash);
    let manifest =
        verify_deployment_manifest(installed.manifest_json.as_bytes(), &installed.deploy_hash)
            .unwrap();
    let replacement = LoadedWorker::load(
        &manifest,
        fixture.store.as_ref(),
        fixture.budget + replacement.len(),
    )
    .await
    .unwrap();
    assert_eq!(
        replacement.modules()["part.js"],
        "export default 'replacement';"
    );

    let old = fixture
        .store
        .get_manifest(&fixture.app, &fixture.hash)
        .await
        .unwrap();
    let original = verify_deployment_manifest(&old, &fixture.hash).unwrap();
    let replay = LoadedWorker::load(&original, fixture.store.as_ref(), fixture.budget)
        .await
        .unwrap();
    assert_eq!(replay.modules(), loaded.modules());
    assert_eq!(replay.runtime_descriptor(), loaded.runtime_descriptor());
}

#[compio::test]
async fn stored_manifest_verification_covers_identity_and_extension_fields() {
    let fixture = Fixture::new().await;
    for (field, value) in [
        ("deploy_hash", json!("0".repeat(64))),
        ("metadata", json!({"built_at":"changed"})),
        ("build_metadata", json!({"origin":"changed"})),
    ] {
        let mut changed: Value = serde_json::from_str(&fixture.manifest_json).unwrap();
        changed[field] = value;
        assert!(matches!(
            verify_deployment_manifest(&serde_json::to_vec(&changed).unwrap(), &fixture.hash),
            Err(ExecutableError::ManifestIdentity)
        ));
    }
    assert!(matches!(
        verify_deployment_manifest(fixture.manifest_json.as_bytes(), &"0".repeat(64)),
        Err(ExecutableError::ManifestIdentity)
    ));
    assert!(matches!(
        verify_deployment_manifest(b"[]", &fixture.hash),
        Err(ExecutableError::InvalidManifest)
    ));
}

#[compio::test]
async fn worker_loading_rejects_corrupt_missing_and_oversized_sources() {
    let fixture = Fixture::new().await;
    assert!(LoadedWorker::load(
        &fixture.manifest,
        fixture.store.as_ref(),
        fixture.budget - 1
    )
    .await
    .is_err());
    assert!(matches!(
        LoadedWorker::load(&fixture.manifest, fixture.store.as_ref(), 0).await,
        Err(ExecutableError::TooLarge)
    ));
    let hash = &fixture.manifest.worker.as_ref().unwrap().modules["part.js"];
    let path = fixture.store.local_path(hash).unwrap();
    std::fs::write(&path, b"corrupted").unwrap();
    assert!(
        LoadedWorker::load(&fixture.manifest, fixture.store.as_ref(), fixture.budget)
            .await
            .is_err()
    );
    std::fs::remove_file(path).unwrap();
    assert!(
        LoadedWorker::load(&fixture.manifest, fixture.store.as_ref(), fixture.budget)
            .await
            .is_err()
    );
}

#[compio::test]
async fn worker_loading_rejects_invalid_paths_text_and_descriptor_json() {
    let fixture = Fixture::new().await;
    let invalid = b"\xff";
    let hash = sha256_hex(invalid);
    fixture.store.put_blob(&hash, invalid).await.unwrap();
    let mut changed = fixture.manifest.clone();
    changed
        .worker
        .as_mut()
        .unwrap()
        .modules
        .insert("part.js".into(), hash.clone());
    assert!(matches!(
        LoadedWorker::load(&changed, fixture.store.as_ref(), fixture.budget).await,
        Err(ExecutableError::InvalidExecutable)
    ));
    changed = fixture.manifest.clone();
    changed.runtime_descriptor.as_mut().unwrap().hash = hash.clone();
    assert!(matches!(
        LoadedWorker::load(&changed, fixture.store.as_ref(), fixture.budget).await,
        Err(ExecutableError::InvalidExecutable)
    ));
    for path in [
        "../part.js",
        "/part.js",
        "file:part.js",
        "a\\part.js",
        "a//part.js",
    ] {
        changed = fixture.manifest.clone();
        changed
            .worker
            .as_mut()
            .unwrap()
            .modules
            .insert(path.into(), hash.clone());
        assert!(matches!(
            LoadedWorker::load(&changed, fixture.store.as_ref(), fixture.budget).await,
            Err(ExecutableError::InvalidExecutable)
        ));
    }
}
