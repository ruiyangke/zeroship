//! Token-bucket rate-limit smoke test (live PG).

use compio_postgres::{connect, NoTls};
use zeroship_auth::ratelimit::{consume, Bucket, RateLimitDecision};

// compio-postgres's `Client` is `!Send` (it owns an io_uring submission
// handle). All async helpers that touch it inherit that.
#[allow(clippy::future_not_send)]
async fn pg_or_skip() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let client = pg_connect(&dsn).await;
    Some(client)
}

#[allow(clippy::future_not_send)]
async fn pg_connect(dsn: &str) -> compio_postgres::Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();
    client
}

#[compio::test]
async fn consumes_until_throttled() {
    let Some(client) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };

    let key = format!("test:{}", uuid::Uuid::new_v4().simple());
    let bucket = Bucket {
        capacity: 3.0,
        refill_per_sec: 0.0,
    }; // no refill for the test

    for i in 1..=3 {
        let res = consume(&client, &key, bucket).await.expect("consume");
        assert!(
            matches!(res, RateLimitDecision::Allowed),
            "request {i} should pass"
        );
    }

    let res = consume(&client, &key, bucket).await.expect("consume");
    let RateLimitDecision::Throttled(err) = res else {
        panic!("4th request should throttle");
    };
    assert!(
        err.retry_after_secs > 0.0 || err.retry_after_secs.is_infinite(),
        "retry_after_secs should be positive, got {}",
        err.retry_after_secs
    );
}

#[compio::test]
async fn concurrent_consumes_are_atomic() {
    let Some(seed_client) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let dsn = std::env::var("AUTH_DB_URL").expect("AUTH_DB_URL present after pg_or_skip");

    let key = format!("test:atomic:{}", uuid::Uuid::new_v4().simple());
    seed_client
        .execute(
            "INSERT INTO auth.rate_limits (bucket_key, tokens, updated_at) \
             VALUES ($1, 5.0::REAL, NOW())",
            &[&key],
        )
        .await
        .expect("seed full bucket");

    let bucket = Bucket {
        capacity: 5.0,
        refill_per_sec: 0.0,
    };
    let mut clients = Vec::new();
    for _ in 0..10 {
        clients.push(pg_connect(&dsn).await);
    }

    let mut handles = Vec::new();
    for client in clients {
        let key = key.clone();
        handles.push(compio::runtime::spawn(async move {
            consume(&client, &key, bucket).await
        }));
    }

    let mut allowed = 0;
    let mut throttled = 0;
    for handle in handles {
        match handle
            .await
            .expect("consume task panicked")
            .expect("consume")
        {
            RateLimitDecision::Allowed => allowed += 1,
            RateLimitDecision::Throttled(_) => throttled += 1,
        }
    }

    assert_eq!(allowed, 5, "exactly capacity tokens should be consumed");
    assert_eq!(throttled, 5, "remaining concurrent attempts should throttle");
}
