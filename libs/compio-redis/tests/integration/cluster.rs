//! Live Redis driver tests against the shared Dragonfly cluster.
//! Run with Cargo or nextest; Docker is required.
//!
//! The cluster is shared by every test process of the worktree, so each case
//! mints its own hash tag with `crate::support::case_prefix()` instead of a
//! literal `{app42}`-style tag: a literal could collide with a concurrent
//! case's, the tag keeps a case's keys in one slot, and the server is never
//! flushed or assumed empty.

use compio_redis::ClusterClient;

fn seeds_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

#[compio::test]
async fn connect_and_roundtrip() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:k1", crate::support::case_prefix());
    cc.set(&key, b"hello", None).await.expect("set");
    let v = cc.get(&key).await.expect("get");
    assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

    cc.del(&key).await.expect("del");
    assert!(cc.get(&key).await.unwrap().is_none());
}

#[compio::test]
async fn hash_tag_isolation_enables_mget() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // All keys share one case-minted hash tag: same slot, same node.
    let tag = crate::support::case_prefix();
    for i in 0..10 {
        cc.set(&format!("{{{tag}}}:k{i}"), b"v", None).await.unwrap();
    }
    let keys = [
        format!("{{{tag}}}:k0"),
        format!("{{{tag}}}:k5"),
        format!("{{{tag}}}:k9"),
    ];
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let values = cc.mget(&refs).await.expect("mget same slot");
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|v| v.is_some()));

    for i in 0..10 {
        cc.del(&format!("{{{tag}}}:k{i}")).await.unwrap();
    }
}

#[compio::test]
async fn cross_slot_mget_errors_without_network_call() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    // No hash tags — "foo" and "bar" land on different slots.
    let err = cc.mget(&["foo", "bar"]).await.unwrap_err();
    assert!(
        matches!(err, compio_redis::Error::CrossSlot),
        "expected CrossSlot, got {err:?}"
    );
}

#[compio::test]
async fn atomic_incr_survives_routing() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:counter", crate::support::case_prefix());
    cc.del(&key).await.ok();
    for expected in 1..=20 {
        let v = cc.incr_by(&key, 1).await.unwrap();
        assert_eq!(v, expected);
    }
    cc.del(&key).await.ok();
}

#[compio::test]
async fn scan_on_routing_key_returns_matching() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let tag = crate::support::case_prefix();
    let prefix = format!("{{{tag}}}:item");
    for i in 0..5 {
        cc.set(&format!("{prefix}:{i}"), b"x", None).await.unwrap();
    }

    let mut cursor = String::from("0");
    let mut found = Vec::new();
    loop {
        let (next, keys) = cc
            .scan(&format!("{{{tag}}}"), &cursor, &format!("{prefix}:*"), 100)
            .await
            .expect("scan");
        found.extend(keys);
        if next == "0" {
            break;
        }
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
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:ttl", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // Set with short TTL, verify PTTL is positive, wait for expiry.
    cc.set(&key, b"bye", Some(200)).await.unwrap();
    let remaining = cc.pttl(&key).await.unwrap();
    assert!(remaining > 0 && remaining <= 200, "pttl={remaining}");
    compio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(cc.get(&key).await.unwrap().is_none());
    assert_eq!(cc.pttl(&key).await.unwrap(), -2);
}

// -----------------------------------------------------------------
// Failure-path and edge-case integration tests.
// -----------------------------------------------------------------

#[compio::test]
async fn all_seeds_unreachable_errors_quickly() {
    // TCP to closed ports fails fast on Linux.
    let seeds = &["redis://127.0.0.1:1", "redis://127.0.0.1:2"];
    let t = std::time::Instant::now();
    let result = ClusterClient::connect(seeds, 4).await;
    let elapsed = t.elapsed();
    assert!(result.is_err(), "expected unreachable seeds to error");
    // Shouldn't hang on long timeouts — each seed should fail within a
    // few seconds max. If this balloons, something added a long timeout.
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "connect took {elapsed:?} — too slow for unreachable seeds"
    );
}

#[compio::test]
async fn non_cluster_redis_behavior_is_bounded() {
    // Point ClusterClient at a non-cluster single-node Redis. Behavior
    // depends on server response to CLUSTER SLOTS:
    //   - New Redis / Dragonfly: returns empty Array (no error). Connect
    //     succeeds, but every command errors with NoRoute since the slot
    //     map is all-None.
    //   - Older Redis: returns Error("CLUSTER support disabled"); connect
    //     fails with ClusterBootstrap.
    // We don't assert a specific variant — just that the failure is
    // observable and doesn't hang.
    // The standalone Redis of the shared fixture, which this suite requires.
    //
    // Reachability is proved with a plain client FIRST, and that ordering is
    // the point: with no server at all, `ClusterClient::connect` also returns
    // Err, and the `Err` arm below would then be judging a connection refusal
    // against `ClusterBootstrap` - a red test whose message names the wrong
    // problem. Proving the node is up first means the match below is only ever
    // read as what it is about.
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    drop(crate::support::connect(&url).await);
    match ClusterClient::connect(&[url.as_str()], 4).await {
        Ok(cc) => {
            // Empty-topology case: slot lookup must return NoRoute.
            let err = match cc.get("anything").await {
                Ok(_) => panic!("expected error against non-cluster Redis"),
                Err(e) => e,
            };
            assert!(
                matches!(err, compio_redis::Error::NoRoute { .. }),
                "expected NoRoute, got {err:?}"
            );
        }
        Err(e) => {
            // Older-Redis case: connect rejects at bootstrap.
            assert!(
                matches!(e, compio_redis::Error::ClusterBootstrap(_)),
                "expected ClusterBootstrap, got {e:?}"
            );
        }
    }
}

