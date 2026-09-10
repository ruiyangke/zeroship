//! Backend contracts against isolated Redis and Dragonfly containers.
//! Run with Cargo and an available Docker daemon; fixtures are created and
//! removed by the tests. Container startup failures fail the suite.

#![cfg(feature = "redis")]

mod support;

use zeroship_kv::backend::{Backend, Redis, TtlState};

#[compio::test]
async fn single_node_roundtrip() {
    let fixtures = support::fixtures();
    let url = fixtures.redis_url();
    let b = Redis::new(url);
    let app = "kv-test-single";

    b.delete(app, "k1").await.ok();
    assert!(b.get(app, "k1").await.unwrap().is_none());

    b.set(app, "k1", "hello", None).await.unwrap();
    assert_eq!(b.get(app, "k1").await.unwrap().as_deref(), Some("hello"));

    let n = b.incr(app, "counter", 5, None).await.unwrap();
    assert_eq!(n, 5);
    let n = b.incr(app, "counter", -3, None).await.unwrap();
    assert_eq!(n, 2);

    b.delete(app, "k1").await.unwrap();
    b.delete(app, "counter").await.ok();
}

/// Drain every page of `list` into a flat `Vec`, following the opaque
/// cursor until it comes back `None`. The Backend's `list` is now
/// paginated; tests that want "all keys under prefix" go through here.
async fn list_all(b: &Redis, app: &str, prefix: &str) -> Vec<String> {
    let mut acc = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (keys, next) = b
            .list(app, prefix, cursor.as_deref(), 1000)
            .await
            .expect("list page");
        acc.extend(keys);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    acc
}

#[compio::test]
async fn cluster_roundtrip_via_backend() {
    let fixtures = support::fixtures();
    let b = Redis::new(fixtures.cluster_url());
    let app = "kv-test-cluster";

    b.delete(app, "k1").await.ok();
    b.delete(app, "counter").await.ok();

    b.set(app, "k1", "cluster-hello", None).await.unwrap();
    assert_eq!(
        b.get(app, "k1").await.unwrap().as_deref(),
        Some("cluster-hello")
    );

    // Atomic INCR across routed calls.
    for expected in 1..=10 {
        let n = b.incr(app, "counter", 1, None).await.unwrap();
        assert_eq!(n, expected);
    }

    // SCAN via list() — hash-tag keeps all keys on one node.
    for i in 0..3 {
        b.set(app, &format!("item:{i}"), "x", None).await.unwrap();
    }
    let mut items = list_all(&b, app, "item:").await;
    items.sort();
    assert_eq!(items, vec!["item:0", "item:1", "item:2"]);

    // Cleanup.
    b.delete(app, "k1").await.ok();
    b.delete(app, "counter").await.ok();
    for i in 0..3 {
        b.delete(app, &format!("item:{i}")).await.ok();
    }
}

#[compio::test]
async fn ttl_expires_in_cluster_mode() {
    let fixtures = support::fixtures();
    let url = fixtures.cluster_url();
    let b = Redis::new(url);
    let app = "kv-test-cluster-ttl";

    b.delete(app, "bye").await.ok();
    b.set(app, "bye", "v", Some(200)).await.unwrap();
    assert_eq!(b.get(app, "bye").await.unwrap().as_deref(), Some("v"));
    compio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(b.get(app, "bye").await.unwrap().is_none());
}

/// Run each contract against both network backends.
async fn for_each_backend<F, Fut>(f: F)
where
    F: Fn(Redis, &'static str) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let fixtures = support::fixtures();
    f(Redis::new(fixtures.redis_url()), "single").await;
    f(Redis::new(fixtures.cluster_url()), "cluster").await;
}

#[compio::test]
async fn list_empty_app_returns_empty_vec() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-empty-app-abc123xyz";
        let got = list_all(&b, app, "").await;
        assert!(got.is_empty(), "[{label}] unexpected keys: {got:?}");
    })
    .await;
}

#[compio::test]
async fn delete_missing_returns_false() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-del-miss";
        b.delete(app, "ghost").await.ok();
        let deleted = b.delete(app, "ghost").await.expect(label);
        assert!(!deleted, "[{label}] deleting missing should return false");
    })
    .await;
}

