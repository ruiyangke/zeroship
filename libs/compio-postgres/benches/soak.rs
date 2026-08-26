//! Opt-in, duration-bounded soak for long-lived compio-postgres behaviour.
//!
//! `tests/soak.sh` is the supported entry point. This is a harness-free bench
//! target so the ordinary `cargo test -p compio-postgres` target set does not
//! change. The workload creates no database objects: a unique
//! `application_name` scopes every server-side observation to this process.

#![recursion_limit = "256"]

use std::any::Any;
use std::cell::Cell;
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use compio::runtime::JoinHandle;
#[cfg(feature = "tls")]
use compio_postgres::MakeRustlsConnect;
use compio_postgres::{Client, Config, Error, NoTls, Pool, PoolConfig};

const DEFAULT_URL: &str = "postgres://postgres:zeroship@127.0.0.1:5455/zeroship";
const DEFAULT_DURATION_SECS: u64 = 180;
const DEFAULT_SAMPLE_INTERVAL_SECS: u64 = 5;
const MIN_DURATION_SECS: u64 = 30;
const QUERY_WORKERS: usize = 4;
const POOL_MAX_SIZE: usize = 8;
const POOL_MIN_IDLE: usize = POOL_MAX_SIZE;
const LARGE_PAYLOAD_EVERY: u64 = 16;
const LARGE_PAYLOAD_BYTES: usize = 64 * 1024;
// Pool housekeeping runs every 30 seconds. Warm through one complete tick so
// its first eviction/refill allocation cannot masquerade as ruled RSS growth.
const WARMUP: Duration = Duration::from_secs(35);
const OPERATION_WATCHDOG: Duration = Duration::from_secs(10);
const SETUP_WATCHDOG: Duration = Duration::from_secs(20);
const SETTLE_WATCHDOG: Duration = Duration::from_secs(15);
const LOAD_GRACE: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const QUERY_DELAY_SECONDS: f64 = 0.02;
const CANCELLATION_DELAY_SECONDS: f64 = 1.0;

const POOLED_QUERY_SQL: &str = "SELECT $1::int8 FROM pg_sleep(0.02) /* cpg_soak_pooled */";
const LARGE_PAYLOAD_SQL: &str =
    "SELECT repeat('x', 65536) FROM pg_sleep(0.02) /* cpg_soak_pooled */";
const CANCEL_SQL: &str = "SELECT pg_sleep(1) /* cpg_soak_cancel */";
const BAD_CONNECTION_SQL: &str = "SELECT pg_terminate_backend(pg_backend_pid()) /* cpg_soak_bad */";

#[derive(Debug)]
struct Args {
    url: String,
    duration_secs: u64,
    sample_interval_secs: u64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut parsed = Self {
            url: DEFAULT_URL.to_owned(),
            duration_secs: DEFAULT_DURATION_SECS,
            sample_interval_secs: DEFAULT_SAMPLE_INTERVAL_SECS,
        };
        let mut arguments = env::args().skip(1);

        while let Some(flag) = arguments.next() {
            // Cargo appends this marker when it launches a harness-free bench.
            // It carries no value and changes no soak behaviour.
            if flag == "--bench" {
                continue;
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--url" => parsed.url = value,
                "--duration-secs" => {
                    parsed.duration_secs = parse_positive(&flag, &value)?;
                }
                "--sample-interval-secs" => {
                    parsed.sample_interval_secs = parse_positive(&flag, &value)?;
                }
                _ => return Err(format!("unknown argument {flag}")),
            }
        }

        if parsed.url.is_empty() {
            return Err("--url must not be empty".to_owned());
        }
        if parsed.duration_secs < MIN_DURATION_SECS {
            return Err(format!(
                "--duration-secs must be at least {MIN_DURATION_SECS}, got {}",
                parsed.duration_secs
            ));
        }
        let sample_floor = parsed.duration_secs / parsed.sample_interval_secs + 1;
        if sample_floor < 6 {
            return Err(format!(
                "duration {}s and sample interval {}s produce only {sample_floor} samples; need at least 6",
                parsed.duration_secs, parsed.sample_interval_secs
            ));
        }

        Ok(parsed)
    }
}

fn parse_positive(flag: &str, value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be a positive integer, got {value:?}"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be positive"));
    }
    Ok(parsed)
}

#[derive(Debug)]
struct Floors {
    pooled_queries: u64,
    pool_acquire_releases: u64,
    large_payload_queries: u64,
    clean_connections: u64,
    bad_connections: u64,
    cancellations: u64,
    total_operations: u64,
    rss_samples: usize,
}

