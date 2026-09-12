use std::sync::Arc;

use bytes::Bytes;
use zeroship_storage::backend::{BoxByteStream, ChunkResult, ChunkSource, ListRequest, OnceChunk};
use zeroship_storage::{
    LocalFs, Namespace, StorageBackendConfig, StorageError, StorageLimits, StorageStore,
};

#[cfg(feature = "s3")]
#[path = "../../../tests/fixtures/s3.rs"]
mod s3_fixture;

async fn drain(mut stream: BoxByteStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    bytes
}

async fn scoped_contract(store: StorageStore) {
    let a = store.namespace(Namespace::app("app_a").unwrap());
    let b = store.namespace(Namespace::app("app_b").unwrap());
    let platform = store.namespace(Namespace::platform("app_a").unwrap());
    for (storage, value) in [(&a, "alpha"), (&b, "beta"), (&platform, "platform")] {
        storage
            .put(
                "uploads",
                "nested/shared",
                value.as_bytes(),
                Some("text/plain"),
            )
            .await
            .unwrap();
    }
    for (storage, value) in [(&a, "alpha"), (&b, "beta"), (&platform, "platform")] {
        let (body, meta) = storage
            .get("uploads", "nested/shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body, value.as_bytes());
        assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
        let (_, stream) = storage
            .get_stream("uploads", "nested/shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drain(stream).await, value.as_bytes());
    }

    b.put("uploads", "b-only", b"private", None).await.unwrap();
    assert!(a.get("uploads", "b-only").await.unwrap().is_none());
    assert!(!a.delete("uploads", "b-only").await.unwrap());
    assert!(b.get("uploads", "b-only").await.unwrap().is_some());
    let page = a
        .list(
            "uploads",
            ListRequest {
                prefix: "",
                cursor: None,
                limit: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        ["nested/shared"]
    );
    assert!(page.cursor.is_none());

    for bad_key in [
        "../app_b/b-only",
        "nested/../../escape",
        "/absolute",
        "a\\b",
        "a\0b",
    ] {
        assert!(matches!(
            a.get("uploads", bad_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.delete("uploads", bad_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.put("uploads", bad_key, b"attack", None).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.get_stream("uploads", bad_key).await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.put_stream(
                "uploads",
                bad_key,
                Box::new(OnceChunk::new(Bytes::new())),
                None
            )
            .await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.list(
                "uploads",
                ListRequest {
                    prefix: bad_key,
                    cursor: None,
                    limit: 1
                }
            )
            .await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.list(
                "uploads",
                ListRequest {
                    prefix: "",
                    cursor: Some(bad_key),
                    limit: 1
                }
            )
            .await,
            Err(StorageError::InvalidArgument(_))
        ));
    }
    for bucket in ["", ".", "..", "../app_b", "a\\b", "a\0b"] {
        assert!(matches!(
            a.get(bucket, "b-only").await,
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            a.list(
                bucket,
                ListRequest {
                    prefix: "",
                    cursor: None,
                    limit: 1
                }
            )
            .await,
            Err(StorageError::InvalidArgument(_))
        ));
    }
    assert!(a.delete("uploads", "nested/shared").await.unwrap());
    assert!(b.get("uploads", "nested/shared").await.unwrap().is_some());
    assert!(platform
        .get("uploads", "nested/shared")
        .await
        .unwrap()
        .is_some());

    let bounded = store
        .with_limits(StorageLimits {
            max_buffered_bytes: 4,
            max_stream_bytes: 6,
        })
        .unwrap()
        .namespace(Namespace::app("bounded").unwrap());
    bounded
        .put("uploads", "object", b"old", None)
        .await
        .unwrap();
    assert!(matches!(
        bounded.put("uploads", "object", b"too large", None).await,
        Err(StorageError::LimitExceeded(_))
    ));
    struct Chunks(std::collections::VecDeque<Bytes>);
    #[async_trait::async_trait(?Send)]
    impl ChunkSource for Chunks {
        async fn next_chunk(&mut self) -> Option<ChunkResult> {
            self.0.pop_front().map(Ok)
        }
    }
    let body = Chunks(
        [
            Bytes::from_static(b"small"),
            Bytes::from_static(b"too large"),
        ]
        .into(),
    );
    assert!(matches!(
        bounded
            .put_stream("uploads", "object", Box::new(body), None)
            .await,
        Err(StorageError::LimitExceeded(_))
    ));
    assert_eq!(
        bounded.get("uploads", "object").await.unwrap().unwrap().0,
        b"old"
    );
    bounded
        .put_stream(
            "uploads",
            "streamed",
            Box::new(OnceChunk::new(Bytes::from_static(b"stream"))),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        bounded.get("uploads", "streamed").await,
        Err(StorageError::LimitExceeded(_))
    ));
    let (_, stream) = bounded
        .get_stream("uploads", "streamed")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(drain(stream).await, b"stream");
}

#[test]
fn local_scoped_storage_contract() {
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::open(&StorageBackendConfig::Local(root.path().to_owned())).unwrap();
    compio::runtime::Runtime::new()
        .unwrap()
        .block_on(scoped_contract(store));
}

#[cfg(feature = "s3")]
#[test]
fn s3_scoped_storage_contract() {
    let minio = s3_fixture::Minio::start();
    let backend = zeroship_storage::S3::with_tuning(
        minio.config("scoped"),
        minio.credentials(),
        zeroship_storage::S3UploadTuning::DEFAULTS,
    );
    let store = StorageStore::from_backend(Arc::new(backend));
    compio::runtime::Runtime::new()
        .unwrap()
        .block_on(scoped_contract(store));
}

#[test]
fn namespaces_cannot_impersonate_platform_or_escape_paths() {
    for name in [
        "",
        ".",
        "..",
        "a..b",
        "a/b",
        "a\\b",
        "a\0b",
        "platform:control",
    ] {
        assert!(matches!(
            Namespace::app(name),
            Err(StorageError::InvalidArgument(_))
        ));
        assert!(matches!(
            Namespace::platform(name),
            Err(StorageError::InvalidArgument(_))
        ));
    }
    assert_ne!(
        Namespace::app("control").unwrap(),
        Namespace::platform("control").unwrap()
    );
}

#[test]
fn code_configuration_rejects_zero_limits() {
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(root.path())));
    assert!(store
        .with_limits(StorageLimits {
            max_buffered_bytes: 0,
            max_stream_bytes: 1
        })
        .is_err());
}
