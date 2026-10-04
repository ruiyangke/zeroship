use zeroship_kv::backend::{Backend, TtlState};

async fn delete_contract(backend: &dyn Backend, app: &str, other: &str) {
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
    delete_contract(&backend, "delete-contract", "delete-contract-neighbor").await;
}

/// The shared Redis and Dragonfly servers run every test process of the
/// worktree at once, so each run of the contract mints its own pair of app
/// ids rather than using a literal: a literal could collide with a
/// concurrent case's.
#[cfg(feature = "redis")]
#[compio::test]
async fn redis_delete_treats_expired_keys_as_absent() {
    let fixtures = crate::support::fixtures();
    for config in [fixtures.redis_config(), fixtures.cluster_config()] {
        let app = crate::support::case_prefix();
        let other = crate::support::case_prefix();
        delete_contract(&zeroship_kv::backend::Redis::new(config), &app, &other).await;
    }
}
