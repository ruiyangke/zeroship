//! Integration tests for the `.zship` ingest path.
//!
//! These tests exercise `zeroship_control::deploy::ingest` directly
//! against an on-disk `LocalDiskBlobStore` (under a tmpdir). The DB
//! step is exercised end-to-end against a real Postgres only when
//! `CONTROL_TEST_DB` is set — otherwise the registry-side asserts
//! are skipped silently, matching the pattern in `env_store.rs`.
//!
//! The deploy pipeline is structured so its core (`ingest`) is a pure
//! function over `(blob_store, app_id, compressed_bytes) -> result`.
//! That keeps the rejection-path tests fast and DB-free.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_bundle::{
    AssetEntry, BlobStore, LocalDiskBlobStore, Manifest, ManifestMetadata, WorkerCode,
};
use zeroship_control::deploy::{self, IngestError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zship-test-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mkdir tmp");
    p
}

fn store(root: &PathBuf) -> Arc<dyn BlobStore> {
    Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Build a minimal valid manifest referencing `assets` + an optional
/// worker bundle, with `built_at` set to a fixed RFC 3339 string.
fn manifest_for(
    worker_hash: Option<&str>,
    assets: &[(&str, &str, &str)], // (path, hash, content_type)
) -> Manifest {
    let mut a: HashMap<String, AssetEntry> = HashMap::new();
    for (p, h, ct) in assets {
        a.insert(
            (*p).to_string(),
            AssetEntry {
                hash: (*h).to_string(),
                content_type: (*ct).to_string(),
                size: 0,
                cache: None,
                updated_at: 0,
                variants: HashMap::new(),
            },
        );
    }
    Manifest {
        version: 1,
        deploy_hash: None,
        worker: worker_hash.map(|h| WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), h.to_string())]),
        }),
        resources: HashMap::new(),
        schemas: HashMap::new(),
        aliases: HashMap::new(),
        transformer: None,
        assets: a,
        runtime_assets: HashMap::new(),
        asset_version: 0,
        sourcemaps: HashMap::new(),
        metadata: ManifestMetadata {
            compiler: Some("test".into()),
            built_at: "2026-04-29T00:00:00Z".into(),
        },
        exports: None,
    }
}