#[compio::test]
async fn set_overwrites_existing_value() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-overwrite";
        b.delete(app, "k").await.ok();

        b.set(app, "k", "first", None).await.expect(label);
        assert_eq!(
            b.get(app, "k").await.expect(label).as_deref(),
            Some("first")
        );

        b.set(app, "k", "second", None).await.expect(label);
        assert_eq!(
            b.get(app, "k").await.expect(label).as_deref(),
            Some("second")
        );

        b.delete(app, "k").await.ok();
    })
    .await;
}

#[compio::test]
async fn app_isolation_prevents_cross_reads() {
    for_each_backend(|b, label| async move {
        let app_a = "kv-test-iso-app-a";
        let app_b = "kv-test-iso-app-b";

        b.delete(app_a, "shared-name").await.ok();
        b.delete(app_b, "shared-name").await.ok();

        b.set(app_a, "shared-name", "in-a", None)
            .await
            .expect(label);
        b.set(app_b, "shared-name", "in-b", None)
            .await
            .expect(label);

        assert_eq!(
            b.get(app_a, "shared-name").await.expect(label).as_deref(),
            Some("in-a")
        );
        assert_eq!(
            b.get(app_b, "shared-name").await.expect(label).as_deref(),
            Some("in-b")
        );

        // list() must not leak across apps.
        let a_keys = list_all(&b, app_a, "").await;
        let b_keys = list_all(&b, app_b, "").await;
        assert!(
            a_keys.contains(&"shared-name".to_string()),
            "[{label}] a missing key"
        );
        assert!(
            b_keys.contains(&"shared-name".to_string()),
            "[{label}] b missing key"
        );
        // Neither list includes the other app's entries — and `list` strips
        // the `{app_id}:` prefix, so the key name is just "shared-name".
        assert_eq!(a_keys.len(), 1, "[{label}] a has extras: {a_keys:?}");
        assert_eq!(b_keys.len(), 1, "[{label}] b has extras: {b_keys:?}");

        b.delete(app_a, "shared-name").await.ok();
        b.delete(app_b, "shared-name").await.ok();
    })
    .await;
}

#[compio::test]
async fn list_with_prefix_filter() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-prefix";
        for k in ["user:1", "user:2", "user:3", "post:1", "post:2"] {
            b.set(app, k, "x", None).await.expect(label);
        }

        let mut users = list_all(&b, app, "user:").await;
        users.sort();
        assert_eq!(users, vec!["user:1", "user:2", "user:3"], "[{label}]");

        let mut posts = list_all(&b, app, "post:").await;
        posts.sort();
        assert_eq!(posts, vec!["post:1", "post:2"], "[{label}]");

        let none = list_all(&b, app, "nonexistent:").await;
        assert!(none.is_empty(), "[{label}] expected empty, got {none:?}");

        for k in ["user:1", "user:2", "user:3", "post:1", "post:2"] {
            b.delete(app, k).await.ok();
        }
    })
    .await;
}

#[compio::test]
async fn incr_on_existing_numeric_value() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-incr-preset";
        b.delete(app, "counter").await.ok();

        // Seed via set, then incr reads-updates-returns.
        b.set(app, "counter", "100", None).await.expect(label);
        let n = b.incr(app, "counter", 5, None).await.expect(label);
        assert_eq!(n, 105, "[{label}]");
        let n = b.incr(app, "counter", -15, None).await.expect(label);
        assert_eq!(n, 90, "[{label}]");

        b.delete(app, "counter").await.ok();
    })
    .await;
}

#[compio::test]
async fn set_ttl_of_zero_errors_cleanly() {
    // Redis/Dragonfly reject PX 0 with `ERR invalid expire time`.
    // Backend surfaces it as a String error — doesn't panic or hang.
    for_each_backend(|b, label| async move {
        let app = "kv-test-zero-ttl";
        b.delete(app, "k").await.ok();

        let result = b.set(app, "k", "v", Some(0)).await;
        assert!(
            result.is_err(),
            "[{label}] PX 0 should be rejected by server"
        );

        b.delete(app, "k").await.ok();
    })
    .await;
}

