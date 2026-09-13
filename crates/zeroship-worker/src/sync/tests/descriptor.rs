use super::fixture::Blobs;
use super::*;
use zeroship_bundle::RuntimeDescriptorEntry;

#[compio::test]
async fn runtime_descriptor_absent_is_schema_less() {
    let blobs = Blobs::new();
    let descriptor = runtime_descriptor_json(&Manifest::default(), &blobs.store, &AppId::mint())
        .await
        .expect("schema-less deployment resolves");
    assert_eq!(descriptor, None);
}

#[compio::test]
async fn runtime_descriptor_blob_fetch_failure_is_load_error() {
    let blobs = Blobs::new();
    let hash = "c".repeat(64);
    let manifest = Manifest {
        runtime_descriptor: Some(RuntimeDescriptorEntry { hash: hash.clone() }),
        ..Manifest::default()
    };
    let error = runtime_descriptor_json(&manifest, &blobs.store, &AppId::mint())
        .await
        .expect_err("missing descriptor blob refuses the load");
    assert!(
        error.contains(&hash) && error.contains("fetch failed"),
        "{error}"
    );
}

#[compio::test]
async fn runtime_descriptor_is_read_from_its_content_addressed_blob() {
    let blobs = Blobs::new();
    let content = r#"{"collections":[],"label":"descriptor payload"}"#;
    let hash = blobs.put(content.as_bytes()).await;
    let manifest = Manifest {
        runtime_descriptor: Some(RuntimeDescriptorEntry { hash }),
        ..Manifest::default()
    };
    let descriptor = runtime_descriptor_json(&manifest, &blobs.store, &AppId::mint())
        .await
        .expect("stored descriptor resolves");
    assert_eq!(descriptor.as_deref(), Some(content));
}

#[compio::test]
async fn runtime_descriptor_invalid_utf8_is_a_load_error() {
    let blobs = Blobs::new();
    let hash = blobs.put(&[0xff, 0xfe]).await;
    let manifest = Manifest {
        runtime_descriptor: Some(RuntimeDescriptorEntry { hash: hash.clone() }),
        ..Manifest::default()
    };
    let error = runtime_descriptor_json(&manifest, &blobs.store, &AppId::mint())
        .await
        .expect_err("invalid descriptor encoding refuses the load");
    assert!(
        error.contains(&hash) && error.contains("not UTF-8"),
        "{error}"
    );
}
