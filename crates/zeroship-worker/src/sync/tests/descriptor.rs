use super::fixture::Blobs;
use super::*;
use zeroship_bundle::RuntimeDescriptorEntry;

#[compio::test]
async fn runtime_descriptor_absent_is_schema_less() {
    let blobs = Blobs::new();
    let descriptor = load_executable(&executable_manifest(&blobs).await, &blobs.store)
        .await
        .expect("schema-less deployment resolves");
    assert_eq!(descriptor.descriptor, None);
}

#[compio::test]
async fn runtime_descriptor_blob_fetch_failure_is_load_error() {
    let blobs = Blobs::new();
    let hash = "c".repeat(64);
    let manifest = Manifest {
        runtime_descriptor: vec![RuntimeDescriptorEntry {
            label: "main".into(),
            database_id: zeroship_core::DatabaseId::mint(),
            primary: true,
            hash: hash.clone(),
        }],
        ..executable_manifest(&blobs).await
    };
    let error = load_executable(&manifest, &blobs.store)
        .await
        .expect_err("missing descriptor blob refuses the load");
    assert!(error.contains("app executable storage"), "{error}");
}

#[compio::test]
async fn runtime_descriptor_is_read_from_its_content_addressed_blob() {
    let blobs = Blobs::new();
    let content = r#"{"collections":[],"label":"descriptor payload"}"#;
    let hash = blobs.put(content.as_bytes()).await;
    let manifest = Manifest {
        runtime_descriptor: vec![RuntimeDescriptorEntry {
            label: "main".into(),
            database_id: zeroship_core::DatabaseId::mint(),
            primary: true,
            hash: hash,
        }],
        ..executable_manifest(&blobs).await
    };
    let descriptor = load_executable(&manifest, &blobs.store)
        .await
        .expect("stored descriptor resolves");
    assert_eq!(
        crate::executable::primary_schema_json(descriptor.descriptor.as_deref()),
        serde_json::from_str::<serde_json::Value>(content).unwrap(),
    );
}

#[compio::test]
async fn runtime_descriptor_invalid_utf8_is_a_load_error() {
    let blobs = Blobs::new();
    let hash = blobs.put(&[0xff, 0xfe]).await;
    let manifest = Manifest {
        runtime_descriptor: vec![RuntimeDescriptorEntry {
            label: "main".into(),
            database_id: zeroship_core::DatabaseId::mint(),
            primary: true,
            hash: hash.clone(),
        }],
        ..executable_manifest(&blobs).await
    };
    let error = load_executable(&manifest, &blobs.store)
        .await
        .expect_err("invalid descriptor encoding refuses the load");
    assert!(error.contains("invalid app executable"), "{error}");
}

#[expect(
    clippy::future_not_send,
    reason = "fixture blob I/O stays on its compio runtime"
)]
async fn executable_manifest(blobs: &Blobs) -> Manifest {
    let hash = blobs
        .put(b"export default { fetch() { return new Response('ok'); } };")
        .await;
    Manifest {
        worker: Some(zeroship_bundle::WorkerCode {
            entry: "index.js".into(),
            modules: [("index.js".into(), hash)].into(),
        }),
        ..Manifest::default()
    }
}