#[compio::test]
async fn set_nx_is_idempotent() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:lock", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // First NX: wins.
    assert!(cc.set_nx(&key, b"owner-A", Some(5_000)).await.unwrap());
    // Second NX: key exists, returns false, original value preserved.
    assert!(!cc.set_nx(&key, b"owner-B", Some(5_000)).await.unwrap());
    assert_eq!(
        cc.get(&key).await.unwrap().as_deref(),
        Some(b"owner-A".as_ref()),
        "NX collision must not overwrite"
    );

    cc.del(&key).await.ok();
}

#[compio::test]
async fn pexpire_and_pttl_semantics() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:k", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // Missing key: EXISTS false, PTTL -2, PEXPIRE returns false.
    assert!(!cc.exists(&key).await.unwrap());
    assert_eq!(cc.pttl(&key).await.unwrap(), -2);
    assert!(!cc.pexpire(&key, 10_000).await.unwrap());

    // Set without TTL: PTTL -1, PEXPIRE sets TTL and returns true.
    cc.set(&key, b"v", None).await.unwrap();
    assert!(cc.exists(&key).await.unwrap());
    assert_eq!(cc.pttl(&key).await.unwrap(), -1);
    assert!(cc.pexpire(&key, 10_000).await.unwrap());
    let remaining = cc.pttl(&key).await.unwrap();
    assert!(remaining > 0 && remaining <= 10_000, "pttl={remaining}");

    cc.del(&key).await.ok();
}

#[compio::test]
async fn binary_safe_values_roundtrip_through_cluster() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:blob", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // Full 0..=255 byte range, including NUL and non-UTF8 sequences.
    let value: Vec<u8> = (0..=255u8).collect();
    cc.set(&key, &value, None).await.unwrap();
    let back = cc.get(&key).await.unwrap().unwrap();
    assert_eq!(back, value);
    assert_eq!(cc.strlen(&key).await.unwrap(), 256);

    cc.del(&key).await.ok();
}

#[compio::test]
async fn cloned_handle_shares_topology_and_pools() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc1 = ClusterClient::connect(&seeds, 4).await.expect("connect");
    let cc2 = cc1.clone();

    // Write via cc1; read via cc2. Same underlying state → same node.
    let key = format!("{{{}}}:k", crate::support::case_prefix());
    cc2.del(&key).await.ok();
    cc1.set(&key, b"via-1", None).await.unwrap();
    assert_eq!(cc2.get(&key).await.unwrap().as_deref(), Some(b"via-1".as_ref()));

    cc2.del(&key).await.ok();
}

#[compio::test]
async fn cross_slot_mset_rejected_before_network() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let err = match cc.mset(&[("foo", b"1" as &[u8]), ("bar", b"2")]).await {
        Ok(_) => panic!("cross-slot mset must error"),
        Err(e) => e,
    };
    assert!(matches!(err, compio_redis::Error::CrossSlot));
}

#[compio::test]
async fn empty_batch_ops_are_noops() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    assert!(cc.mget(&[]).await.unwrap().is_empty());
    cc.mset(&[]).await.unwrap();
}

#[compio::test]
async fn mget_preserves_order_and_holes() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let tag = crate::support::case_prefix();
    for k in ["a", "c"] {
        cc.set(&format!("{{{tag}}}:{k}"), k.as_bytes(), None).await.unwrap();
    }
    // Middle key is intentionally missing → Some / None / Some shape.
    let key_a = format!("{{{tag}}}:a");
    let missing = format!("{{{tag}}}:missing");
    let key_c = format!("{{{tag}}}:c");
    let results = cc
        .mget(&[key_a.as_str(), missing.as_str(), key_c.as_str()])
        .await
        .unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_deref(), Some(b"a".as_ref()));
    assert!(results[1].is_none());
    assert_eq!(results[2].as_deref(), Some(b"c".as_ref()));

    for k in ["a", "c"] {
        cc.del(&format!("{{{tag}}}:{k}")).await.ok();
    }
}

#[compio::test]
async fn decr_by_can_go_negative() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:cnt", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // Starting from 0 (missing), decrement 5 → -5.
    assert_eq!(cc.decr_by(&key, 5).await.unwrap(), -5);
    // Symmetry with incr_by.
    assert_eq!(cc.incr_by(&key, 10).await.unwrap(), 5);

    cc.del(&key).await.ok();
}

#[compio::test]
async fn incr_on_non_numeric_errors_with_server() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:notanumber", crate::support::case_prefix());
    cc.del(&key).await.ok();
    cc.set(&key, b"abc", None).await.unwrap();

    let err = match cc.incr_by(&key, 1).await {
        Ok(_) => panic!("INCR on non-numeric must error"),
        Err(e) => e,
    };
    assert!(
        matches!(err, compio_redis::Error::Server(ref m) if m.contains("not an integer")),
        "expected Server(not an integer), got {err:?}"
    );

    cc.del(&key).await.ok();
}

#[compio::test]
async fn large_value_roundtrip() {
    let fixture = crate::support::fixtures();
    let s = fixture.cluster_urls();
    let seeds = seeds_refs(&s);
    let cc = ClusterClient::connect(&seeds, 4).await.expect("connect");

    let key = format!("{{{}}}:v", crate::support::case_prefix());
    cc.del(&key).await.ok();

    // 512 KiB of deterministic content — large enough to exceed a single
    // kernel read buffer and force multi-chunk reassembly.
    let value: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    cc.set(&key, &value, None).await.unwrap();
    let back = cc.get(&key).await.unwrap().unwrap();
    assert_eq!(back.len(), value.len());
    assert_eq!(back, value);
    assert_eq!(cc.strlen(&key).await.unwrap(), value.len() as u64);

    cc.del(&key).await.ok();
}
