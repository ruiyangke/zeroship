//! `PostgreSQL` token-bucket behavior under the auth role.

use crate::common::database::Database;
use zeroship_authn::rate_limit::{consume, Quota, RateLimitDecision};

#[compio::test]
async fn consumes_until_throttled() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let key = "consumes-until-throttled";
        let bucket = Quota {
            capacity: 3.0,
            refill_per_sec: 0.0,
        }; // no refill for the test

        for i in 1..=3 {
            let res = consume(&client, key, bucket).await.expect("consume");
            assert!(
                matches!(res, RateLimitDecision::Allowed),
                "request {i} should pass"
            );
        }

        let res = consume(&client, key, bucket).await.expect("consume");
        let RateLimitDecision::Throttled(err) = res else {
            panic!("4th request should throttle");
        };
        assert!(
            err.retry_after_secs > 0.0 || err.retry_after_secs.is_infinite(),
            "retry_after_secs should be positive, got {}",
            err.retry_after_secs
        );
    })
    .await;
}

#[compio::test]
async fn concurrent_consumes_are_atomic() {
    Database::run(async |database| {
        let seed_client = database.connect_as_auth().await;

        let key = "concurrent-consumption";
        seed_client
            .execute(
                "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) \
                 VALUES ($1, 5.0::REAL, NOW())",
                &[&key],
            )
            .await
            .expect("seed full bucket");

        let bucket = Quota {
            capacity: 5.0,
            refill_per_sec: 0.0,
        };
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.expect("begin bucket lock");
        transaction
            .query_one(
                "SELECT bucket_key FROM zeroship.rate_limits WHERE bucket_key = $1 FOR UPDATE",
                &[&key],
            )
            .await
            .expect("hold the bucket while consumers start");
        let mut clients = Vec::new();
        let mut pids = Vec::new();
        for _ in 0..10 {
            let client = database.connect_as_auth().await;
            pids.push(
                client
                    .query_one("SELECT pg_backend_pid()", &[])
                    .await
                    .unwrap()
                    .get(0),
            );
            clients.push(client);
        }

        let mut handles = Vec::new();
        for client in clients {
            handles.push(compio::runtime::spawn(async move {
                consume(&client, key, bucket).await
            }));
        }

        let overlapped = database.wait_until_blocked(&pids).await;
        transaction
            .commit()
            .await
            .expect("release waiting consumers");

        let mut allowed = 0;
        let mut throttled = 0;
        for result in futures::future::join_all(handles).await {
            match result.expect("consume task panicked").expect("consume") {
                RateLimitDecision::Allowed => allowed += 1,
                RateLimitDecision::Throttled(_) => throttled += 1,
            }
        }

        assert_eq!(allowed, 5, "exactly capacity tokens should be consumed");
        assert_eq!(
            throttled, 5,
            "remaining concurrent attempts should throttle"
        );
        assert!(overlapped, "consumers must contend for the locked bucket");
    })
    .await;
}
