//! In-process prices for per-operation work that a live PostgreSQL round trip
//! makes invisible.
//!
//! This is an external Cargo target, while the measured types live in private
//! modules. As `buf_fill.rs` does for `buf_stream.rs`, this target compiles the
//! production source into the benchmark crate instead of widening the public
//! API for measurement. The cases below call the real implementation; their
//! small wrappers are labelled as proxies where they include an outer branch
//! or a benchmark-only control.

#![warn(rust_2018_idioms, clippy::all)]
#![allow(dead_code)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::nursery, clippy::pedantic)]
#![allow(missing_debug_implementations)]
#![allow(unused_imports)]

// Reproduce src/lib.rs's module root so crate-private hot-path methods remain
// crate-private while still being callable by this benchmark crate. Keeping
// the measured module bodies as #[path] modules makes production edits their
// source of truth; the copied crate-root plumbing below contains no measured
// implementation.
#[path = "../src/binary_copy.rs"]
pub mod binary_copy;
#[path = "../src/bind.rs"]
mod bind;
#[path = "../src/buf_stream.rs"]
mod buf_stream;
#[path = "../src/cancel_query.rs"]
mod cancel_query;
#[path = "../src/cancel_query_raw.rs"]
mod cancel_query_raw;
#[path = "../src/cancel_token.rs"]
mod cancel_token;
#[path = "../src/client.rs"]
pub(crate) mod client;
#[path = "../src/codec.rs"]
mod codec;
#[path = "../src/config.rs"]
pub mod config;
#[path = "../src/connect.rs"]
mod connect;
#[path = "../src/connect_raw.rs"]
mod connect_raw;
#[path = "../src/connect_socket.rs"]
mod connect_socket;
#[path = "../src/connect_tls.rs"]
mod connect_tls;
#[path = "../src/connection.rs"]
pub(crate) mod connection;
#[path = "../src/copy_in.rs"]
mod copy_in;
#[path = "../src/copy_out.rs"]
mod copy_out;
#[path = "../src/error/mod.rs"]
pub mod error;
#[path = "../src/generic_client.rs"]
mod generic_client;
#[cfg(not(target_arch = "wasm32"))]
#[path = "../src/keepalive.rs"]
mod keepalive;
#[path = "../src/live.rs"]
mod live;
#[path = "../src/maybe_tls_stream.rs"]
mod maybe_tls_stream;
#[path = "../src/pool.rs"]
mod pool;
#[path = "../src/portal.rs"]
mod portal;
#[path = "../src/prepare.rs"]
mod prepare;
#[path = "../src/query.rs"]
mod query;
#[path = "../src/release.rs"]
mod release;
#[path = "../src/replication.rs"]
pub mod replication;
#[path = "../src/row.rs"]
pub mod row;
#[path = "../src/simple_query.rs"]
mod simple_query;
#[path = "../src/socket.rs"]
mod socket;
#[path = "../src/statement.rs"]
mod statement;
#[cfg(feature = "test-utils")]
#[path = "../src/test_utils.rs"]
pub mod test_utils;
#[path = "../src/tls.rs"]
pub mod tls;
#[cfg(feature = "tls")]
#[path = "../src/tls_rustls.rs"]
pub mod tls_rustls;
#[path = "../src/to_statement.rs"]
mod to_statement;
#[path = "../src/transaction.rs"]
mod transaction;
#[path = "../src/transaction_builder.rs"]
mod transaction_builder;
#[path = "../src/types.rs"]
pub mod types;

pub use cancel_token::CancelToken;
pub use client::{Client, QueryEvent, QueryOutcome, TransactionStatus};
pub use config::Config;
pub use connection::Connection;
pub use copy_in::CopyInSink;
pub use copy_out::CopyOutStream;
use error::DbError;
pub use error::Error;
pub use fallible_iterator;
pub use generic_client::GenericClient;
pub use live::{drain_connections, live_connections};
pub use pool::{Pool, PoolConfig, PoolHookFuture, PoolMetrics, PooledClient};
pub use portal::Portal;
pub use query::RowStream;
pub use row::{Row, SimpleQueryRow};
pub use simple_query::{SimpleColumn, SimpleQueryStream};
pub use socket::Socket;
pub use statement::{Column, Statement};
pub use tls::NoTls;
#[cfg(feature = "tls")]
pub use tls_rustls::{MakeRustlsConnect, RustlsConnect, RustlsStream};
pub use to_statement::{ToStatement, Uncached};
pub use transaction::Transaction;
pub use transaction_builder::{IsolationLevel, TransactionBuilder};
use types::ToSql;

