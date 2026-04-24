//! Cluster integration tests against a live 3-node Dragonfly.
//!
//! Bring up the cluster first:
//!   docker compose -f docker-compose.cluster.yml up -d
//!   ./scripts/bootstrap-dragonfly-cluster.sh
//!
//! Then:
//!   DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
//!     cargo test -p compio-redis --test cluster -- --nocapture

use compio_redis::ClusterClient;

fn seeds() -> Option<Vec<String>> {
    std::env::var("DRAGONFLY_CLUSTER_SEEDS").ok().map(|s|
        s.split(',').map(|x| x.trim().to_string()).collect())
}

fn seeds_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

#[compio::test]
async fn connect_and_roundtrip() {
    let Some(s) = seeds() else {
        eprintln!("skip: DRAGONFLY_CLUSTER_SEEDS not set");
        return;
    };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    cc.set("zs:cluster:k1", b"hello", None).await.expect("set");
    let v = cc.get("zs:cluster:k1").await.expect("get");
    assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

    cc.del("zs:cluster:k1").await.expect("del");
    assert!(cc.get("zs:cluster:k1").await.unwrap().is_none());
}

#[compio::test]
async fn hash_tag_isolation_enables_mget() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // All keys share an {app42} hash tag — same slot, same node.
    for i in 0..10 {
        cc.set(&format!("{{app42}}:k{i}"), b"v", None).await.unwrap();
    }
    let values = cc.mget(&["{app42}:k0", "{app42}:k5", "{app42}:k9"])
        .await.expect("mget same slot");
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|v| v.is_some()));

    for i in 0..10 {
        cc.del(&format!("{{app42}}:k{i}")).await.unwrap();
    }
}

#[compio::test]
async fn cross_slot_mget_errors_without_network_call() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // No hash tags — "foo" and "bar" land on different slots.
    let err = cc.mget(&["foo", "bar"]).await.unwrap_err();
    assert!(matches!(err, compio_redis::Error::CrossSlot),
        "expected CrossSlot, got {err:?}");
}

#[compio::test]
async fn atomic_incr_survives_routing() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    cc.del("{appC}:counter").await.ok();
    for expected in 1..=20 {
        let v = cc.incr_by("{appC}:counter", 1).await.unwrap();
        assert_eq!(v, expected);
    }
    cc.del("{appC}:counter").await.ok();
}

#[compio::test]
async fn scan_on_routing_key_returns_matching() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let prefix = "{appS}:item";
    for i in 0..5 {
        cc.set(&format!("{prefix}:{i}"), b"x", None).await.unwrap();
    }

    let mut cursor = String::from("0");
    let mut found = Vec::new();
    loop {
        let (next, keys) = cc.scan("{appS}", &cursor, &format!("{prefix}:*"), 100)
            .await.expect("scan");
        found.extend(keys);
        if next == "0" { break; }
        cursor = next;
    }
    found.sort();
    assert_eq!(found.len(), 5, "got {found:?}");

    for i in 0..5 {
        cc.del(&format!("{prefix}:{i}")).await.unwrap();
    }
}

#[compio::test]
async fn ttl_lifecycle_on_cluster() {
    let Some(s) = seeds() else { return; };
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = "{appT}:ttl";
    cc.del(key).await.ok();

    // Set with short TTL, verify PTTL is positive, wait for expiry.
    cc.set(key, b"bye", Some(200)).await.unwrap();
    let remaining = cc.pttl(key).await.unwrap();
    assert!(remaining > 0 && remaining <= 200, "pttl={remaining}");
    compio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(cc.get(key).await.unwrap().is_none());
    assert_eq!(cc.pttl(key).await.unwrap(), -2);
}
