//! Live Redis driver tests against the shared standalone server.
//! Run with Cargo or nextest; Docker is required.
//!
//! The server is shared by every test process of the worktree, so each case
//! mints its own key prefix with `crate::support::case_prefix()` rather than
//! using a literal key: a literal could collide with a concurrent case's, and
//! the server is never flushed or assumed empty.

#[compio::test]
async fn ping_set_get_del_roundtrip() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    c.ping().await.expect("ping");

    let p = crate::support::case_prefix();
    let k1 = format!("{p}:k1");
    c.set(&k1, b"hello", None).await.expect("set");
    let v = c.get(&k1).await.expect("get");
    assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

    let deleted = c.del(&k1).await.expect("del");
    assert!(deleted);

    let missing = c.get(&k1).await.expect("get after del");
    assert!(missing.is_none());
}

#[compio::test]
async fn ttl_ms_expires() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:ttl", crate::support::case_prefix());
    c.set(&key, b"bye", Some(100)).await.expect("set ttl");
    assert_eq!(c.get(&key).await.unwrap().as_deref(), Some(b"bye".as_ref()));
    compio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(c.get(&key).await.unwrap().is_none());
}

#[compio::test]
async fn incr_is_atomic_and_correct() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:counter", crate::support::case_prefix());
    c.del(&key).await.ok();

    // Serial incr — correctness.
    for expected in 1..=10 {
        let v = c.incr_by(&key, 1).await.expect("incr");
        assert_eq!(v, expected);
    }

    // Negative delta works.
    let v = c.incr_by(&key, -5).await.unwrap();
    assert_eq!(v, 5);

    c.del(&key).await.ok();
}

#[compio::test]
async fn scan_prefix_returns_matching_keys() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;

    // Seed a few keys under a case-unique prefix so the scan is isolated from
    // every other case on the shared server.
    let base = crate::support::case_prefix();
    let prefix = format!("{base}:scan:42");
    for i in 0..5 {
        c.set(&format!("{prefix}:{i}"), b"x", None).await.unwrap();
    }
    // A decoy that SCAN must NOT return.
    let decoy = format!("{base}:other:decoy");
    c.set(&decoy, b"x", None).await.unwrap();

    let mut cursor = String::from("0");
    let mut found = Vec::new();
    loop {
        let (next, keys) = c.scan(&cursor, &format!("{prefix}:*"), 100).await.unwrap();
        found.extend(keys);
        if next == "0" {
            break;
        }
        cursor = next;
    }
    found.sort();
    assert_eq!(found.len(), 5, "got {found:?}");
    assert!(!found.iter().any(|k| k.contains("decoy")));

    // Cleanup.
    for i in 0..5 {
        c.del(&format!("{prefix}:{i}")).await.ok();
    }
    c.del(&decoy).await.ok();
}

#[compio::test]
async fn pool_acquire_and_reuse() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let pool = crate::support::connect_pool(&url, 4).await;
    let prefix = crate::support::case_prefix();
    // Five sequential acquires share the same underlying 1-conn pool.
    for i in 0..5 {
        let key = format!("{prefix}:pool:{i}");
        let mut c = pool.acquire().await.expect("acquire");
        c.set(&key, b"v", None).await.unwrap();
        let v = c.get(&key).await.unwrap();
        assert_eq!(v.as_deref(), Some(b"v".as_ref()));
        c.del(&key).await.unwrap();
    }
}

#[compio::test]
async fn null_reply_on_missing_key() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:missing", crate::support::case_prefix());
    c.del(&key).await.ok();
    let v = c.get(&key).await.expect("get");
    assert!(v.is_none());
}

#[compio::test]
async fn binary_safe_values() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:bin", crate::support::case_prefix());
    // NUL bytes + non-UTF8 sequences must survive round-trip.
    let value: Vec<u8> = (0..=255u8).collect();
    c.set(&key, &value, None).await.unwrap();
    let back = c.get(&key).await.unwrap().unwrap();
    assert_eq!(back, value);
    c.del(&key).await.unwrap();
}

#[compio::test]
async fn set_nx_acts_as_lock() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:lock", crate::support::case_prefix());
    c.del(&key).await.ok();

    // First acquire: key doesn't exist, SET NX succeeds.
    assert!(c.set_nx(&key, b"owner-1", Some(5_000)).await.unwrap());
    // Second acquire: key exists, SET NX returns false.
    assert!(!c.set_nx(&key, b"owner-2", Some(5_000)).await.unwrap());
    // Holder is the original owner.
    let v = c.get(&key).await.unwrap();
    assert_eq!(v.as_deref(), Some(b"owner-1".as_ref()));

    c.del(&key).await.ok();
}

#[compio::test]
async fn exists_pexpire_pttl_lifecycle() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:life", crate::support::case_prefix());
    c.del(&key).await.ok();

    // Missing key: EXISTS=false, PTTL=-2.
    assert!(!c.exists(&key).await.unwrap());
    assert_eq!(c.pttl(&key).await.unwrap(), -2);

    // Set without TTL: EXISTS=true, PTTL=-1.
    c.set(&key, b"v", None).await.unwrap();
    assert!(c.exists(&key).await.unwrap());
    assert_eq!(c.pttl(&key).await.unwrap(), -1);

    // PEXPIRE hits: PTTL becomes positive.
    assert!(c.pexpire(&key, 10_000).await.unwrap());
    let remaining = c.pttl(&key).await.unwrap();
    assert!(remaining > 0 && remaining <= 10_000, "pttl={remaining}");

    // PEXPIRE on missing key returns false.
    c.del(&key).await.unwrap();
    assert!(!c.pexpire(&key, 1_000).await.unwrap());
}

#[compio::test]
async fn decr_by_and_strlen() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;
    let key = format!("{}:cnt", crate::support::case_prefix());
    c.del(&key).await.ok();

    // Seed via incr, then decrement.
    assert_eq!(c.incr_by(&key, 100).await.unwrap(), 100);
    assert_eq!(c.decr_by(&key, 30).await.unwrap(), 70);
    assert_eq!(c.decr_by(&key, 70).await.unwrap(), 0);

    // STRLEN reads the byte length of the stringified counter.
    c.set(&key, b"hello", None).await.unwrap();
    assert_eq!(c.strlen(&key).await.unwrap(), 5);
    // STRLEN on missing key returns 0, not an error.
    c.del(&key).await.unwrap();
    assert_eq!(c.strlen(&key).await.unwrap(), 0);
}

#[compio::test]
async fn mget_mset_batch_roundtrip() {
    let fixture = crate::support::fixtures();
    let url = fixture.redis_url();
    let mut c = crate::support::connect(&url).await;

    let base = crate::support::case_prefix();
    let keys = [format!("{base}:m1"), format!("{base}:m2"), format!("{base}:m3")];
    for k in &keys {
        c.del(k).await.ok();
    }

    // Empty batch is a no-op that returns an empty vec / Ok.
    assert!(c.mget(&[]).await.unwrap().is_empty());
    c.mset(&[]).await.unwrap();

    c.mset(&[
        (keys[0].as_str(), b"one" as &[u8]),
        (keys[1].as_str(), b"two"),
        (keys[2].as_str(), b"three"),
    ])
    .await
    .unwrap();

    let missing = format!("{base}:missing");
    let values = c
        .mget(&[keys[0].as_str(), missing.as_str(), keys[2].as_str()])
        .await
        .unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].as_deref(), Some(b"one".as_ref()));
    assert!(values[1].is_none());
    assert_eq!(values[2].as_deref(), Some(b"three".as_ref()));

    for k in &keys {
        c.del(k).await.ok();
    }
}
