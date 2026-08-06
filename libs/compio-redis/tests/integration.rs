//! Integration tests against a real Redis. Set `REDIS_TEST_URL` to run;
//! otherwise tests are skipped.
//!
//! For local dev:
//!   docker run -d --name zs-redis-test -p 6390:6379 redis:7-alpine
//!   REDIS_TEST_URL=redis://127.0.0.1:6390 cargo test -p compio-redis -- --nocapture

use compio_redis::{Client, Pool};

fn test_url() -> Option<String> {
    std::env::var("REDIS_TEST_URL").ok()
}

#[compio::test]
async fn ping_set_get_del_roundtrip() {
    let Some(url) = test_url() else {
        eprintln!("skip: REDIS_TEST_URL not set");
        return;
    };
    let mut c = Client::connect(&url).await.expect("connect");
    c.ping().await.expect("ping");

    c.set("zs:test:k1", b"hello", None).await.expect("set");
    let v = c.get("zs:test:k1").await.expect("get");
    assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

    let deleted = c.del("zs:test:k1").await.expect("del");
    assert!(deleted);

    let missing = c.get("zs:test:k1").await.expect("get after del");
    assert!(missing.is_none());
}

#[compio::test]
async fn ttl_ms_expires() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.set("zs:test:ttl", b"bye", Some(100)).await.expect("set ttl");
    assert_eq!(c.get("zs:test:ttl").await.unwrap().as_deref(), Some(b"bye".as_ref()));
    compio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(c.get("zs:test:ttl").await.unwrap().is_none());
}

#[compio::test]
async fn incr_is_atomic_and_correct() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.del("zs:test:counter").await.ok();

    // Serial incr — correctness.
    for expected in 1..=10 {
        let v = c.incr_by("zs:test:counter", 1).await.expect("incr");
        assert_eq!(v, expected);
    }

    // Negative delta works.
    let v = c.incr_by("zs:test:counter", -5).await.unwrap();
    assert_eq!(v, 5);

    c.del("zs:test:counter").await.ok();
}

#[compio::test]
async fn scan_prefix_returns_matching_keys() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");

    // Seed a few keys under a unique prefix so the test is isolated.
    let prefix = "zs:scan:42";
    for i in 0..5 {
        c.set(&format!("{prefix}:{i}"), b"x", None).await.unwrap();
    }
    // A decoy that SCAN must NOT return.
    c.set("zs:other:decoy", b"x", None).await.unwrap();

    let mut cursor = String::from("0");
    let mut found = Vec::new();
    loop {
        let (next, keys) = c.scan(&cursor, &format!("{prefix}:*"), 100).await.unwrap();
        found.extend(keys);
        if next == "0" { break; }
        cursor = next;
    }
    found.sort();
    assert_eq!(found.len(), 5, "got {found:?}");
    assert!(!found.iter().any(|k| k.contains("decoy")));

    // Cleanup.
    for i in 0..5 {
        c.del(&format!("{prefix}:{i}")).await.ok();
    }
    c.del("zs:other:decoy").await.ok();
}

#[compio::test]
async fn pool_acquire_and_reuse() {
    let Some(url) = test_url() else { return; };
    let pool = Pool::connect(&url, 4).await.expect("pool");
    // Five sequential acquires share the same underlying 1-conn pool.
    for i in 0..5 {
        let mut c = pool.acquire().await.expect("acquire");
        c.set(&format!("zs:pool:{i}"), b"v", None).await.unwrap();
        let v = c.get(&format!("zs:pool:{i}")).await.unwrap();
        assert_eq!(v.as_deref(), Some(b"v".as_ref()));
        c.del(&format!("zs:pool:{i}")).await.unwrap();
    }
}

#[compio::test]
async fn null_reply_on_missing_key() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.del("zs:test:missing").await.ok();
    let v = c.get("zs:test:missing").await.expect("get");
    assert!(v.is_none());
}

