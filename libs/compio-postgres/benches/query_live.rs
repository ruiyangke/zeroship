//! Live, sequential query-throughput probe for hot-path comparisons.
//!
//! This deliberately uses libtest's auto-discovered bench target rather than
//! Criterion: every reported sample is a fixed-size timed loop, which makes it
//! straightforward to alternate already-built binaries from two git revisions.
//! `cargo bench` still compiles the target with the optimized bench profile.
//!
//! Required:
//!   PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5455/zeroship
//!
//! Controls:
//!   CPG_QUERY_BENCH_MODE=raw|prepared|cache|candidate (default: raw)
//!   CPG_QUERY_BENCH_OBSERVER=off|slow              (default: off)
//!   CPG_QUERY_BENCH_READ_TIMEOUT_MS=<nonzero ms>   (default: unset)
//!   CPG_QUERY_BENCH_WARMUP=<operations>            (default: 1000)
//!   CPG_QUERY_BENCH_ITERATIONS=<operations/sample> (default: 10000)
//!   CPG_QUERY_BENCH_SAMPLES=<samples>              (default: 7)
//!   CPG_QUERY_BENCH_LABEL=<free-form run label>     (default: unlabeled)

use std::env;
use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use compio_postgres::{Client, Config, NoTls, ToStatement};

const SQL: &str = "SELECT 1::int4 /* cpg_query_live */";

#[derive(Clone, Copy, Debug)]
enum QueryMode {
    Raw,
    Prepared,
    Cache,
    Candidate,
}

impl QueryMode {
    fn from_env() -> Self {
        match env::var("CPG_QUERY_BENCH_MODE").as_deref() {
            Ok("raw") | Err(_) => Self::Raw,
            Ok("prepared") => Self::Prepared,
            Ok("cache") => Self::Cache,
            Ok("candidate") => Self::Candidate,
            Ok(value) => panic!(
                "invalid CPG_QUERY_BENCH_MODE={value:?}; expected raw, prepared, cache, or candidate"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Prepared => "prepared",
            Self::Cache => "cache",
            Self::Candidate => "candidate",
        }
    }

    fn uses_raw_sql(self) -> bool {
        !matches!(self, Self::Prepared)
    }
}

#[derive(Clone, Copy, Debug)]
enum ObserverMode {
    Off,
    Slow,
}

impl ObserverMode {
    fn from_env() -> Self {
        match env::var("CPG_QUERY_BENCH_OBSERVER").as_deref() {
            Ok("off") | Err(_) => Self::Off,
            Ok("slow") => Self::Slow,
            Ok(value) => panic!(
                "invalid CPG_QUERY_BENCH_OBSERVER={value:?}; expected off or slow"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Slow => "slow",
        }
    }
}

fn positive_env(name: &str, default: usize) -> usize {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or_else(|| panic!("{name} must be a positive integer, got {value:?}")),
        Err(_) => default,
    }
}

async fn run_operations<T>(client: &Client, statement: &T, operations: usize)
where
    T: ?Sized + ToStatement,
{
    for _ in 0..operations {
        let row = client
            .query_one(statement, &[])
            .await
            .expect("live SELECT failed");
        let value: i32 = row.get(0);
        assert_eq!(black_box(value), 1);
    }
}

async fn measure<T>(client: &Client, statement: &T, warmup: usize, iterations: usize, samples: usize)
where
    T: ?Sized + ToStatement,
{
    run_operations(client, statement, warmup).await;

    for sample in 1..=samples {
        let started = Instant::now();
        run_operations(client, statement, iterations).await;
        let elapsed = started.elapsed();
        let elapsed_ns = elapsed.as_nanos();
        let ns_per_op = elapsed_ns as f64 / iterations as f64;
        let ops_per_second = iterations as f64 / elapsed.as_secs_f64();
        println!(
            "CPG_QUERY_RESULT sample={sample} iterations={iterations} elapsed_ns={elapsed_ns} ns_per_op={ns_per_op:.3} ops_per_second={ops_per_second:.3}"
        );
    }
}

#[test]
fn query_throughput() {
    let url = env::var("PG_TEST_URL").expect("PG_TEST_URL is required for this live benchmark");
    let mode = QueryMode::from_env();
    let observer = ObserverMode::from_env();
    let warmup = positive_env("CPG_QUERY_BENCH_WARMUP", 1_000);
    let iterations = positive_env("CPG_QUERY_BENCH_ITERATIONS", 10_000);
    let samples = positive_env("CPG_QUERY_BENCH_SAMPLES", 7);
    let read_timeout_ms = env::var("CPG_QUERY_BENCH_READ_TIMEOUT_MS")
        .ok()
        .map(|_| positive_env("CPG_QUERY_BENCH_READ_TIMEOUT_MS", 0));
    let label = env::var("CPG_QUERY_BENCH_LABEL").unwrap_or_else(|_| "unlabeled".to_owned());

    println!(
        "CPG_QUERY_CONFIG label={label} mode={} observer={} read_timeout_ms={} warmup={warmup} iterations={iterations} samples={samples}",
        mode.label(),
        observer.label(),
        read_timeout_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "off".to_owned())
    );

    let runtime = compio::runtime::Runtime::new().expect("create compio runtime");
    runtime.block_on(async move {
        let mut config: Config = url.parse().expect("parse PG_TEST_URL");
        match mode {
            QueryMode::Raw | QueryMode::Prepared => {}
            QueryMode::Cache => {
                // Threshold one is the normal cache-on policy. The untimed
                // warmup below promotes SQL before any sample starts.
                config.statement_cache_capacity(1);
            }
            QueryMode::Candidate => {
                // An unreachable promotion threshold keeps every measured raw
                // query probationary while exercising the bounded candidate
                // map. This is useful as an absolute workload, but it is not
                // an isolated map-cost comparison with `raw`: probationary
                // execution uses the unnamed statement and therefore avoids
                // the named statement's close/cleanup path.
                config.statement_cache_capacity(1);
                config.statement_cache_execution_threshold(
                    NonZeroUsize::new(usize::MAX).expect("usize::MAX is nonzero"),
                );
            }
        }
        if let Some(read_timeout_ms) = read_timeout_ms {
            config.read_timeout(Duration::from_millis(read_timeout_ms as u64));
        }

        let (client, connection) = config.connect(NoTls).await.expect("connect to PostgreSQL");
        compio::runtime::spawn(async move {
            if let Err(error) = connection.run().await {
                eprintln!("connection driver failed: {error}");
            }
        })
        .detach();

        // Retain the receiver for the entire run. Duration::MAX filters every
        // ordinary SELECT without queue growth while preserving the per-query
        // slow-threshold decision this mode exists to measure.
        let _query_events = match observer {
            ObserverMode::Off => None,
            ObserverMode::Slow => Some(client.query_events_with_threshold(Duration::MAX)),
        };

        if mode.uses_raw_sql() {
            measure(&client, SQL, warmup, iterations, samples).await;
        } else {
            // Preparation is intentionally outside both warmup and samples;
            // this is the exact cross-revision one-round-trip control, not an
            // alias for the implicit statement-cache path.
            let statement = client.prepare(SQL).await.expect("prepare live SELECT");
            measure(&client, &statement, warmup, iterations, samples).await;
        }
    });
}