impl Floors {
    fn for_args(args: &Args) -> Result<Self, String> {
        let workers = u64::try_from(QUERY_WORKERS).expect("query worker count fits u64");
        let pooled_queries = args
            .duration_secs
            .checked_mul(workers)
            .ok_or_else(|| "duration is too large to derive operation floors".to_owned())?;
        let cancellations = (args.duration_secs / 10).max(3);
        let clean_connections = (args.duration_secs / 2).max(10);
        let bad_connections = (args.duration_secs / 10).max(3);
        let pool_acquire_releases = pooled_queries
            .checked_add(cancellations)
            .ok_or_else(|| "duration is too large to derive pool floors".to_owned())?;
        let total_operations = pooled_queries
            .checked_add(clean_connections)
            .and_then(|value| value.checked_add(bad_connections))
            .and_then(|value| value.checked_add(cancellations))
            .ok_or_else(|| "duration is too large to derive the total floor".to_owned())?;
        let rss_samples = usize::try_from(args.duration_secs / args.sample_interval_secs + 1)
            .map_err(|_| "sample count does not fit usize".to_owned())?;

        Ok(Self {
            pooled_queries,
            pool_acquire_releases,
            large_payload_queries: (pooled_queries / LARGE_PAYLOAD_EVERY).max(1),
            clean_connections,
            bad_connections,
            cancellations,
            total_operations,
            rss_samples,
        })
    }
}

#[derive(Debug)]
struct Counts {
    pooled_queries: Cell<u64>,
    pool_acquires: Cell<u64>,
    pool_releases: Cell<u64>,
    large_payload_queries: Cell<u64>,
    clean_connections: Cell<u64>,
    bad_connections: Cell<u64>,
    cancellations: Cell<u64>,
    cancellation_recoveries: Cell<u64>,
    per_query_worker: Vec<Cell<u64>>,
}

impl Counts {
    fn new() -> Self {
        Self {
            pooled_queries: Cell::new(0),
            pool_acquires: Cell::new(0),
            pool_releases: Cell::new(0),
            large_payload_queries: Cell::new(0),
            clean_connections: Cell::new(0),
            bad_connections: Cell::new(0),
            cancellations: Cell::new(0),
            cancellation_recoveries: Cell::new(0),
            per_query_worker: (0..QUERY_WORKERS).map(|_| Cell::new(0)).collect(),
        }
    }

    fn total_operations(&self) -> u64 {
        self.pooled_queries
            .get()
            .saturating_add(self.clean_connections.get())
            .saturating_add(self.bad_connections.get())
            .saturating_add(self.cancellations.get())
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    elapsed_ms: u128,
    rss_kib: u64,
    server_backends: i64,
    server_active_queries: i64,
    driver_live_connections: usize,
    operations: u64,
}

#[derive(Debug)]
struct ServerSnapshot {
    backends: i64,
    active_queries: i64,
}

fn tagged_config(url: &str, application_name: &str) -> Result<Config, String> {
    let mut config: Config = url
        .parse()
        .map_err(|error| format!("parse PostgreSQL URL: {error}"))?;
    config.application_name(application_name);
    Ok(config)
}

type ConnectionDriver = Pin<Box<dyn Future<Output = Result<(), Error>>>>;

/// A direct-connection recipe resolved once, matching the pool's transport policy.
#[derive(Clone)]
struct DirectTransport {
    config: Config,
    #[cfg(feature = "tls")]
    tls: Option<MakeRustlsConnect>,
}

impl DirectTransport {
    fn resolve(config: Config) -> Result<Self, String> {
        #[cfg(feature = "tls")]
        let tls = if config.get_ssl_mode().permits_tls() {
            Some(
                MakeRustlsConnect::from_config(&config)
                    .map_err(|error| format!("resolve direct TLS connector: {error}"))?,
            )
        } else {
            None
        };

        Ok(Self {
            config,
            #[cfg(feature = "tls")]
            tls,
        })
    }

