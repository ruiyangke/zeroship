//! Integration tests for the Redis backend — exercises the Backend
//! trait impl against a live Redis AND a live 3-node Dragonfly cluster.
//!
//! Single-node: set `REDIS_TEST_URL=redis://127.0.0.1:6379`
//! Cluster:     set `DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002'`

#![cfg(feature = "redis")]

use zeroship_plugin_kv::backend::{Backend, Redis};

fn redis_url() -> Option<String> { std::env::var("REDIS_TEST_URL").ok() }

fn cluster_url() -> Option<String> {
    let seeds = std::env::var("DRAGONFLY_CLUSTER_SEEDS").ok()?;
    let mut it = seeds.split(',');
    let first = it.next()?.trim().to_string();
    // Build the "plugin-kv" cluster URL: cluster=true + seeds=... with
    // the base URL pointing at the first seed.
    Some(format!("{first}?cluster=true&seeds={seeds}"))
}

#[compio::test]
async fn single_node_roundtrip() {
    let Some(url) = redis_url() else {
        eprintln!("skip: REDIS_TEST_URL not set");
        return;
    };
    let b = Redis::new(url);
    let app = "kv-test-single";

    b.delete(app, "k1").await.ok();
    assert!(b.get(app, "k1").await.unwrap().is_none());

    b.set(app, "k1", "hello", None).await.unwrap();
    assert_eq!(b.get(app, "k1").await.unwrap().as_deref(), Some("hello"));

    let n = b.incr(app, "counter", 5).await.unwrap();
    assert_eq!(n, 5);
    let n = b.incr(app, "counter", -3).await.unwrap();
    assert_eq!(n, 2);

    b.delete(app, "k1").await.unwrap();
    b.delete(app, "counter").await.ok();
}

#[compio::test]
async fn cluster_roundtrip_via_backend() {
    let Some(url) = cluster_url() else {
        eprintln!("skip: DRAGONFLY_CLUSTER_SEEDS not set");
        return;
    };
    let b = Redis::new(url);
    let app = "kv-test-cluster";

    b.delete(app, "k1").await.ok();
    b.delete(app, "counter").await.ok();

    b.set(app, "k1", "cluster-hello", None).await.unwrap();
    assert_eq!(b.get(app, "k1").await.unwrap().as_deref(), Some("cluster-hello"));

    // Atomic INCR across routed calls.
    for expected in 1..=10 {
        let n = b.incr(app, "counter", 1).await.unwrap();
        assert_eq!(n, expected);
    }

    // SCAN via list() — hash-tag keeps all keys on one node.
    for i in 0..3 {
        b.set(app, &format!("item:{i}"), "x", None).await.unwrap();
    }
    let mut items = b.list(app, "item:").await.unwrap();
    items.sort();
    assert_eq!(items, vec!["item:0", "item:1", "item:2"]);

    // Cleanup.
    b.delete(app, "k1").await.ok();
    b.delete(app, "counter").await.ok();
    for i in 0..3 { b.delete(app, &format!("item:{i}")).await.ok(); }
}

#[compio::test]
async fn ttl_expires_in_cluster_mode() {
    let Some(url) = cluster_url() else { return; };
    let b = Redis::new(url);
    let app = "kv-test-cluster-ttl";

    b.delete(app, "bye").await.ok();
    b.set(app, "bye", "v", Some(200)).await.unwrap();
    assert_eq!(b.get(app, "bye").await.unwrap().as_deref(), Some("v"));
    compio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(b.get(app, "bye").await.unwrap().is_none());
}
