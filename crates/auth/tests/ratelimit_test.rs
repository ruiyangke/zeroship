//! Token-bucket rate-limit smoke test (live PG).

use compio_postgres::{connect, NoTls};
use zeroship_auth::ratelimit::{consume, Bucket};
use zeroship_auth::store::migrations;

async fn pg_or_skip() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
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
        assert!(res.is_ok(), "request {i} should pass");
    }

    let res = consume(&client, &key, bucket).await.expect("consume");
    let err = res.expect_err("4th request should throttle");
    assert!(
        err.retry_after_secs > 0.0 || err.retry_after_secs.is_infinite(),
        "retry_after_secs should be positive, got {}",
        err.retry_after_secs
    );
}
