//! Faithful end-to-end ingest test for the runtime schema descriptor slot.
//!
//! Builds a real `.zship`-shaped `tar.zst` (manifest.json first, then
//! `blobs/<hash>` entries — exactly the wire format the vite packer emits),
//! pipes it through the REAL [`zeroship_bundle::ingest`] path against a real
//! [`LocalDiskBlobStore`], then reads back the stored manifest + the descriptor
//! blob and asserts the descriptor survives byte-identical.
//!
//! This exercises the full carrying capacity of the descriptor slot: serde,
//! `runtime_descriptor` slot, `collect_expected_hashes` requiring the descriptor
//! blob to be present in the tar (a missing blob would 400), and the blob's
//! content-addressed round-trip. Migration documents are applied through the
//! migration service and are rejected if a legacy manifest still carries them.

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;
use zeroship_bundle::blob::{sha256_hex, BlobStore, LocalDiskBlobStore};
use zeroship_bundle::{ingest, IngestError, Manifest, RuntimeDescriptorEntry};

fn tmpdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zeroship-rtd-ingest-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pack a manifest + a set of `(hash → bytes)` blobs into the `.zship` wire form
/// (`tar.zst`, manifest.json first, blobs sorted by hash) — mirrors the vite
/// packer's archive layout so the test drives the real ingest contract.
fn pack(manifest: &Manifest, blobs: &[(String, Vec<u8>)]) -> Vec<u8> {
    let manifest_json = serde_json::to_vec(manifest).unwrap();
    pack_raw_manifest(&manifest_json, blobs)
}

fn pack_raw_manifest(manifest_json: &[u8], blobs: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);

        // manifest.json MUST be first.
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_json.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", manifest_json)
            .unwrap();

        // blobs/<hash>, sorted by hash for determinism.
        let mut sorted: Vec<&(String, Vec<u8>)> = blobs.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (hash, bytes) in sorted {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("blobs/{hash}"), bytes.as_slice())
                .unwrap();
        }
        builder.finish().unwrap();
    }

    zstd::encode_all(tar_buf.as_slice(), 0).unwrap()
}

fn base_manifest() -> Manifest {
    Manifest {
        metadata: zeroship_bundle::ManifestMetadata {
            compiler: Some("test".into()),
            built_at: "2026-06-25T00:00:00Z".into(),
        },
        ..Manifest::default()
    }
}

#[compio::test]
async fn descriptor_survives_pack_then_ingest_byte_identical() {
    // The exact JSON gen-types' `schema.runtime.json` emits:
    // RuntimeSchemaDescriptor v1.
    let descriptor_bytes = br#"{"version":1,"collections":{"users":{"fields":{"id":{"type":"id","idPrefix":"usr"},"email":{"type":"string"}},"options":{"softDelete":false,"versioning":false,"strictness":"strict"},"indexes":[]}}}"#.to_vec();
    let descriptor_hash = sha256_hex(&descriptor_bytes);

    let mut manifest = base_manifest();
    manifest.runtime_descriptor = Some(RuntimeDescriptorEntry {
        hash: descriptor_hash.clone(),
    });

    let archive = pack(&manifest, &[(descriptor_hash.clone(), descriptor_bytes.clone())]);

    let store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(tmpdir()).expect("local store"));
    let app_id = Uuid::now_v7();

    let success = ingest(&store, &app_id, &archive)
        .await
        .expect("ingest must accept a manifest carrying a runtime_descriptor");

    // The re-serialized manifest still carries the descriptor entry verbatim.
    let stored: Manifest = serde_json::from_str(&success.manifest_json).unwrap();
    assert_eq!(
        stored.runtime_descriptor,
        Some(RuntimeDescriptorEntry {
            hash: descriptor_hash.clone()
        }),
        "ingest must round-trip the runtime_descriptor entry"
    );

    // And the descriptor blob is retrievable byte-identical from the store.
    let fetched = store.get_blob(&descriptor_hash).await.expect("blob present");
    assert_eq!(
        fetched.as_ref(),
        descriptor_bytes.as_slice(),
        "the schema.runtime.json blob must round-trip byte-identical"
    );
}

#[compio::test]
async fn absent_descriptor_ingests_to_none() {
    // No descriptor: pack + ingest must succeed with the slot left None (an app
    // may ship no schema).
    let manifest = base_manifest();
    assert!(manifest.runtime_descriptor.is_none());

    let archive = pack(&manifest, &[]);

    let store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(tmpdir()).expect("local store"));
    let app_id = Uuid::now_v7();

    let success = ingest(&store, &app_id, &archive)
        .await
        .expect("ingest must succeed with no descriptor");
    let stored: Manifest = serde_json::from_str(&success.manifest_json).unwrap();
    assert!(
        stored.runtime_descriptor.is_none(),
        "absent descriptor must remain None after ingest"
    );
}

#[compio::test]
async fn legacy_manifest_migrations_key_is_rejected() {
    let body = br#"{"ir_version":1,"name":"legacy","ops":[]}"#.to_vec();
    let body_hash = sha256_hex(&body);
    let manifest_json = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0,
        "sourcemaps": {},
        "metadata": { "built_at": "2026-06-25T00:00:00Z" },
        "migrations": [
            { "name": "20260625000000_legacy.ir.json", "hash": body_hash.clone() }
        ]
    }))
    .unwrap();
    let archive = pack_raw_manifest(&manifest_json, &[(body_hash, body)]);

    let store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(tmpdir()).expect("local store"));
    let app_id = Uuid::now_v7();

    let err = ingest(&store, &app_id, &archive)
        .await
        .expect_err("legacy manifest.migrations must be rejected");
    match err {
        IngestError::BadRequest { error, detail } => {
            assert_eq!(error, "invalid manifest");
            assert!(
                detail.contains("manifest.migrations") && detail.contains("migration service"),
                "legacy manifest error should point at migration service, got {detail}"
            );
        }
        other => panic!("expected bad manifest error, got {other:?}"),
    }
}

#[compio::test]
async fn descriptor_blob_missing_from_tar_is_rejected() {
    // The manifest references a descriptor hash but the tar carries no matching
    // blob — ingest must 400 (collect_expected_hashes gathered the descriptor
    // hash; step 8 finds it absent). This proves the new slot is wired into the
    // missing-blob assertion, not silently ignored.
    let descriptor_hash = "9".repeat(64);
    let mut manifest = base_manifest();
    manifest.runtime_descriptor = Some(RuntimeDescriptorEntry {
        hash: descriptor_hash.clone(),
    });

    let archive = pack(&manifest, &[]); // no blob staged

    let store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(tmpdir()).expect("local store"));
    let app_id = Uuid::now_v7();

    let err = ingest(&store, &app_id, &archive)
        .await
        .expect_err("a referenced-but-absent descriptor blob must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains(&descriptor_hash) && msg.to_lowercase().contains("blob"),
        "error must name the missing descriptor blob, got {msg}"
    );
}
