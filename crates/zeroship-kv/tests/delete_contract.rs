use zeroship_kv::backend::{Backend, TtlState};

#[cfg(feature = "redis")]
mod support;

async fn delete_contract(backend: &dyn Backend) {
    let app = "delete-contract";
    let other = "delete-contract-neighbor";
    for read_before_delete in [false, true] {
        backend.set(app, "key", "expired", None).await.unwrap();
        backend.set(other, "key", "live", None).await.unwrap();
        assert!(backend.expire(app, "key", 0).await.unwrap());
        if read_before_delete {
            assert_eq!(backend.get(app, "key").await.unwrap(), None);
            assert_eq!(backend.ttl(app, "key").await.unwrap(), TtlState::Missing);
        }
        assert!(!backend.delete(app, "key").await.unwrap());
        assert!(!backend.delete(app, "key").await.unwrap());
        assert_eq!(
            backend.get(other, "key").await.unwrap().as_deref(),
            Some("live")
        );
        assert!(backend.delete(other, "key").await.unwrap());
    }
    for ttl in [None, Some(60_000)] {
        backend.set(app, "key", "live", ttl).await.unwrap();
        assert!(backend.delete(app, "key").await.unwrap());
        assert_eq!(backend.get(app, "key").await.unwrap(), None);
        assert!(!backend.delete(app, "key").await.unwrap());
    }
}

#[cfg(feature = "redb")]
#[compio::test]
async fn redb_delete_treats_expired_keys_as_absent() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        zeroship_kv::backend::RedbBackend::open(directory.path().join("kv.redb")).unwrap();
    delete_contract(&backend).await;
}

#[cfg(feature = "redis")]
#[compio::test]
async fn redis_delete_treats_expired_keys_as_absent() {
    let fixtures = support::fixtures();
    for config in [fixtures.redis_config(), fixtures.cluster_config()] {
        delete_contract(&zeroship_kv::backend::Redis::new(config)).await;
    }
}
