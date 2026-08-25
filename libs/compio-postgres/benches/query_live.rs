//! Hot-path cost of the opt-in per-operation work, measured against a live
//! server.
//!
//! THIS BENCHMARK CANNOT ANSWER THE QUESTION IT WAS BUILT FOR, and the
//! measurement that says so is worth more than the one it was meant to
//! produce. Measured 2026-08-21 on an otherwise idle machine, two runs of this
//! file at one commit, against one database, nothing changed in between:
//!
//!   mode                     run 1        run 2      drift
//!   baseline                 358.49 us    416.28 us   +16%
//!   cache_immediate          144.33 us    202.16 us   +40%
//!   cache_counting_only      317.25 us    332.43 us    +5%
//!   read_deadline_armed      342.01 us    337.53 us    -1%
//!   observer_never_reports   341.51 us    574.49 us   +68%
//!
//! Each iteration is a full round trip to PostgreSQL, so the sample is
//! dominated by server and socket variance. The costs this file set out to
//! price - an atomic load, a counter check, an Option test - are tens of
//! nanoseconds against ~350 us of round trip, a ratio near 1:10000. They are
//! not small here; they are INVISIBLE, and a run that reports them is
//! reporting noise. Three of the five modes came out FASTER than baseline in
//! run 1, which they cannot be, because they only add work.
//!
//! What survives: `cache_immediate` runs at 0.40 and 0.49 of baseline in the
//! two runs. That agrees in direction and rough size across the noise and
//! matches the mechanism (a named prepared statement is reused instead of
//! reparsed). Quote that one; do not quote the others.
//!
//! To price the per-operation work, write an IN-PROCESS microbenchmark of the
//! code path, the way `benches/buf_fill.rs` does. Do not add samples here: the
//! noise floor is a property of the round trip, not of the sample count.
//!
//! Four features added work to paths every query crosses, and each was
//! justified on correctness rather than cost: the socket read deadline
//! consults an obligation counter on the read poll, the statement cache's
//! execution threshold does a candidate lookup per query, and the query
//! observer decides per completion whether to build an event.
//!
//! Every mode is a case in ONE criterion group so a single run compares them
//! against each other on one machine, in one process, with criterion's own
//! variance estimate. Selecting a mode with an environment variable would
//! measure each in a separate process and leave the comparison to whoever read
//! the numbers.
//!
//! `PG_TEST_URL` selects the server; absent, the default DSN below is used.
//! Nothing here creates or drops schemas: the statement `SELECT 1` needs none,
//! and a benchmark that mutates the database measures the mutation.

use std::hint::black_box;
use std::time::Duration;

use compio_postgres::{Client, Config, NoTls};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// The default when `PG_TEST_URL` is absent, matching the test suites.
const DEFAULT_URL: &str = "postgres://postgres:zeroship@127.0.0.1:5455/zeroship";

/// The sealed key enum this crate reads the environment through, shared with
/// the test suites rather than copied.
///
/// It was copied here, and the copy was the problem: the workspace gate
/// recognises the sanctioned accessor BY PATH
/// (`libs/<crate>/tests/common/env.rs`), so a local re-implementation of the
/// same shape is an unsanctioned raw read plus an illicit `allow`, and
/// `crates/core/tests/config_env_access_gate.rs` failed on exactly that.
#[path = "../tests/common/env.rs"]
mod env;

fn test_url() -> String {
    env::get(env::TestEnvKey::PgTestUrl).unwrap_or_else(|| DEFAULT_URL.to_owned())
}

/// What a case turns on. Each isolates one feature's per-operation cost
/// against `Baseline`, which is the driver as a caller gets it by default.
#[derive(Clone, Copy)]
enum Mode {
    /// No cache, no deadline, no observer.
    Baseline,
    /// Cache on, promoting on first use, so the named statement is reused.
    CacheImmediate,
    /// Cache on with a threshold no run reaches, so every execution stays
    /// unnamed and the measurement is the counter, not the reuse.
    CacheCountingOnly,
    /// A read deadline long enough that no query can trip it, so the
    /// measurement is the obligation bookkeeping, not a timeout.
    ReadDeadlineArmed,
    /// An observer whose threshold no query can clear, so the measurement is
    /// the per-completion decision, not event construction.
    ObserverNeverReports,
}

impl Mode {
    const fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::CacheImmediate => "cache_immediate",
            Self::CacheCountingOnly => "cache_counting_only",
            Self::ReadDeadlineArmed => "read_deadline_armed",
            Self::ObserverNeverReports => "observer_never_reports",
        }
    }

    fn config(self, url: &str) -> Config {
        let mut config: Config = url.parse().expect("PG_TEST_URL is not a valid DSN");
        match self {
            // The observer is installed on the Client after connecting, not
            // configured here, so its case leaves Config at the default.
            Self::Baseline | Self::ObserverNeverReports => {}
            Self::CacheImmediate => {
                config.statement_cache_capacity(8);
            }
            Self::CacheCountingOnly => {
                config.statement_cache_capacity(8);
                config.statement_cache_execution_threshold(
                    std::num::NonZeroUsize::new(usize::MAX).expect("MAX is non-zero"),
                );
            }
            Self::ReadDeadlineArmed => {
                config.read_timeout(Duration::from_secs(3600));
            }
        }
        config
    }
}

/// Open a client and spawn its driver, returning the client and the observer
/// receiver the mode asked for. The receiver must outlive the client: dropping
/// it uninstalls the observer, which would silently turn that case into the
/// baseline.
fn connect(
    rt: &compio::runtime::Runtime,
    mode: Mode,
    url: &str,
) -> (Client, Option<futures_channel::mpsc::UnboundedReceiver<compio_postgres::QueryEvent>>) {
    rt.block_on(async {
        let (client, connection) = mode
            .config(url)
            .connect(NoTls)
            .await
            .unwrap_or_else(|error| panic!("live benchmark could not reach {url}: {error}"));
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();

        let events = match mode {
            Mode::ObserverNeverReports => Some(client.query_events_with_threshold(Duration::MAX)),
            _ => None,
        };
        (client, events)
    })
}

fn bench_query_hot_path(c: &mut Criterion) {
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let url = test_url();

    let mut group = c.benchmark_group("compio_postgres/query/select_1");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for mode in [
        Mode::Baseline,
        Mode::CacheImmediate,
        Mode::CacheCountingOnly,
        Mode::ReadDeadlineArmed,
        Mode::ObserverNeverReports,
    ] {
        let (client, _events) = connect(&rt, mode, &url);
        group.bench_with_input(
            BenchmarkId::from_parameter(mode.label()),
            &client,
            |b, client| {
                b.iter(|| {
                    let value: i32 = rt.block_on(async {
                        let row = client
                            .query_one("SELECT 1::int4", &[])
                            .await
                            .expect("live SELECT failed");
                        row.get(0)
                    });
                    assert_eq!(black_box(value), 1);
                });
            },
        );
        // `_events` is dropped here, after its case has finished measuring.
    }

    group.finish();
}

criterion_group!(benches, bench_query_hot_path);
criterion_main!(benches);