    async fn connect(&self, label: &str) -> Result<(Client, ConnectionDriver), String> {
        #[cfg(feature = "tls")]
        if let Some(tls) = &self.tls {
            let (client, connection) = watched_db(label, self.config.connect(tls.clone())).await?;
            return Ok((client, Box::pin(async move { connection.run().await })));
        }

        let (client, connection) = watched_db(label, self.config.connect(NoTls)).await?;
        Ok((client, Box::pin(async move { connection.run().await })))
    }
}

async fn watched_db<T, F>(label: &str, future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, Error>>,
{
    match compio::time::timeout(OPERATION_WATCHDOG, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{label} failed: {error}")),
        Err(_) => Err(format!(
            "{label} exceeded its {OPERATION_WATCHDOG:?} watchdog"
        )),
    }
}

async fn open_client(transport: &DirectTransport, label: &str) -> Result<Client, String> {
    let (client, driver) = transport.connect(label).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = driver.await {
            eprintln!("detached connection ended with an error: {error}");
        }
    })
    .detach();
    Ok(client)
}

async fn server_snapshot(
    observer: &Client,
    application_name: &str,
) -> Result<ServerSnapshot, String> {
    let row = watched_db(
        "sample pg_stat_activity",
        observer.query_one(
            "SELECT count(*)::int8,
                    count(*) FILTER (
                        WHERE state = 'active'
                          AND query LIKE '%cpg_soak_pooled%'
                    )::int8
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND application_name = $1",
            &[&application_name],
        ),
    )
    .await?;
    Ok(ServerSnapshot {
        backends: row.get(0),
        active_queries: row.get(1),
    })
}