#[compio::test]
async fn binary_safe_values() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    // NUL bytes + non-UTF8 sequences must survive round-trip.
    let value: Vec<u8> = (0..=255u8).collect();
    c.set("zs:test:bin", &value, None).await.unwrap();
    let back = c.get("zs:test:bin").await.unwrap().unwrap();
    assert_eq!(back, value);
    c.del("zs:test:bin").await.unwrap();
}

#[compio::test]
async fn set_nx_acts_as_lock() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.del("zs:test:lock").await.ok();

    // First acquire: key doesn't exist, SET NX succeeds.
    assert!(c.set_nx("zs:test:lock", b"owner-1", Some(5_000)).await.unwrap());
    // Second acquire: key exists, SET NX returns false.
    assert!(!c.set_nx("zs:test:lock", b"owner-2", Some(5_000)).await.unwrap());
    // Holder is the original owner.
    let v = c.get("zs:test:lock").await.unwrap();
    assert_eq!(v.as_deref(), Some(b"owner-1".as_ref()));

    c.del("zs:test:lock").await.ok();
}

#[compio::test]
async fn exists_pexpire_pttl_lifecycle() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.del("zs:test:life").await.ok();

    // Missing key: EXISTS=false, PTTL=-2.
    assert!(!c.exists("zs:test:life").await.unwrap());
    assert_eq!(c.pttl("zs:test:life").await.unwrap(), -2);

    // Set without TTL: EXISTS=true, PTTL=-1.
    c.set("zs:test:life", b"v", None).await.unwrap();
    assert!(c.exists("zs:test:life").await.unwrap());
    assert_eq!(c.pttl("zs:test:life").await.unwrap(), -1);

    // PEXPIRE hits: PTTL becomes positive.
    assert!(c.pexpire("zs:test:life", 10_000).await.unwrap());
    let remaining = c.pttl("zs:test:life").await.unwrap();
    assert!(remaining > 0 && remaining <= 10_000, "pttl={remaining}");

    // PEXPIRE on missing key returns false.
    c.del("zs:test:life").await.unwrap();
    assert!(!c.pexpire("zs:test:life", 1_000).await.unwrap());
}

#[compio::test]
async fn decr_by_and_strlen() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");
    c.del("zs:test:cnt").await.ok();

    // Seed via incr, then decrement.
    assert_eq!(c.incr_by("zs:test:cnt", 100).await.unwrap(), 100);
    assert_eq!(c.decr_by("zs:test:cnt", 30).await.unwrap(), 70);
    assert_eq!(c.decr_by("zs:test:cnt", 70).await.unwrap(), 0);

    // STRLEN reads the byte length of the stringified counter.
    c.set("zs:test:cnt", b"hello", None).await.unwrap();
    assert_eq!(c.strlen("zs:test:cnt").await.unwrap(), 5);
    // STRLEN on missing key returns 0, not an error.
    c.del("zs:test:cnt").await.unwrap();
    assert_eq!(c.strlen("zs:test:cnt").await.unwrap(), 0);
}

#[compio::test]
async fn mget_mset_batch_roundtrip() {
    let Some(url) = test_url() else { return; };
    let mut c = Client::connect(&url).await.expect("connect");

    let keys = ["zs:test:m1", "zs:test:m2", "zs:test:m3"];
    for k in &keys { c.del(k).await.ok(); }

    // Empty batch is a no-op that returns an empty vec / Ok.
    assert!(c.mget(&[]).await.unwrap().is_empty());
    c.mset(&[]).await.unwrap();

    c.mset(&[
        ("zs:test:m1", b"one" as &[u8]),
        ("zs:test:m2", b"two"),
        ("zs:test:m3", b"three"),
    ]).await.unwrap();

    let values = c.mget(&["zs:test:m1", "zs:test:missing", "zs:test:m3"]).await.unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].as_deref(), Some(b"one".as_ref()));
    assert!(values[1].is_none());
    assert_eq!(values[2].as_deref(), Some(b"three".as_ref()));

    for k in &keys { c.del(k).await.ok(); }
}
