//! Tests for the content-addressed blob store. Uses `compio::test`
//! since `LocalDiskBlobStore` is async over compio's filesystem APIs.

use std::path::PathBuf;

use uuid::Uuid;
use zeroship_bundle::blob::{
    sha256_hex, validate_hash_format, BlobError, BlobStore, LocalDiskBlobStore, PutOutcome,
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

// ---------------------------------------------------------------------------
// Streaming put — `put_blob_stream` path
// ---------------------------------------------------------------------------

#[compio::test]
async fn local_disk_put_blob_stream_happy_path() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"hello, streaming blob world!";
    let hash = sha256_hex(data);

    let mut cursor = std::io::Cursor::new(&data[..]);
    let outcome = store
        .put_blob_stream(&hash, data.len() as u64, &mut cursor)
        .await
        .expect("stream put ok");
    assert_eq!(outcome, PutOutcome::Wrote);
    assert!(store.has_blob(&hash).await.unwrap());
    let got = store.get_blob(&hash).await.unwrap();
    assert_eq!(got.as_ref(), data);

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_stream_idempotent_returns_deduped() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"dedup me please";
    let hash = sha256_hex(data);

    // First put: fresh write.
    let mut c1 = std::io::Cursor::new(&data[..]);
    let r1 = store
        .put_blob_stream(&hash, data.len() as u64, &mut c1)
        .await
        .expect("first put");
    assert_eq!(r1, PutOutcome::Wrote);

    // Second put: pre-existing → reader is drained, outcome is Deduped.
    let mut c2 = std::io::Cursor::new(&data[..]);
    let r2 = store
        .put_blob_stream(&hash, data.len() as u64, &mut c2)
        .await
        .expect("second put");
    assert_eq!(r2, PutOutcome::Deduped);
    // The reader should be fully consumed (cursor advanced past end).
    assert_eq!(
        c2.position(),
        data.len() as u64,
        "dedup path must drain the reader"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_stream_rejects_hash_mismatch() {
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let data = b"actual content";
    let bogus_hash = "a".repeat(64);
    let mut cursor = std::io::Cursor::new(&data[..]);
    let err = store
        .put_blob_stream(&bogus_hash, data.len() as u64, &mut cursor)
        .await
        .unwrap_err();
    match err {
        BlobError::HashMismatch { expected, got } => {
            assert_eq!(expected, bogus_hash);
            assert_eq!(got, sha256_hex(data));
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    // Tmp file must be cleaned up; final blob must not exist.
    assert!(!store.has_blob(&bogus_hash).await.unwrap());
    let blob_dir = root.join("blobs").join(&bogus_hash[..2]);
    if blob_dir.exists() {
        // No leftover tmp-* files in the shard directory.
        for entry in std::fs::read_dir(&blob_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            assert!(
                !name_str.starts_with(&format!("{}.tmp-", &bogus_hash[2..])),
                "leftover tmp file: {name_str}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_stream_rejects_size_overflow() {
    // Reader yields more bytes than declared → reject before we hash
    // past the cap.
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let actual = b"this is more than declared";
    // Lie: claim only 5 bytes.
    let declared: u64 = 5;
    // Compute the hash of the first 5 bytes (so hash format is fine);
    // size enforcement is what we're exercising here.
    let claimed_hash = sha256_hex(&actual[..declared as usize]);

    let mut cursor = std::io::Cursor::new(&actual[..]);
    let err = store
        .put_blob_stream(&claimed_hash, declared, &mut cursor)
        .await
        .unwrap_err();
    match err {
        BlobError::Backend(msg) => assert!(
            msg.contains("exceeds declared size"),
            "got backend error: {msg}"
        ),
        other => panic!("expected Backend size error, got {other:?}"),
    }
    assert!(!store.has_blob(&claimed_hash).await.unwrap());

    let _ = std::fs::remove_dir_all(&root);
}

#[compio::test]
async fn local_disk_put_blob_stream_rejects_size_underflow() {
    // Reader hits EOF before the declared size → reject.
    let root = tmpdir();
    let store = LocalDiskBlobStore::new(root.clone()).unwrap();

    let actual = b"short";
    let declared: u64 = 1024; // overstate
    let h = sha256_hex(actual);

    let mut cursor = std::io::Cursor::new(&actual[..]);
    let err = store
        .put_blob_stream(&h, declared, &mut cursor)
        .await
        .unwrap_err();
    match err {
        BlobError::Backend(msg) => assert!(msg.contains("size mismatch"), "got: {msg}"),
        other => panic!("expected Backend size mismatch, got {other:?}"),
    }
    assert!(!store.has_blob(&h).await.unwrap());

    let _ = std::fs::remove_dir_all(&root);
}