async fn wait_for_backend_absent(observer: &Client, process_id: i32) -> Result<(), String> {
    let deadline = Instant::now() + SETTLE_WATCHDOG;
    loop {
        let exists: bool = watched_db(
            "poll pg_stat_activity for a terminated backend",
            observer.query_one_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                &[&process_id],
            ),
        )
        .await?;
        if !exists {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "terminated backend {process_id} remained in pg_stat_activity after {SETTLE_WATCHDOG:?}"
            ));
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_server_baseline(
    observer: &Client,
    application_name: &str,
    baseline: i64,
) -> Result<i64, String> {
    let deadline = Instant::now() + SETTLE_WATCHDOG;
    loop {
        let observed = server_snapshot(observer, application_name).await?.backends;
        if observed == baseline {
            return Ok(observed);
        }
        if Instant::now() >= deadline {
            return Ok(observed);
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_driver_baseline(baseline: usize) -> usize {
    let deadline = Instant::now() + SETTLE_WATCHDOG;
    loop {
        let observed = compio_postgres::live_connections();
        if observed == baseline || Instant::now() >= deadline {
            return observed;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

fn read_rss_kib() -> Result<u64, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("read /proc/self/status: {error}"))?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .ok_or_else(|| "VmRSS is absent from /proc/self/status".to_owned())?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("VmRSS:") {
        return Err(format!("malformed VmRSS line: {line}"));
    }
    fields
        .next()
        .ok_or_else(|| format!("VmRSS line has no value: {line}"))?
        .parse::<u64>()
        .map_err(|error| format!("parse VmRSS from {line:?}: {error}"))
}

async fn query_worker(
    worker: usize,
    pool: Rc<Pool>,
    counts: Rc<Counts>,
    stop: Rc<Cell<bool>>,
    deadline: Instant,
) -> Result<(), String> {
    let mut iteration = 0_u64;
    while Instant::now() < deadline && !stop.get() {
        let client = watched_db("acquire pooled query connection", pool.get()).await?;
        counts.pool_acquires.set(counts.pool_acquires.get() + 1);

        if iteration % LARGE_PAYLOAD_EVERY == LARGE_PAYLOAD_EVERY - 1 {
            let payload: String = watched_db(
                "run pooled large-payload query",
                client.query_one_scalar(LARGE_PAYLOAD_SQL, &[]),
            )
            .await?;
            if payload.len() != LARGE_PAYLOAD_BYTES {
                return Err(format!(
                    "large-payload query returned {} bytes, expected {LARGE_PAYLOAD_BYTES}",
                    payload.len()
                ));
            }
            counts
                .large_payload_queries
                .set(counts.large_payload_queries.get() + 1);
        } else {
            let sent = i64::try_from(iteration).unwrap_or(i64::MAX);
            let returned: i64 = watched_db(
                "run pooled scalar query",
                client.query_one_scalar(POOLED_QUERY_SQL, &[&sent]),
            )
            .await?;
            if returned != sent {
                return Err(format!(
                    "pooled scalar query changed {sent} into {returned}"
                ));
            }
        }

        drop(client);
        counts.pool_releases.set(counts.pool_releases.get() + 1);
        counts.pooled_queries.set(counts.pooled_queries.get() + 1);
        counts.per_query_worker[worker].set(counts.per_query_worker[worker].get() + 1);
        iteration += 1;
    }
    Ok(())
}

async fn clean_connection_worker(
    transport: DirectTransport,
    counts: Rc<Counts>,
    stop: Rc<Cell<bool>>,
    deadline: Instant,
) -> Result<(), String> {
    let mut iteration = 0_i64;
    while Instant::now() < deadline && !stop.get() {
        let client = open_client(&transport, "open clean churn connection").await?;
        let returned: i64 = watched_db(
            "query clean churn connection",
            client.query_one_scalar("SELECT $1::int8", &[&iteration]),
        )
        .await?;
        if returned != iteration {
            return Err(format!(
                "clean churn query changed {iteration} into {returned}"
            ));
        }
        drop(client);
        counts
            .clean_connections
            .set(counts.clean_connections.get() + 1);
        iteration = iteration.saturating_add(1);
        compio::time::sleep(Duration::from_millis(40)).await;
    }
    Ok(())
}

async fn bad_connection_worker(
    transport: DirectTransport,
    observer: Rc<Client>,
    counts: Rc<Counts>,
    stop: Rc<Cell<bool>>,
    deadline: Instant,
) -> Result<(), String> {
    while Instant::now() < deadline && !stop.get() {
        let (client, connection_driver) = transport
            .connect("open deliberately bad connection")
            .await?;
        let process_id = client.process_id();
        let driver = compio::runtime::spawn(connection_driver);

        match compio::time::timeout(OPERATION_WATCHDOG, client.simple_query(BAD_CONNECTION_SQL))
            .await
        {
            Ok(Err(_)) => {}
            Ok(Ok(_)) => {
                return Err(format!(
                    "backend {process_id} survived its own pg_terminate_backend call"
                ));
            }
            Err(_) => {
                return Err(format!(
                    "self-termination query exceeded its {OPERATION_WATCHDOG:?} watchdog"
                ));
            }
        }

        let driver_outcome = compio::time::timeout(OPERATION_WATCHDOG, driver)
            .await
            .map_err(|_| format!("terminated connection driver exceeded {OPERATION_WATCHDOG:?}"))?
            .map_err(|panic| {
                format!(
                    "terminated connection driver panicked: {}",
                    panic_text(panic)
                )
            })?;
        if driver_outcome.is_ok() {
            return Err(format!(
                "connection driver for terminated backend {process_id} reported a clean close"
            ));
        }

        drop(client);
        wait_for_backend_absent(&observer, process_id).await?;
        counts.bad_connections.set(counts.bad_connections.get() + 1);
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

async fn cancellation_worker(
    pool: Rc<Pool>,
    counts: Rc<Counts>,
    stop: Rc<Cell<bool>>,
    deadline: Instant,
) -> Result<(), String> {
    while Instant::now() < deadline && !stop.get() {
        let mut client = watched_db("acquire cancellation connection", pool.get()).await?;
        counts.pool_acquires.set(counts.pool_acquires.get() + 1);

        let result = compio::time::timeout(
            OPERATION_WATCHDOG,
            client.command(async |client| client.batch_execute(CANCEL_SQL).await),
        )
        .await
        .map_err(|_| format!("cancellation cycle exceeded {OPERATION_WATCHDOG:?}"))?;
        let error = match result {
            Ok(()) => {
                return Err(
                    "pg_sleep completed instead of reaching the pool command timeout".to_owned(),
                );
            }
            Err(error) => error,
        };
        if !error.is_command_timeout() {
            return Err(format!(
                "cancelled command returned a non-timeout error: {error}"
            ));
        }
        counts.cancellations.set(counts.cancellations.get() + 1);

        let recovered: i32 = watched_db(
            "query cancellation connection after recovery",
            client.query_one_scalar("SELECT 42::int4", &[]),
        )
        .await?;
        if recovered != 42 {
            return Err(format!(
                "post-cancellation query returned {recovered}, expected 42"
            ));
        }
        counts
            .cancellation_recoveries
            .set(counts.cancellation_recoveries.get() + 1);
        drop(client);
        counts.pool_releases.set(counts.pool_releases.get() + 1);
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

fn spawn_worker<F>(
    name: &'static str,
    stop: Rc<Cell<bool>>,
    future: F,
) -> JoinHandle<Result<(), String>>
where
    F: Future<Output = Result<(), String>> + 'static,
{
    compio::runtime::spawn(async move {
        let result = future.await.map_err(|error| format!("{name}: {error}"));
        if result.is_err() {
            stop.set(true);
        }
        result
    })
}

async fn await_workers(
    workers: Vec<(&'static str, JoinHandle<Result<(), String>>)>,
) -> Result<(), String> {
    let mut first_error = None;
    for (name, worker) in workers {
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                first_error.get_or_insert(error);
            }
            Err(panic) => {
                first_error
                    .get_or_insert_with(|| format!("{name} panicked: {}", panic_text(panic)));
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn panic_text(panic: Box<dyn Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => match panic.downcast::<&'static str>() {
            Ok(message) => (*message).to_owned(),
            Err(_) => "non-string panic payload".to_owned(),
        },
    }
}

async fn run_load(
    duration: Duration,
    pool: Rc<Pool>,
    direct_transport: DirectTransport,
    observer: Rc<Client>,
    counts: Rc<Counts>,
) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    let stop = Rc::new(Cell::new(false));
    let mut workers = Vec::with_capacity(QUERY_WORKERS + 3);

    for worker in 0..QUERY_WORKERS {
        let pool = Rc::clone(&pool);
        let counts = Rc::clone(&counts);
        let worker_stop = Rc::clone(&stop);
        let future = query_worker(worker, pool, counts, Rc::clone(&worker_stop), deadline);
        workers.push((
            "pooled query worker",
            spawn_worker("pooled query worker", worker_stop, future),
        ));
    }

    {
        let counts = Rc::clone(&counts);
        let worker_stop = Rc::clone(&stop);
        let future = clean_connection_worker(
            direct_transport.clone(),
            counts,
            Rc::clone(&worker_stop),
            deadline,
        );
        workers.push((
            "clean connection worker",
            spawn_worker("clean connection worker", worker_stop, future),
        ));
    }
    {
        let counts = Rc::clone(&counts);
        let worker_stop = Rc::clone(&stop);
        let future = bad_connection_worker(
            direct_transport,
            observer,
            counts,
            Rc::clone(&worker_stop),
            deadline,
        );
        workers.push((
            "bad connection worker",
            spawn_worker("bad connection worker", worker_stop, future),
        ));
    }
    {
        let worker_stop = Rc::clone(&stop);
        let future = cancellation_worker(pool, counts, Rc::clone(&worker_stop), deadline);
        workers.push((
            "cancellation worker",
            spawn_worker("cancellation worker", worker_stop, future),
        ));
    }

    await_workers(workers).await
}

async fn sample_series(
    sample_count: usize,
    interval: Duration,
    application_name: String,
    observer: Rc<Client>,
    counts: Rc<Counts>,
    samples: Rc<std::cell::RefCell<Vec<Sample>>>,
) -> Result<(), String> {
    let started = Instant::now();
    for index in 0..sample_count {
        if index > 0 {
            let multiplier = u32::try_from(index).map_err(|_| "too many samples".to_owned())?;
            let target = started + interval * multiplier;
            compio::time::sleep(target.saturating_duration_since(Instant::now())).await;
        }
        let rss_kib = read_rss_kib()?;
        let snapshot = server_snapshot(&observer, &application_name).await?;
        let sample = Sample {
            elapsed_ms: started.elapsed().as_millis(),
            rss_kib,
            server_backends: snapshot.backends,
            server_active_queries: snapshot.active_queries,
            driver_live_connections: compio_postgres::live_connections(),
            operations: counts.total_operations(),
        };
        println!(
            "sample index={index} elapsed_ms={} rss_kib={} server_backends={} \
             server_active_queries={} driver_live_connections={} operations={}",
            sample.elapsed_ms,
            sample.rss_kib,
            sample.server_backends,
            sample.server_active_queries,
            sample.driver_live_connections,
            sample.operations
        );
        samples.borrow_mut().push(sample);
    }
    Ok(())
}

fn check_floor(name: &str, observed: u64, floor: u64) -> Result<(), String> {
    println!("floor name={name} observed={observed} required={floor}");
    if observed < floor {
        Err(format!(
            "operation floor {name} ruled on only {observed}, below required {floor}"
        ))
    } else {
        Ok(())
    }
}

fn rule_measurements(counts: &Counts, floors: &Floors, samples: &[Sample]) -> Result<(), String> {
    let mut failures = Vec::new();
    for result in [
        check_floor(
            "pooled_queries",
            counts.pooled_queries.get(),
            floors.pooled_queries,
        ),
        check_floor(
            "pool_acquires",
            counts.pool_acquires.get(),
            floors.pool_acquire_releases,
        ),
        check_floor(
            "pool_releases",
            counts.pool_releases.get(),
            floors.pool_acquire_releases,
        ),
        check_floor(
            "large_payload_queries",
            counts.large_payload_queries.get(),
            floors.large_payload_queries,
        ),
        check_floor(
            "clean_connections",
            counts.clean_connections.get(),
            floors.clean_connections,
        ),
        check_floor(
            "bad_connections",
            counts.bad_connections.get(),
            floors.bad_connections,
        ),
        check_floor(
            "cancellations",
            counts.cancellations.get(),
            floors.cancellations,
        ),
        check_floor(
            "cancellation_recoveries",
            counts.cancellation_recoveries.get(),
            floors.cancellations,
        ),
        check_floor(
            "total_operations",
            counts.total_operations(),
            floors.total_operations,
        ),
        check_floor(
            "rss_samples",
            u64::try_from(samples.len()).unwrap_or(u64::MAX),
            u64::try_from(floors.rss_samples).unwrap_or(u64::MAX),
        ),
    ] {
        if let Err(error) = result {
            failures.push(error);
        }
    }

    for (worker, count) in counts.per_query_worker.iter().enumerate() {
        if let Err(error) = check_floor(
            &format!("pooled_query_worker_{worker}"),
            count.get(),
            floors.pooled_queries / u64::try_from(QUERY_WORKERS).unwrap_or(1),
        ) {
            failures.push(error);
        }
    }

    let peak_active = samples
        .iter()
        .map(|sample| sample.server_active_queries)
        .max()
        .unwrap_or(0);
    println!("floor name=peak_server_active_queries observed={peak_active} required=2");
    if peak_active < 2 {
        failures.push(format!(
            "server observed only {peak_active} simultaneous tagged pooled queries; need at least 2"
        ));
    }

    let rss_series = samples
        .iter()
        .map(|sample| sample.rss_kib.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!("rss_kib_series=[{rss_series}]");
    let nondecreasing = samples
        .windows(2)
        .all(|window| window[1].rss_kib >= window[0].rss_kib);
    let rises = samples
        .windows(2)
        .filter(|window| window[1].rss_kib > window[0].rss_kib)
        .count();
    let falls = samples
        .windows(2)
        .filter(|window| window[1].rss_kib < window[0].rss_kib)
        .count();
    let rss_delta_kib = match (samples.first(), samples.last()) {
        (Some(first), Some(last)) => i128::from(last.rss_kib) - i128::from(first.rss_kib),
        _ => 0,
    };
    println!(
        "rss_rule samples={} rises={} falls={} nondecreasing={} delta_kib={rss_delta_kib}",
        samples.len(),
        rises,
        falls,
        nondecreasing
    );
    if nondecreasing && rises > 0 {
        failures.push(format!(
            "RSS climbed monotonically across the measured series: rises={rises}, falls={falls}, delta_kib={rss_delta_kib}"
        ));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

async fn run(args: Args) -> Result<(), String> {
    let overall_started = Instant::now();
    let floors = Floors::for_args(&args)?;
    let duration = Duration::from_secs(args.duration_secs);
    let sample_interval = Duration::from_secs(args.sample_interval_secs);
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read wall clock for application tag: {error}"))?
        .as_secs();
    let application_name = format!("cpg_soak_{}_{}", std::process::id(), run_id);
    let observer_name = format!("{application_name}_observer");
    let initial_live = compio_postgres::live_connections();
    if initial_live != 0 {
        return Err(format!(
            "standalone soak started with {initial_live} live connections, expected 0"
        ));
    }

    println!(
        "configuration url={} duration_secs={} sample_interval_secs={} \
         query_workers={} pool_max_size={} pool_min_idle={} application_name={}",
        args.url,
        args.duration_secs,
        args.sample_interval_secs,
        QUERY_WORKERS,
        POOL_MAX_SIZE,
        POOL_MIN_IDLE,
        application_name
    );
    println!(
        "workload query_delay_seconds={QUERY_DELAY_SECONDS} \
         cancellation_delay_seconds={CANCELLATION_DELAY_SECONDS} large_payload_bytes={LARGE_PAYLOAD_BYTES}"
    );

    println!("phase=setup status=started watchdog={SETUP_WATCHDOG:?}");
    let setup = compio::time::timeout(SETUP_WATCHDOG, async {
        let observer_config = tagged_config(&args.url, &observer_name)?;
        let observer_transport = DirectTransport::resolve(observer_config)?;
        let observer = Rc::new(open_client(&observer_transport, "open soak observer").await?);
        let observer_live = compio_postgres::live_connections();
        if observer_live != initial_live + 1 {
            return Err(format!(
                "opening the observer changed live_connections from {initial_live} to {observer_live}, expected {}",
                initial_live + 1
            ));
        }

        let endpoint = watched_db(
            "read PostgreSQL endpoint identity",
            observer.query_one(
                "SELECT current_database(), current_user, inet_server_addr()::text, \
                        inet_server_port()::int4",
                &[],
            ),
        )
        .await?;
        let database: String = endpoint.get(0);
        let user: String = endpoint.get(1);
        let server_address: Option<String> = endpoint.get(2);
        let server_port: Option<i32> = endpoint.get(3);
        println!(
            "server_identity database={database} user={user} address={} port={}",
            server_address.as_deref().unwrap_or("local-socket"),
            server_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "none".to_owned())
        );

        let server_baseline = server_snapshot(&observer, &application_name).await?.backends;
        let connection_config = tagged_config(&args.url, &application_name)?;
        let direct_transport = DirectTransport::resolve(connection_config.clone())?;
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(POOL_MAX_SIZE)
            .min_idle(POOL_MIN_IDLE)
            .max_lifetime(Duration::from_secs(12))
            .acquire_timeout(Duration::from_secs(5))
            .command_timeout(Duration::from_millis(50));
        let pool = watched_db(
            "create soak pool",
            Pool::connect_with_config(connection_config.clone(), pool_config),
        )
        .await?;
        let pool = Rc::new(pool);
        pool.start_housekeeper();
        Ok::<_, String>((observer, observer_live, server_baseline, direct_transport, pool))
    })
    .await
    .map_err(|_| format!("setup exceeded its {SETUP_WATCHDOG:?} watchdog"))??;
    let (observer, observer_live_baseline, server_baseline, direct_transport, pool) = setup;
    println!(
        "phase=setup status=complete server_baseline={} driver_initial={} \
         driver_observer_baseline={} pool_total={}",
        server_baseline,
        initial_live,
        observer_live_baseline,
        pool.total_count()
    );

    println!(
        "phase=warmup status=started duration_ms={} watchdog={:?}",
        WARMUP.as_millis(),
        WARMUP + LOAD_GRACE
    );
    let warmup_counts = Rc::new(Counts::new());
    compio::time::timeout(
        WARMUP + LOAD_GRACE,
        run_load(
            WARMUP,
            Rc::clone(&pool),
            direct_transport.clone(),
            Rc::clone(&observer),
            Rc::clone(&warmup_counts),
        ),
    )
    .await
    .map_err(|_| format!("warmup exceeded its {:?} watchdog", WARMUP + LOAD_GRACE))??;
    if warmup_counts.pooled_queries.get() == 0
        || warmup_counts.large_payload_queries.get() == 0
        || warmup_counts.clean_connections.get() == 0
        || warmup_counts.bad_connections.get() == 0
        || warmup_counts.cancellations.get() == 0
        || warmup_counts.cancellation_recoveries.get() == 0
        || warmup_counts
            .per_query_worker
            .iter()
            .any(|count| count.get() == 0)
    {
        return Err(format!(
            "warmup did not exercise every path: pooled={} large_payload={} clean={} bad={} \
             cancellations={} recoveries={} per_query_worker={:?}",
            warmup_counts.pooled_queries.get(),
            warmup_counts.large_payload_queries.get(),
            warmup_counts.clean_connections.get(),
            warmup_counts.bad_connections.get(),
            warmup_counts.cancellations.get(),
            warmup_counts.cancellation_recoveries.get(),
            warmup_counts
                .per_query_worker
                .iter()
                .map(Cell::get)
                .collect::<Vec<_>>()
        ));
    }
    let warmup_rss_kib = read_rss_kib()?;
    println!(
        "phase=warmup status=complete pooled_queries={} clean_connections={} \
         bad_connections={} cancellations={} rss_kib={}",
        warmup_counts.pooled_queries.get(),
        warmup_counts.clean_connections.get(),
        warmup_counts.bad_connections.get(),
        warmup_counts.cancellations.get(),
        warmup_rss_kib
    );

    println!(
        "phase=measure status=started duration_secs={} watchdog={:?}",
        args.duration_secs,
        duration + LOAD_GRACE
    );
    let counts = Rc::new(Counts::new());
    let samples = Rc::new(std::cell::RefCell::new(Vec::with_capacity(
        floors.rss_samples,
    )));
    let sample_handle = compio::runtime::spawn(sample_series(
        floors.rss_samples,
        sample_interval,
        application_name.clone(),
        Rc::clone(&observer),
        Rc::clone(&counts),
        Rc::clone(&samples),
    ));
    compio::time::timeout(
        duration + LOAD_GRACE,
        run_load(
            duration,
            Rc::clone(&pool),
            direct_transport,
            Rc::clone(&observer),
            Rc::clone(&counts),
        ),
    )
    .await
    .map_err(|_| {
        format!(
            "measured load exceeded its {:?} watchdog",
            duration + LOAD_GRACE
        )
    })??;
    let sample_result = compio::time::timeout(SETTLE_WATCHDOG, sample_handle)
        .await
        .map_err(|_| format!("RSS sampler exceeded its {SETTLE_WATCHDOG:?} join watchdog"))?
        .map_err(|panic| format!("RSS sampler panicked: {}", panic_text(panic)))?;
    sample_result?;
    println!(
        "phase=measure status=complete operations={} samples={}",
        counts.total_operations(),
        samples.borrow().len()
    );

    println!("phase=shutdown status=started watchdog={SETTLE_WATCHDOG:?}");
    compio::time::timeout(SETTLE_WATCHDOG, pool.close())
        .await
        .map_err(|_| format!("pool close exceeded its {SETTLE_WATCHDOG:?} watchdog"))?;
    let final_server =
        wait_for_server_baseline(&observer, &application_name, server_baseline).await?;
    if final_server != server_baseline {
        return Err(format!(
            "server backend count did not return to baseline after {SETTLE_WATCHDOG:?}: baseline={server_baseline} observed={final_server}"
        ));
    }
    let final_with_observer = wait_for_driver_baseline(observer_live_baseline).await;
    if final_with_observer != observer_live_baseline {
        return Err(format!(
            "driver live connection count did not return to observer baseline after {SETTLE_WATCHDOG:?}: baseline={observer_live_baseline} observed={final_with_observer}"
        ));
    }

    let pool_connections_created = pool.metrics.connections_created.get();
    let pool_evictions = pool.metrics.evictions.get();
    let pool_timeouts = pool.metrics.timeouts.get();
    drop(pool);
    drop(observer);
    let drained = compio_postgres::drain_connections(SETTLE_WATCHDOG).await;
    let final_live = compio_postgres::live_connections();
    if !drained || final_live != initial_live {
        return Err(format!(
            "driver did not drain to its initial baseline after {SETTLE_WATCHDOG:?}: baseline={initial_live} observed={final_live} drained={drained}"
        ));
    }
    println!(
        "phase=shutdown status=complete server_baseline={} server_final={} \
         driver_initial={} driver_with_observer_final={} driver_final={}",
        server_baseline, final_server, initial_live, final_with_observer, final_live
    );

    let borrowed_samples = samples.borrow();
    rule_measurements(&counts, &floors, &borrowed_samples)?;
    let peak_server_backends = borrowed_samples
        .iter()
        .map(|sample| sample.server_backends)
        .max()
        .unwrap_or(server_baseline);
    let peak_driver_live = borrowed_samples
        .iter()
        .map(|sample| sample.driver_live_connections)
        .max()
        .unwrap_or(initial_live);
    println!(
        "counts pooled_queries={} pool_acquires={} pool_releases={} \
         large_payload_queries={} clean_connections={} bad_connections={} \
         cancellations={} cancellation_recoveries={} total_operations={}",
        counts.pooled_queries.get(),
        counts.pool_acquires.get(),
        counts.pool_releases.get(),
        counts.large_payload_queries.get(),
        counts.clean_connections.get(),
        counts.bad_connections.get(),
        counts.cancellations.get(),
        counts.cancellation_recoveries.get(),
        counts.total_operations()
    );
    println!(
        "pool connections_created={} evictions={} acquire_timeouts={} \
         peak_server_backends={} peak_driver_live_connections={} final_rss_kib={}",
        pool_connections_created,
        pool_evictions,
        pool_timeouts,
        peak_server_backends,
        peak_driver_live,
        read_rss_kib()?
    );
    println!(
        "soak result=ok elapsed_ms={}",
        overall_started.elapsed().as_millis()
    );
    Ok(())
}

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("configuration error: {error}");
            eprintln!("usage: soak [--url DSN] [--duration-secs N] [--sample-interval-secs N]");
            return ExitCode::from(2);
        }
    };
    let runtime = match compio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("create compio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            eprintln!("soak result=failed: {error}");
            eprintln!(
                "live_connections_at_failure={}",
                compio_postgres::live_connections()
            );
            ExitCode::FAILURE
        }
    }
}
