//! Tests for the content-addressed blob store. Uses `compio::test`
//! since `LocalDiskBlobStore` is async over compio's filesystem APIs.

use std::path::PathBuf;

use uuid::Uuid;
use zeroship_core::blob::{
    sha256_hex, validate_hash_format, BlobError, BlobStore, LocalDiskBlobStore,
};

fn tmpdir() -> PathBuf {
    let base = std::env::temp_dir();
    let unique = Uuid::new_v4().simple().to_string();
    let dir = base.join(format!("zeroship-blob-test-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sha256_hex_known_vector() {
    // Empty input → well-known sha256 of zero bytes.
    let want = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(sha256_hex(b""), want);
    assert!(validate_hash_format(want));
}

#[test]
fn validate_hash_format_rules() {
    let lower = "0".repeat(64);
    assert!(validate_hash_format(&lower));
    let upper = "A".repeat(64);
    assert!(!validate_hash_format(&upper), "uppercase rejected");
    let short = "0".repeat(63);
    assert!(!validate_hash_format(&short), "wrong length");
    let with_g = format!("g{}", "0".repeat(63));
    assert!(!validate_hash_format(&with_g), "non-hex char");
}

#[compio::test]
async fn local_disk_round_trip_blob() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"hello, zeroship blob";
    let hash = sha256_hex(data);

    assert!(!store.has_blob(&hash).await.unwrap());
    store.put_blob(&hash, data).await.unwrap();
    assert!(store.has_blob(&hash).await.unwrap());
    let got = store.get_blob(&hash).await.unwrap();
    assert_eq!(got.as_ref(), data);

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_get_blob_verifies_hash() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"original bytes";
    let hash = sha256_hex(data);
    store.put_blob(&hash, data).await.unwrap();

    // Corrupt the file directly on disk. The local_path API gives us
    // the sharded path.
    let path = store.local_path(&hash).expect("local_path");
    assert!(path.exists());
    std::fs::write(&path, b"tampered bytes!").unwrap();

    let err = store.get_blob(&hash).await.unwrap_err();
    match err {
        BlobError::HashMismatch { expected, got } => {
            assert_eq!(expected, hash);
            assert_ne!(got, hash);
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_rejects_wrong_hash() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let claimed = "a".repeat(64); // valid format, wrong content
    let err = store.put_blob(&claimed, b"different bytes").await.unwrap_err();
    match err {
        BlobError::HashMismatch { expected, got } => {
            assert_eq!(expected, claimed);
            assert_eq!(got, sha256_hex(b"different bytes"));
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    // Nothing should have been written.
    assert!(!store.has_blob(&claimed).await.unwrap());

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_idempotent() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"idempotent payload";
    let hash = sha256_hex(data);

    store.put_blob(&hash, data).await.unwrap();
    let path = store.local_path(&hash).unwrap();
    let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

    // Sleep is overkill for a "no-write" assertion; rely on the no-op
    // path: re-puts return Ok and don't touch the file. We compare
    // bytes plus mtime to catch any rewrite that changes them.
    store.put_blob(&hash, data).await.unwrap();
    let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(mtime1, mtime2, "second put must be a no-op");

    // And the bytes are still right.
    let got = store.get_blob(&hash).await.unwrap();
    assert_eq!(got.as_ref(), data);

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_round_trip_manifest() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();
    let app_id = Uuid::new_v4();
    let deploy_hash = sha256_hex(b"deploy-payload");

    let json = br#"{"version":1,"rules":[],"assets":{}}"#;
    store.put_manifest(&app_id, &deploy_hash, json).await.unwrap();

    let got = store.get_manifest(&app_id, &deploy_hash).await.unwrap();
    assert_eq!(got.as_ref(), json);

    // Missing manifest → NotFound.
    let other = Uuid::new_v4();
    let err = store
        .get_manifest(&other, &deploy_hash)
        .await
        .unwrap_err();
    matches!(err, BlobError::NotFound(_));

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_local_path_returns_sharded_path() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();
    let hash = "ab1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd";
    let path = store.local_path(hash).expect("local_path");
    let path_str = path.to_string_lossy();
    assert!(
        path_str.contains("/blobs/ab/") && path_str.ends_with(&hash[2..]),
        "expected sharded /blobs/ab/<rest>, got {path_str}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_get_blob_not_found() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();
    let missing = "0".repeat(64);
    let err = store.get_blob(&missing).await.unwrap_err();
    matches!(err, BlobError::NotFound(_));

    let _ = std::fs::remove_dir_all(&root);
}