use std::sync::Arc;

/// An asynchronous notification.
#[derive(Clone, Debug)]
pub struct Notification {
    process_id: i32,
    channel: String,
    payload: String,
}

impl Notification {
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn payload(&self) -> &str {
        &self.payload
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AsyncMessage {
    Notice(DbError),
    Notification(Notification),
}

#[derive(Debug)]
#[non_exhaustive]
pub enum SimpleQueryMessage {
    Row(SimpleQueryRow),
    CommandComplete(u64),
    RowDescription(Arc<[SimpleColumn]>),
}

pub async fn connect<T>(
    config: &str,
    tls: T,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: tls::MakeTlsConnect<Socket>,
{
    let config = config.parse::<Config>()?;
    config.connect(tls).await
}

pub(crate) fn slice_iter<'a>(
    values: &'a [&'a (dyn ToSql + Sync)],
) -> impl ExactSizeIterator<Item = &'a (dyn ToSql + Sync)> + 'a {
    values.iter().copied()
}

use std::hint::black_box;
use std::io;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use client::{InnerClient, QueryObservation, StatementCacheAdmission, StatementCacheSettings};
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use config::{SslMode, SslNegotiation};
use connection::{Request, RequestMessages};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use futures_channel::mpsc;
use parking_lot::Mutex;
use postgres_protocol::message::frontend;

const SQL: &str = "SELECT 1";

/// The deadline pair never performs I/O, but `BufStream`'s generic bounds still
/// require a transport shape.
struct NoIo;

impl AsyncRead for NoIo {
    async fn read<B: IoBufMut>(&mut self, _buf: B) -> BufResult<usize, B> {
        unreachable!("the response-obligation benchmark must not read")
    }
}

impl AsyncWrite for NoIo {
    async fn write<B: IoBuf>(&mut self, _buf: B) -> BufResult<usize, B> {
        unreachable!("the response-obligation benchmark must not write")
    }

    async fn flush(&mut self) -> io::Result<()> {
        unreachable!("the response-obligation benchmark must not flush")
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        unreachable!("the response-obligation benchmark must not shut down")
    }
}

fn response_obligation_pair(
    stream: &buf_stream::BufStream<NoIo>,
) -> &buf_stream::BufStream<NoIo> {
    let stream = black_box(stream);
    stream.begin_read_response();
    stream.finish_read_response();
    stream
}

fn bench_read_deadline(c: &mut Criterion) {
    let none = buf_stream::BufStream::new(NoIo);
    let mut configured = buf_stream::BufStream::new(NoIo);
    configured.set_read_timeout(Some(Duration::from_secs(3600)));

    let mut group = c.benchmark_group("compio_postgres/hot_path/read_deadline_pair_proxy");
    for (label, stream) in [("none", &none), ("configured", &configured)] {
        group.bench_with_input(BenchmarkId::from_parameter(label), stream, |b, stream| {
            b.iter(|| black_box(response_obligation_pair(black_box(stream))));
        });
    }
    group.finish();
}

fn make_client(
    cache_capacity: usize,
    cache_threshold: NonZeroUsize,
) -> (Client, mpsc::UnboundedReceiver<Request>) {
    let (sender, receiver) = mpsc::unbounded();
    let client = Client::new_with_statement_cache(
        sender,
        SslMode::Disable,
        SslNegotiation::Postgres,
        0,
        0,
        None,
        StatementCacheSettings::new(cache_capacity, cache_threshold),
    );
    (client, receiver)
}

fn cache_admission_proxy(client: &InnerClient, query: &str) -> u8 {
    if client.statement_cache_capacity() == 0 {
        return 0;
    }

    match client.statement_cache_admission(query) {
        StatementCacheAdmission::Cached(_) => 1,
        StatementCacheAdmission::PrepareNamed => 2,
        StatementCacheAdmission::ExecuteUnnamed => 3,
    }
}

fn bench_statement_cache(c: &mut Criterion) {
    let threshold = NonZeroUsize::new(usize::MAX).expect("MAX is non-zero");
    let (disabled, _disabled_requests) = make_client(0, threshold);
    let (configured, _configured_requests) = make_client(8, threshold);

    // Measure the steady per-query candidate hit, not the one-time Arc/string
    // allocation that admits a new SQL string to probation.
    assert_eq!(cache_admission_proxy(configured.inner(), SQL), 3);

    let mut group = c.benchmark_group("compio_postgres/hot_path/cache_admission_proxy");
    for (label, client) in [("disabled", &disabled), ("configured", &configured)] {
        group.bench_with_input(BenchmarkId::from_parameter(label), client, |b, client| {
            b.iter(|| {
                let decision = cache_admission_proxy(black_box(client.inner()), black_box(SQL));
                black_box(decision)
            });
        });
    }
    group.finish();
}

struct ObservationFactory {
    client: Client,
    requests: mpsc::UnboundedReceiver<Request>,
    query: bytes::Bytes,
    _events: mpsc::UnboundedReceiver<QueryEvent>,
}

impl ObservationFactory {
    fn new() -> Self {
        let threshold = NonZeroUsize::new(usize::MAX).expect("MAX is non-zero");
        let (client, requests) = make_client(0, threshold);
        let events = client.query_events_with_threshold(Duration::MAX);
        let mut query = BytesMut::new();
        frontend::query(SQL, &mut query).expect("static query encodes");
        Self {
            client,
            requests,
            query: query.freeze(),
            _events: events,
        }
    }