/// Pack `(name, bytes)` entries into a tar archive, then zstd-compress
/// the whole thing. Manifest goes first if `manifest_first`; otherwise
/// the entries are appended in the order given.
fn build_zship(
    manifest_bytes: &[u8],
    blobs: &[(String, Vec<u8>)], // (hash, raw bytes)
    manifest_first: bool,
) -> Vec<u8> {
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let write_manifest = |b: &mut tar::Builder<&mut Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest_bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            b.append_data(&mut header, "manifest.json", manifest_bytes)
                .expect("append manifest");
        };
        let write_blobs = |b: &mut tar::Builder<&mut Vec<u8>>| {
            for (hash, bytes) in blobs {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                b.append_data(
                    &mut header,
                    format!("blobs/{hash}"),
                    bytes.as_slice(),
                )
                .expect("append blob");
            }
        };
        if manifest_first {
            write_manifest(&mut builder);
            write_blobs(&mut builder);
        } else {
            write_blobs(&mut builder);
            write_manifest(&mut builder);
        }
        builder.finish().expect("tar finish");
    }
    let mut compressed: Vec<u8> = Vec::new();
    {
        let mut enc = zstd::Encoder::new(&mut compressed, 0).expect("zstd enc");
        enc.write_all(&tar_buf).expect("zstd write");
        enc.finish().expect("zstd finish");
    }
    compressed
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_round_trip() {
    let root = tmpdir();
    let bs = store(&root);

    // One asset + one server bundle.
    let html = b"<!doctype html><body>hello</body>";
    let html_hash = sha256_hex(html);
    let server = b"export default { fetch() { return new Response('ok'); } }";
    let server_hash = sha256_hex(server);

    let manifest = manifest_for(
        Some(&server_hash),
        &[("/index.html", &html_hash, "text/html")],
    );
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    let blobs = vec![
        (html_hash.clone(), html.to_vec()),
        (server_hash.clone(), server.to_vec()),
    ];
    let body = build_zship(&manifest_bytes, &blobs, true);

    let app_id = Uuid::new_v4();
    let success = deploy::ingest(&bs, &app_id, &body).await.expect("ingest ok");

    assert_eq!(success.blobs_uploaded, 2);
    assert_eq!(success.blobs_deduped, 0);
    assert_eq!(success.deploy_hash.len(), 64);

    // Both blobs and the manifest are persisted.
    assert!(bs.has_blob(&html_hash).await.unwrap());
    assert!(bs.has_blob(&server_hash).await.unwrap());
    let stored_manifest = bs
        .get_manifest(&app_id, &success.deploy_hash)
        .await
        .expect("manifest stored");
    let parsed: Manifest = serde_json::from_slice(&stored_manifest).unwrap();
    assert_eq!(parsed.deploy_hash.as_deref(), Some(success.deploy_hash.as_str()));
    assert_eq!(
        parsed.worker,
        Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), server_hash.clone())]),
        })
    );

    // DB-side assert (only when CONTROL_TEST_DB is set).
    if let Some(url) = db_url() {
        use zeroship_control::Registry;
        let registry = Registry::new(&url).await.expect("registry");
        let name = format!("test-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let record = registry.create_app(&name, "free").await.expect("create");
        let app_id2 = record.id;

        // Re-run ingest under the real app id, then update the DB.
        let success2 = deploy::ingest(&bs, &app_id2, &body)
            .await
            .expect("ingest2");
        let updated = registry
            .set_deploy_with_manifest(
                &app_id2,
                &success2.deploy_hash,
                &success2.manifest_json,
            )
            .await
            .expect("set deploy");
        assert!(updated);

        let row = registry.get_app(&app_id2).await.expect("get_app").unwrap();
        assert_eq!(row.deploy_hash.as_deref(), Some(success2.deploy_hash.as_str()));
        registry.delete_app(&app_id2).await.ok();
    }

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Reject paths — DB-free
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_rejects_hash_mismatch() {
    let root = tmpdir();
    let bs = store(&root);

    // Real bytes, fake filename: tar entry called blobs/<wrong_hash>.
    let bytes = b"actual content";
    let real_hash = sha256_hex(bytes);
    let wrong_hash = "0".repeat(64);
    assert_ne!(real_hash, wrong_hash);

    let manifest = manifest_for(None, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(wrong_hash.clone(), bytes.to_vec())];
    let body = build_zship(&manifest_bytes, &blobs, true);

    let result = deploy::ingest(&bs, &Uuid::new_v4(), &body).await;
    match result {
        Err(IngestError::BadRequest { error, .. }) => {
            assert!(error.contains("hash mismatch"), "got error={error}");
        }
        other => panic!("expected BadRequest hash mismatch, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn deploy_rejects_missing_blob() {
    let root = tmpdir();
    let bs = store(&root);

    // Manifest claims an asset whose blob isn't in the tar.
    let phantom_hash = "1".repeat(64);
    let manifest = manifest_for(None, &[("/index.html", &phantom_hash, "text/html")]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let body = build_zship(&manifest_bytes, &[], true);

    let result = deploy::ingest(&bs, &Uuid::new_v4(), &body).await;
    match result {
        Err(IngestError::BadRequest { error, .. }) => {
            assert!(error.contains("missing blob"), "got error={error}");
        }
        other => panic!("expected BadRequest missing blob, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn deploy_rejects_manifest_not_first() {
    let root = tmpdir();
    let bs = store(&root);

    let bytes = b"some payload";
    let hash = sha256_hex(bytes);
    let manifest = manifest_for(None, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(hash, bytes.to_vec())];
    // manifest_first = false → blob entry comes before manifest.json.
    let body = build_zship(&manifest_bytes, &blobs, false);

    let result = deploy::ingest(&bs, &Uuid::new_v4(), &body).await;
    match result {
        Err(IngestError::BadRequest { error, .. }) => {
            assert!(
                error.contains("manifest must be first"),
                "got error={error}"
            );
        }
        other => panic!("expected BadRequest manifest-first, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn deploy_rejects_unsupported_version() {
    let root = tmpdir();
    let bs = store(&root);

    // Manually build a JSON manifest with version=99 (typed Manifest
    // has version: u16 so we can serialize via serde_json::Value).
    let raw = serde_json::json!({
        "version": 99,
        "rules": [],
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0,
        "sourcemaps": {},
        "metadata": { "built_at": "2026-04-29T00:00:00Z" }
    });
    let manifest_bytes = serde_json::to_vec(&raw).unwrap();
    let body = build_zship(&manifest_bytes, &[], true);

    let result = deploy::ingest(&bs, &Uuid::new_v4(), &body).await;
    match result {
        Err(IngestError::BadRequest { error, .. }) => {
            assert!(
                error.contains("unsupported manifest version"),
                "got error={error}"
            );
        }
        other => panic!("expected BadRequest unsupported version, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn deploy_dedup_internal() {
    let root = tmpdir();
    let bs = store(&root);

    // Same blob in both deploys. Deploy A: fresh upload. Deploy B: should
    // see has_blob -> true and report blobs_deduped > 0.
    let payload = b"shared content across deploys";
    let hash = sha256_hex(payload);
    let manifest = manifest_for(None, &[("/file.txt", &hash, "text/plain")]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let blobs = vec![(hash.clone(), payload.to_vec())];
    let body = build_zship(&manifest_bytes, &blobs, true);

    let app_a = Uuid::new_v4();
    let success_a = deploy::ingest(&bs, &app_a, &body).await.expect("A");
    assert_eq!(success_a.blobs_uploaded, 1);
    assert_eq!(success_a.blobs_deduped, 0);

    let app_b = Uuid::new_v4();
    let success_b = deploy::ingest(&bs, &app_b, &body).await.expect("B");
    assert_eq!(success_b.blobs_uploaded, 0);
    assert_eq!(success_b.blobs_deduped, 1);

    // The blob lives at exactly one on-disk path (sharded layout).
    let shard = &hash[..2];
    let rest = &hash[2..];
    let blob_path = root.join("blobs").join(shard).join(rest);
    assert!(blob_path.is_file(), "blob present at {blob_path:?}");
    assert_eq!(
        std::fs::read(&blob_path).unwrap(),
        payload,
        "blob bytes round-trip"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Master-key auth
// ---------------------------------------------------------------------------

/// 401 contract for bad master keys. The auth check itself lives in
/// `api::check_admin_auth` (pub(crate)) — we exercise the underlying
/// `validate_control_key` helper here, which is what the deploy
/// handler ultimately defers to. End-to-end coverage of the 401 wire
/// behavior lives in `tests/e2e_platform.sh`.
#[test]
fn deploy_rejects_invalid_master_key() {
    let configured = "real-master-key-deadbeef";
    // Wrong bearer → reject.
    assert!(!zeroship_core::auth::validate_control_key(
        "wrong-key",
        configured,
    ));
    // Right bearer → accept.
    assert!(zeroship_core::auth::validate_control_key(
        configured,
        configured,
    ));
}
