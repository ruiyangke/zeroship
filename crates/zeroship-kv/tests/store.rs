use zeroship_kv::{KvConfig, KvError, KvStore, Namespace};

#[cfg(feature = "redis")]
mod support;

#[test]
fn namespaces_cannot_change_the_key_grammar_or_impersonate_platform_scopes() {
    assert_ne!(
        Namespace::app("control").unwrap(),
        Namespace::platform("control").unwrap()
    );
    assert_eq!(
        Namespace::app("app_example").unwrap().as_str(),
        "app_example"
    );
    for invalid in [
        "",
        "platform:control",
        "app}:key",
        "{app",
        "*",
        "a?b",
        "[ab]",
        "a\\b",
        "a\nb",
        "é",
    ] {
        assert!(Namespace::app(invalid).is_err(), "accepted {invalid:?}");
        assert!(
            Namespace::platform(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn config_debug_does_not_disclose_connection_credentials() {
    let config = KvConfig::Redis {
        url: "redis://user:secret@127.0.0.1?seeds=redis://user:seed-secret@127.0.0.1".into(),
    };
    let debug = format!("{config:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("127.0.0.1"));
    assert_eq!(config.kind(), "redis");
}

#[cfg(feature = "redis")]
#[test]
fn invalid_redis_configuration_fails_without_echoing_credentials() {
    for url in ["", "not a URL", "https://user:secret@127.0.0.1", "redis://"] {
        let config = KvConfig::Redis { url: url.into() };
        let error = KvStore::open(&config).unwrap_err();
        assert!(matches!(error, KvError::InvalidArgument { .. }));
        assert!(!error.to_string().contains("secret"));
    }
    let store = KvStore::open(&KvConfig::Redis {
        url: "redis://user:secret@127.0.0.1".into(),
    })
    .unwrap();
    let kv = store.namespace(Namespace::platform("control").unwrap());
    assert!(!format!("{store:?} {kv:?}").contains("secret"));
}

#[cfg(not(feature = "redis"))]
#[test]
fn configured_redis_requires_its_compiled_implementation() {
    let config = KvConfig::Redis {
        url: "redis://127.0.0.1".into(),
    };
    assert!(matches!(
        KvStore::open(&config),
        Err(KvError::InvalidArgument { .. })
    ));
}

#[cfg(not(feature = "redb"))]
#[test]
fn configured_redb_requires_its_compiled_implementation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unused.redb");
    let config = KvConfig::Redb { path: path.clone() };
    assert!(matches!(
        KvStore::open(&config),
        Err(KvError::InvalidArgument { .. })
    ));
    assert!(!path.exists());
}

#[cfg(any(feature = "redb", feature = "redis"))]
#[allow(
    clippy::future_not_send,
    reason = "Exercises thread-local compio operations."
)]
async fn exercise_store(store: &KvStore) {
    use zeroship_kv::{limits, TtlState};

    let app = store.namespace(Namespace::app("app_example").unwrap());
    let other_app = store.namespace(Namespace::app("app_other").unwrap());
    let platform = store.namespace(Namespace::platform("control").unwrap());
    let other_service = store.namespace(Namespace::platform("auth").unwrap());
    let same_platform = store
        .clone()
        .namespace(Namespace::platform("control").unwrap());

    app.set("shared", "app value", None).await.unwrap();
    platform
        .set("shared", "platform value", Some(60_000))
        .await
        .unwrap();
    assert_eq!(
        same_platform.get("shared").await.unwrap().as_deref(),
        Some("platform value")
    );
    assert_eq!(
        app.get("shared").await.unwrap().as_deref(),
        Some("app value")
    );
    assert_eq!(other_app.get("shared").await.unwrap(), None);
    assert_eq!(other_service.get("shared").await.unwrap(), None);

    // Invalid writes leave the previous value intact across implementations.
    for ttl in [0, limits::MAX_TTL_MS + 1, u64::MAX] {
        assert!(matches!(
            platform.set("shared", "invalid", Some(ttl)).await,
            Err(KvError::InvalidArgument { .. })
        ));
        assert!(platform.incr("counter", 1, Some(ttl)).await.is_err());
        assert!(platform
            .set_if_absent("lease", "invalid", Some(ttl))
            .await
            .is_err());
        assert!(platform.expire("shared", ttl).await.is_err());
    }
    assert!(matches!(
        platform
            .set("shared", &"x".repeat(limits::MAX_VALUE_BYTES + 1), None)
            .await,
        Err(KvError::InvalidValue { .. })
    ));
    assert!(matches!(
        platform.get("{app_example}:shared").await,
        Err(KvError::InvalidKey { .. })
    ));
    assert!(platform.delete("").await.is_err());
    assert!(platform.ttl("").await.is_err());
    assert!(platform.persist("").await.is_err());
    assert_eq!(platform.get("counter").await.unwrap(), None);
    assert_eq!(platform.get("lease").await.unwrap(), None);
    assert_eq!(
        platform.get("shared").await.unwrap().as_deref(),
        Some("platform value")
    );

    assert!(platform
        .set_if_absent("lease", "owner", None)
        .await
        .unwrap());
    assert!(!same_platform
        .set_if_absent("lease", "other", None)
        .await
        .unwrap());
    assert_eq!(platform.incr("counter", 3, None).await.unwrap(), 3);
    assert_eq!(same_platform.incr("counter", -1, None).await.unwrap(), 2);
    assert!(platform.expire("lease", 60_000).await.unwrap());
    assert!(
        matches!(platform.ttl("lease").await.unwrap(), TtlState::ExpiresInMs(ms) if ms <= 60_000)
    );
    assert!(platform.persist("lease").await.unwrap());
    assert_eq!(platform.ttl("lease").await.unwrap(), TtlState::NoExpiry);
    assert!(same_platform.delete("lease").await.unwrap());
    assert_eq!(platform.ttl("lease").await.unwrap(), TtlState::Missing);

    let mut keys = Vec::new();
    let mut cursor = None;
    loop {
        let (page, next) = platform.list("", cursor.as_deref(), 0).await.unwrap();
        keys.extend(page);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    keys.sort();
    keys.dedup();
    assert_eq!(keys, ["counter", "shared"]);
    assert!(other_service
        .list("", None, usize::MAX)
        .await
        .unwrap()
        .0
        .is_empty());
}

#[cfg(feature = "redb")]
#[compio::test]
async fn configured_redb_store_shares_handles_and_preserves_data_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let config = KvConfig::Redb {
        path: dir.path().join("nested/kv.redb"),
    };
    let store = KvStore::open(&config).unwrap();
    exercise_store(&store).await;
    let kv = store.namespace(Namespace::platform("control").unwrap());
    kv.set("persistent", "saved", None).await.unwrap();
    drop(store);
    assert_eq!(
        kv.get("persistent").await.unwrap().as_deref(),
        Some("saved")
    );
    assert!(KvStore::open(&config)
        .unwrap_err()
        .to_string()
        .contains(zeroship_kv::STATE_DIR_LOCK_MARKER));
    drop(kv);
    let reopened = KvStore::open(&config).unwrap();
    let kv = reopened.namespace(Namespace::platform("control").unwrap());
    assert_eq!(
        kv.get("persistent").await.unwrap().as_deref(),
        Some("saved")
    );
}

#[cfg(feature = "redb")]
#[test]
fn failed_embedded_open_does_not_fall_back_to_another_store() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("file");
    std::fs::write(&parent, "not a directory").unwrap();
    let config = KvConfig::Redb {
        path: parent.join("kv.redb"),
    };
    assert!(matches!(
        KvStore::open(&config),
        Err(KvError::Connection { .. })
    ));
}

#[cfg(feature = "redis")]
#[compio::test]
async fn runtime_redis_configuration_runs_the_same_scoped_contract() {
    let fixtures = support::fixtures();
    for url in [fixtures.redis_url(), fixtures.cluster_url()] {
        let config = KvConfig::Redis { url: url.into() };
        let store = KvStore::open(&config).unwrap();
        exercise_store(&store).await;
    }
}
