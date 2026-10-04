//! Throughput of one large result over the public query surface.
//!
//! The buffered read path (`BufStream::fill`, which grows the 8 KB read buffer
//! to hold a whole message) is private to the crate, so a benchmark cannot call
//! it. The public operation that exercises it is a query whose single field is
//! large enough to arrive over many socket reads: `SELECT repeat(...)` drives
//! `fill` through the same code a production row does. This measures that
//! operation against the configured server.
//!
//! The server is the one the suites dial, `compio_postgres_testkit::server`,
//! started in Docker and shared with every test process of the worktree.
//! Nothing here creates or drops schemas.

use std::hint::black_box;
use std::time::Duration;

use compio_postgres::{Client, Config, NoTls};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// The row sizes, in bytes, the server renders with `repeat`.
const SIZES: [usize; 2] = [1024 * 1024, 16 * 1024 * 1024];

fn test_url() -> String {
    compio_postgres_testkit::server::server().url()
}

/// Open a client and drive its connection for the life of the bench.
fn connect(rt: &compio::runtime::Runtime, url: &str) -> Client {
    rt.block_on(async {
        let config: Config = url.parse().expect("the fixture server URL is a valid DSN");
        let (client, connection) = config
            .connect(NoTls)
            .await
            .unwrap_or_else(|error| panic!("large-read benchmark could not reach {url}: {error}"));
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
    })
}

fn read_len(rt: &compio::runtime::Runtime, client: &Client, size: usize) -> usize {
    rt.block_on(async {
        let row = client
            .query_one("SELECT repeat('x', $1::int4)", &[&i32::try_from(size).expect("bench sizes fit in int4")])
            .await
            .expect("live repeat query failed");
        row.get::<_, String>(0).len()
    })
}

fn bench_large_read(c: &mut Criterion) {
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let url = test_url();
    let client = connect(&rt, &url);

    let mut group = c.benchmark_group("compio_postgres/large_read/select_repeat");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for size in SIZES {
        // Fail the bench setup, not the timed region, if the fixture cannot
        // actually produce the body the case claims to read.
        assert_eq!(read_len(&rt, &client, size), size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let len = read_len(&rt, black_box(&client), black_box(size));
                assert_eq!(len, size);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_large_read);
criterion_main!(benches);
