#![expect(
    clippy::future_not_send,
    reason = "blob fixtures use their owning compio runtime"
)]

use serde_json::json;
use zeroship_bundle::{
    sha256_hex, BlobStore, LocalDiskBlobStore, Manifest, RuntimeDescriptorEntry, WorkerCode,
};
use zeroship_workflow::{service::BundleExecutable, WorkflowServiceError};

async fn fixture(store: &dyn BlobStore) -> Manifest {
    let mut modules = std::collections::HashMap::new();
    for (name, source) in [
        (
            "entry.js",
            "import value from './part.js'; export default value;",
        ),
        ("part.js", "export default 'retained';"),
    ] {
        let hash = sha256_hex(source.as_bytes());
        store.put_blob(&hash, source.as_bytes()).await.unwrap();
        modules.insert(name.into(), hash);
    }
    let descriptor = br#"{"version":2,"collections":{}}"#;
    let hash = sha256_hex(descriptor);
    store.put_blob(&hash, descriptor).await.unwrap();
    Manifest {
        worker: Some(WorkerCode {
            entry: "entry.js".into(),
            modules,
        }),
        workflows: Some(json!(["Example"])),
        runtime_descriptor: Some(RuntimeDescriptorEntry { hash }),
        ..Manifest::default()
    }
}

#[compio::test]
async fn executable_loading_preserves_code_schema_and_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskBlobStore::new(dir.path().into()).unwrap();
    let manifest = fixture(&store).await;
    let original = BundleExecutable::load(&manifest, &store, 4096)
        .await
        .unwrap();
    assert_eq!(
        original.snapshot().modules()["part.js"],
        "export default 'retained';"
    );
    assert_eq!(
        original.snapshot().runtime_descriptor(),
        Some(&json!({"version":2,"collections":{}}))
    );
    let mut declarations = manifest.clone();
    declarations.workflows = Some(json!(["Other"]));
    let loaded = BundleExecutable::load(&declarations, &store, 4096)
        .await
        .unwrap();
    let registration = loaded.registration("deployment".into(), "a".repeat(64));
    assert_eq!(registration.workflows, ["Other".into()].into());
    assert_eq!(registration.hash, "a".repeat(64));
    let mut changes = vec![];
    let mut schema = manifest.clone();
    schema.runtime_descriptor = None;
    changes.push(schema);
    let mut code = manifest.clone();
    let source = b"export default 'replacement';";
    let hash = sha256_hex(source);
    store.put_blob(&hash, source).await.unwrap();
    code.worker
        .as_mut()
        .unwrap()
        .modules
        .insert("part.js".into(), hash);
    changes.push(code);
    for changed in changes {
        assert_ne!(
            BundleExecutable::load(&changed, &store, 4096)
                .await
                .unwrap()
                .snapshot(),
            original.snapshot()
        );
    }
}

#[compio::test]
async fn executable_reads_refuse_missing_corrupt_and_over_budget_sources() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskBlobStore::new(dir.path().into()).unwrap();
    let manifest = fixture(&store).await;
    assert!(matches!(
        BundleExecutable::load(&manifest, &store, 1).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    let hash = &manifest.worker.as_ref().unwrap().modules["part.js"];
    let blob_path = dir.path().join("blobs").join(&hash[..2]).join(&hash[2..]);
    // Bypass the content-addressed writer to model damaged retained source.
    std::fs::write(&blob_path, b"damaged").unwrap();
    assert!(BundleExecutable::load(&manifest, &store, 4096)
        .await
        .is_err());
    std::fs::remove_file(&blob_path).unwrap();
    assert!(BundleExecutable::load(&manifest, &store, 4096)
        .await
        .is_err());
    let mut invalid = manifest.clone();
    invalid.workflows = Some(json!({"Example": true}));
    assert!(matches!(
        BundleExecutable::load(&invalid, &store, 4096).await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
}
