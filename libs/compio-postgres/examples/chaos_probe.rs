//! TEMPORARY chaos probe. Not committed: it exists to measure the read-timeout
//! clock against a server frozen with `docker pause`, which no scripted peer
//! can reproduce (a scripted peer is a live local socket choosing to withhold
//! bytes; a paused container closes nothing at all).
use compio_postgres::{Config, NoTls};
use std::time::{Duration, Instant};

#[compio::main]
async fn main() {
    let url = std::env::args().nth(1).expect("usage: chaos_probe <url>");
    let mut config: Config = url.parse().expect("parse url");
    config.read_timeout(Duration::from_secs(5));
    let (client, connection) = config.connect(NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    // Prove the session works BEFORE the freeze, so a failure after it cannot
    // be confused with a connection that was never usable.
    let warm: i32 = client
        .query_one("SELECT 1", &[])
        .await
        .expect("pre-freeze query")
        .get(0);
    println!("PROBE warm={warm} ready_for_freeze");
    let started = Instant::now();
    let outcome = client.query_one("SELECT pg_sleep(30)", &[]).await;
    let elapsed = started.elapsed();
    match outcome {
        Ok(_) => println!("PROBE query SUCCEEDED after {elapsed:?} - no timeout fired"),
        Err(error) => println!(
            "PROBE query ended after {elapsed:?} is_closed={} err={error}",
            error.is_closed()
        ),
    }
    println!(
        "PROBE live_connections={}",
        compio_postgres::live_connections()
    );
    // The flag is not the point: what matters is whether the NEXT caller can be
    // handed this session. Ask it directly rather than inferring from is_closed.
    match client.query_one("SELECT 1", &[]).await {
        Ok(_) => println!("PROBE reuse=SUCCEEDED - the session survived the timeout"),
        Err(error) => println!(
            "PROBE reuse=refused is_closed={} err={error}",
            error.is_closed()
        ),
    }
}