    /// Build fresh per-query observer state outside Criterion's timed region.
    fn next(&mut self) -> QueryObservation {
        let responses = self
            .client
            .inner()
            .send(RequestMessages::Single(codec::FrontendMessage::Raw(
                self.query.clone(),
            )))
            .expect("request receiver is retained");
        let request = self
            .requests
            .try_recv()
            .expect("one request was sent while the channel was connected");
        let observation = request
            .observation
            .expect("installed observer produced an observation");
        assert!(observation.is_execution());
        drop(responses);
        observation
    }
}

/// The same observer-selection and below-threshold decision made for a
/// completed response batch, without decoding a batch around it.
fn observer_completion_proxy(observation: Option<&QueryObservation>) -> bool {
    let completed_at = (observation
        .as_ref()
        .is_some_and(|observation| observation.filters_by_elapsed()))
    .then(Instant::now);
    let execution = observation.filter(|observation| observation.is_execution());
    execution.is_some_and(|observation| {
        completed_at.is_some_and(|at| observation.filter_completed_at(at))
    })
}

fn bench_query_observer(c: &mut Criterion) {
    let mut factory = ObservationFactory::new();
    let mut group = c.benchmark_group("compio_postgres/hot_path/observer_completion_proxy");
    group.bench_function("none", |b| {
        b.iter_batched_ref(
            || None::<QueryObservation>,
            |observation| {
                let filtered = observer_completion_proxy(black_box(observation.as_ref()));
                black_box(filtered)
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("threshold_never_reports", |b| {
        b.iter_batched_ref(
            || Some(factory.next()),
            |observation| {
                let filtered = observer_completion_proxy(black_box(observation.as_ref()));
                black_box(filtered)
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

/// Control matching `with_buf`'s pre-Drop-guard implementation. The candidate
/// calls the production method; both lock a persistent `BytesMut`, run the same
/// closure, and clear it. Only the cleanup mechanism differs.
#[derive(Default)]
struct ExplicitClearBuffer {
    buffer: Mutex<BytesMut>,
}

impl ExplicitClearBuffer {
    fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buffer = self.buffer.lock();
        let result = f(&mut buffer);
        buffer.clear();
        result
    }
}

fn fill_scratch(buffer: &mut BytesMut) -> usize {
    buffer.extend_from_slice(black_box(SQL.as_bytes()));
    black_box(buffer.len())
}

fn bench_with_buf(c: &mut Criterion) {
    let threshold = NonZeroUsize::new(usize::MAX).expect("MAX is non-zero");
    let (client, _requests) = make_client(0, threshold);
    let control = ExplicitClearBuffer::default();
    let control = &control;
    let inner = client.inner().as_ref();

    // Remove first-use allocation from both cases.
    assert_eq!(control.with_buf(fill_scratch), SQL.len());
    assert_eq!(inner.with_buf(fill_scratch), SQL.len());

    let mut group = c.benchmark_group("compio_postgres/hot_path/with_buf_cleanup_proxy");
    group.bench_function("explicit_clear_control", |b| {
        b.iter(|| {
            let len = black_box(control).with_buf(fill_scratch);
            black_box(len)
        });
    });
    group.bench_function("drop_guard", |b| {
        b.iter(|| {
            let len = black_box(inner).with_buf(fill_scratch);
            black_box(len)
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_read_deadline,
    bench_statement_cache,
    bench_query_observer,
    bench_with_buf
);
criterion_main!(benches);
