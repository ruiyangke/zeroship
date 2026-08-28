//! Chaos probe: a healthy server with no connection slots left.
//!
//! The server is up and answering; it just will not take you. This is the shape
//! a platform running many apps against one PostgreSQL meets first, and the one
//! where a regression is invisible from the top line alone: the pool refuses
//! either way, and only the CAUSE distinguishes "the server is full" from
//! "something went wrong". `Error`'s own `Display` is the terse `db error` by
//! design, so this probe walks the source chain and prints what it finds.
//!
//! Run it against a SMALL, DEDICATED server, never a shared fixture:
//!
//! ```text
//! docker run -d --name zs-cpg-small-5461 -p 127.0.0.1:5461:5432 \
//!   -e POSTGRES_PASSWORD=zeroship -e POSTGRES_DB=zeroship \
//!   postgres:16 postgres -c max_connections=15
//! ```
use compio_postgres::{Client, Config, NoTls, Pool, PoolConfig};
use std::error::Error as _;
use std::time::Instant;

/// Every message in the chain, joined. The actionable text is never the top.
fn chain(error: &compio_postgres::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(" | ")
}

async fn open(config: &Config) -> Result<Client, compio_postgres::Error> {
    let (client, connection) = config.connect(NoTls).await?;
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    Ok(client)
}

#[compio::main]
async fn main() {
    let url = std::env::args().nth(1).expect("usage: chaos_slots <url>");
    let config: Config = url.parse().expect("parse url");

    // Hold connections until the server REFUSES. Counting to max_connections is
    // not the same as reaching exhaustion: this role may be a superuser and can
    // take the superuser_reserved_connections slots too.
    let mut held: Vec<Client> = Vec::new();
    let refusal = loop {
        match open(&config).await {
            Ok(client) => held.push(client),
            Err(error) => break error,
        }
        if held.len() > 200 {
            println!("SLOTS held={} NEVER REFUSED - aborting", held.len());
            return;
        }
    };
    println!(
        "SLOTS held={} then refused: {}",
        held.len(),
        chain(&refusal)
    );

    // The same refusal seen through a fresh connect attempt, timed.
    let started = Instant::now();
    let again = open(&config).await;
    let elapsed = started.elapsed();
    match again {
        Ok(_) => println!("SLOTS connect SUCCEEDED after {elapsed:?} - not exhausted"),
        Err(error) => println!("SLOTS connect refused after {elapsed:?}: {}", chain(&error)),
    }

    // The POOL arm, which is NOT the same measurement. A direct connect is
    // single-shot by design, so it refuses in about a millisecond. Pool warm-up
    // retries three times, sleeping 100ms then 400ms between failures, so its
    // refusal necessarily costs about 500ms. Quoting one number for the other
    // makes a healthy driver look 380x slower or faster than it is.
    let mut pool_config = PoolConfig::new();
    pool_config.max_size(2);
    pool_config.min_idle(2);
    let started = Instant::now();
    let pool = Pool::connect_with_config(config.clone(), pool_config);
    let pool_outcome = pool.await;
    let pool_elapsed = started.elapsed();
    match pool_outcome {
        Ok(_) => println!("SLOTS pool build SUCCEEDED after {pool_elapsed:?} - not exhausted"),
        Err(error) => println!(
            "SLOTS pool build refused after {pool_elapsed:?}: {}",
            chain(&error)
        ),
    }

    // Free the slots and prove the server serves again.
    held.clear();
    match open(&config).await {
        Ok(client) => {
            let value: i32 = client
                .query_one("SELECT 42", &[])
                .await
                .expect("post-recovery query")
                .get(0);
            println!(
                "SLOTS recovered: SELECT 42 = {value}, live={}",
                compio_postgres::live_connections()
            );
        }
        Err(error) => println!("SLOTS DID NOT RECOVER: {}", chain(&error)),
    }
}