#[compio::test]
async fn set_if_absent_is_atomic_lock() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-sia";
        b.delete(app, "lock").await.ok();
        assert!(
            b.set_if_absent(app, "lock", "1", None).await.expect(label),
            "[{label}]"
        );
        assert!(
            !b.set_if_absent(app, "lock", "2", None).await.expect(label),
            "[{label}]"
        );
        assert_eq!(b.get(app, "lock").await.expect(label).as_deref(), Some("1"));
        b.delete(app, "lock").await.ok();
    })
    .await;
}

#[compio::test]
async fn expire_ttl_persist_lifecycle() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-ttl-lifecycle";
        b.delete(app, "k").await.ok();

        // Missing key.
        assert_eq!(b.ttl(app, "k").await.expect(label), TtlState::Missing);
        assert!(
            !b.expire(app, "k", 1000).await.expect(label),
            "[{label}] expire missing"
        );

        // Set without TTL → NoExpiry.
        b.set(app, "k", "v", None).await.expect(label);
        assert_eq!(b.ttl(app, "k").await.expect(label), TtlState::NoExpiry);

        // expire → ExpiresInMs.
        assert!(
            b.expire(app, "k", 100_000).await.expect(label),
            "[{label}] expire set"
        );
        assert!(matches!(
            b.ttl(app, "k").await.expect(label),
            TtlState::ExpiresInMs(_)
        ));

        // persist → back to NoExpiry; second persist is a no-op false.
        assert!(
            b.persist(app, "k").await.expect(label),
            "[{label}] persist removed TTL"
        );
        assert_eq!(b.ttl(app, "k").await.expect(label), TtlState::NoExpiry);
        assert!(
            !b.persist(app, "k").await.expect(label),
            "[{label}] persist no-op"
        );

        b.delete(app, "k").await.ok();
    })
    .await;
}

#[compio::test]
async fn incr_preserves_existing_ttl_via_redis() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-incr-ttl";
        b.delete(app, "c").await.ok();

        // Create with TTL via incr (key created this call).
        assert_eq!(b.incr(app, "c", 1, Some(100_000)).await.expect(label), 1);
        assert!(matches!(
            b.ttl(app, "c").await.expect(label),
            TtlState::ExpiresInMs(_)
        ));

        // Subsequent incr must NOT reset the TTL (fixed-window).
        b.incr(app, "c", 1, Some(50)).await.expect(label);
        match b.ttl(app, "c").await.expect(label) {
            TtlState::ExpiresInMs(ms) => assert!(ms > 1000, "[{label}] TTL was reset: {ms}"),
            other => panic!("[{label}] expected ExpiresInMs, got {other:?}"),
        }

        b.delete(app, "c").await.ok();
    })
    .await;
}

#[compio::test]
async fn incr_on_non_numeric_is_typed_error() {
    for_each_backend(|b, label| async move {
        let app = "kv-test-incr-nonnum";
        b.delete(app, "s").await.ok();
        b.set(app, "s", "hello", None).await.expect(label);
        match b.incr(app, "s", 1, None).await {
            Err(zeroship_kv::KvError::NonNumeric { .. }) => {}
            other => panic!("[{label}] expected NonNumeric, got {other:?}"),
        }
        b.delete(app, "s").await.ok();
    })
    .await;
}

#[compio::test]
async fn reordered_seeds_preserve_data_access() {
    let fixtures = support::fixtures();
    let url = url::Url::parse(fixtures.cluster_url()).unwrap();
    let seeds = url.query_pairs().find(|(key, _)| key == "seeds").unwrap().1;
    let parts: Vec<&str> = seeds.split(',').map(str::trim).collect();
    assert!(parts.len() >= 2, "need >= 2 seeds for this test");

    // Same seed set, opposite order.
    let url_forward = format!("{}?cluster=true&seeds={}", parts[0], parts.join(","));
    let mut reversed = parts.clone();
    reversed.reverse();
    let url_reverse = format!("{}?cluster=true&seeds={}", reversed[0], reversed.join(","));

    let a = Redis::new(url_forward);
    let b = Redis::new(url_reverse);

    let app = "kv-test-share-seeds";
    b.delete(app, "shared").await.ok();

    a.set(app, "shared", "written-via-a", None).await.unwrap();
    assert_eq!(
        b.get(app, "shared").await.unwrap().as_deref(),
        Some("written-via-a"),
        "write via a must be visible to b regardless of seed order"
    );

    b.delete(app, "shared").await.ok();
}
