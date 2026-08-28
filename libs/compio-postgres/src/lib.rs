// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! compio-postgres - a native, asynchronous PostgreSQL client for compio/io_uring.
//!
//! Port of [`tokio-postgres`](https://github.com/rust-postgres/rust-postgres), adapted to
//! compio's completion-based, owned-buffer I/O model. Client/Connection split preserved;
//! pipelining and async notifications preserved - over PLAINTEXT. A TLS stream
//! cannot be split into owned halves, so it runs a serialized loop that reads
//! no socket while idle: see
//! [`Connection::notifications`](crate::Connection::notifications) for what
//! that costs a LISTEN/NOTIFY subscriber.
//!
//! Licensed under MIT or Apache-2.0; see LICENSE files in the tokio-postgres repository.
//!
//! # Example
//!
//! ```no_run
//! use compio_postgres::{NoTls, Error};
//!
//! async fn example() -> Result<(), Error> {
//!     // Connect to the database.
//!     let (client, connection) =
//!         compio_postgres::connect("host=localhost user=postgres", NoTls).await?;
//!
//!     // The connection object performs the actual communication with the database,
//!     // so spawn it off to run on its own.
//!     compio::runtime::spawn(async move {
//!         if let Err(e) = connection.run().await {
//!             eprintln!("connection error: {}", e);
//!         }
//!     }).detach();
//!
//!     // Now we can execute a simple statement that just returns its parameter.
//!     let rows = client
//!         .query("SELECT $1::TEXT", &[&"hello world"])
//!         .await?;
//!
//!     // And then check that we got back the same string we sent over.
//!     let value: &str = rows[0].get(0);
//!     assert_eq!(value, "hello world");
//!
//!     Ok(())
//! }
//! ```
//!
//! # Behavior
//!
//! Calling a method like `Client::query` on its own does nothing. The associated request is not sent to the database
//! until the future returned by the method is first polled. Requests are executed in the order that they are first
//! polled, not in the order that their futures are created.
//!
//! Dropping an ordinary command future after polling abandons delivery to that
//! caller; it does not cancel the PostgreSQL command. The connection continues
//! draining that response through its `ReadyForQuery` terminator so later
//! pipelined responses stay in their registered FIFO slots. Use a
//! [`CancelToken`] when server-side cancellation is required. Dropping
//! [`Connection::run`] instead retires the whole protocol session and fails
//! every outstanding operation.
//!
//! # Pipelining
//!
//! The client supports *pipelined* requests. Pipelining can improve performance in use cases in which multiple,
//! independent queries need to be executed. In a traditional workflow, each query is sent to the server after the
//! previous query completes. In contrast, pipelining allows the client to send all of the queries to the server up
//! front, minimizing time spent by one side waiting for the other to finish sending data.
//!
//! # Client command deadlines
//!
//! [`PoolConfig::command_timeout`] and [`PooledClient::command`] provide the
//! automatic, genuinely cancelling command deadline. The pool is the natural
//! owner because [`CancelToken::cancel_query`] needs a TLS connector at cancel
//! time, while a bare [`Client`] deliberately does not retain that connector.
//! Keeping it out of `Client` also avoids making `Client` generic over transport.
//!
//! With a bare client, compose the timer and cancellation token explicitly.
//! The simplest honest policy is to consume and discard that client when the
//! timer wins. Cancellation waits for the postmaster to close its dedicated
//! connection, but that proves only that the request was consumed. It does not
//! prove the target connection has drained through `ReadyForQuery`:
//!
//! ```no_run
//! use compio_postgres::{Client, Error, NoTls, Row};
//! use std::time::Duration;
//!
//! #[derive(Debug)]
//! enum RunError {
//!     Deadline,
//!     Driver(Error),
//! }
//!
//! async fn query_with_deadline(client: Client) -> Result<(Client, Vec<Row>), RunError> {
//!     let mut query = Box::pin(client.query("SELECT pg_sleep(10)", &[]));
//!
//!     let outcome = compio::time::timeout(Duration::from_secs(1), query.as_mut()).await;
//!     // End the response consumer before either returning or cancelling.
//!     drop(query);
//!     match outcome {
//!         Ok(result) => result
//!             .map(|rows| (client, rows))
//!             .map_err(RunError::Driver),
//!         Err(_) => {
//!             let cancel_result = client
//!                 .cancel_token()
//!                 .cancel_query(NoTls)
//!                 .await;
//!             // Do not let a late CancelRequest race this backend's next
//!             // query; dropping the client deterministically retires it.
//!             drop(client);
//!             cancel_result.map_err(RunError::Driver)?;
//!             Err(RunError::Deadline)
//!         }
//!     }
//! }
//! ```
//!
//! `NoTls` above is correct only for a plaintext session; pass the connector
//! matching a TLS session instead. Advanced bare-client code can retain the
//! session by awaiting cancellation and then [`Client::check_connection`]. The
//! pool wrapper performs both barriers, restores an idle transaction state, and
//! otherwise retires the session.
//!
//! # Socket read deadlines
//!
//! [`Config::read_timeout`] is an opt-in, programmatic-only inactivity clock
//! on post-startup socket reads. It is armed while a protocol response is owed
//! and reset by every successful underlying read, so an idle plaintext
//! connection's background notification read does not spend the budget. If it
//! expires, the driver drops the possibly partial read and makes the protocol
//! session permanently non-reusable. Both the stalled operation and
//! [`Connection::run`] receive an [`Error`] for which
//! [`Error::is_read_timeout`] is true. Standard [`Socket`] connections also
//! synchronously shut down an owned descriptor; an arbitrary stream passed to
//! [`Config::connect_raw`] has no generic descriptor to shut down and is
//! instead dropped when `run` returns.
//!
//! This is not `tcp_user_timeout`, connection setup, pool acquisition, or a
//! whole-command deadline, and it sends no `CancelRequest`.
//!
//! # SSL/TLS support
//!
//! `Client::connect` and `Config::connect` take a TLS implementation as an argument. The `NoTls` type in this crate can
//! be used when TLS is not required.
//!
//! The `tls` Cargo feature adds `MakeRustlsConnect`, a rustls backend that reads its trust anchors from the
//! connection configuration (TLS version bounds, SNI policy, `sslrootcert`,
//! `sslcrl` / `sslcrldir`, plus `sslcertmode` and
//! `sslcert`/`sslkey`/`sslpassword` for client-certificate auth) and implements
//! `tls-server-end-point` channel binding. With that
//! feature on, [`Pool`] builds one automatically for a
//! connection configuration whose `sslmode` permits TLS; see the `tls_rustls` module (`src/tls_rustls.rs`) and
//! the `Transport` section of the `pool` module docs (`src/pool.rs`).
//!
//! # Opt-in behaviour worth knowing about
//!
//! These are off unless asked for, so the base client behaves like
//! tokio-postgres. Each carries a caveat that is easier to learn here than to
//! discover in production:
//!
//! - [`Config::target_session_attrs`] picks among several hosts by whether the
//!   session is writable or in recovery. `primary` and `standby` need
//!   hot-standby detection and are refused rather than approximated.
//! - [`Config::statement_cache_capacity`] caches prepared statements per
//!   connection, and [`Config::statement_cache_execution_threshold`] delays
//!   promotion until SQL repeats. Leave the cache off when connecting through a
//!   transaction-mode pooler, which does not keep one session per transaction.
//! - [`Client::query_events`] reports completed queries, and
//!   [`Client::query_events_with_threshold`] reports only slow ones. A
//!   threshold hides an N+1 built from many fast queries.

// `MakeRustlsConnect` and `tls_rustls` above are code spans, not intra-doc links, and must stay that way. The
// module carrying them is `#[cfg(feature = "tls")]`, so in a default-feature `cargo doc` there is no item for a
// link to resolve against and rustdoc emits `unresolved link`. tests/run_doc_gate.sh builds both feature
// configurations and allows zero unresolved links under `--all-features`.

#![warn(rust_2018_idioms, clippy::all)]
#![allow(clippy::needless_lifetimes)]
#![allow(missing_debug_implementations)]
#![allow(dead_code)]

pub use crate::buf_stream::SplitStream;
pub use crate::cancel_token::CancelToken;
pub use crate::client::{Client, QueryEvent, QueryOutcome, TransactionStatus};
pub use crate::config::Config;
pub use crate::connection::Connection;
pub use crate::copy_format::CopyFormat;
pub use crate::copy_in::CopyInSink;
pub use crate::copy_out::CopyOutStream;
use crate::error::DbError;
pub use crate::error::Error;
pub use crate::generic_client::GenericClient;
pub use crate::live::{drain_connections, live_connections};
pub use crate::maybe_tls_stream::{MaybeTlsReadHalf, MaybeTlsWriteHalf};
pub use crate::pool::{Pool, PoolConfig, PoolHookFuture, PoolMetrics, PooledClient};
pub use crate::portal::Portal;
pub use crate::query::RowStream;
pub use crate::row::{Row, SimpleQueryRow};
pub use crate::simple_query::{SimpleColumn, SimpleQueryFormat, SimpleQueryStream};
pub use crate::socket::Socket;
pub use crate::statement::{Column, Statement};
pub use crate::tls::NoTls;
#[cfg(feature = "tls")]
pub use crate::tls_rustls::{MakeRustlsConnect, RustlsConnect, RustlsStream};
#[cfg(feature = "tls")]
pub use crate::tls_sansio::{TlsReadHalf, TlsWriteHalf};
pub use crate::to_statement::{ToStatement, Uncached};
pub use crate::transaction::Transaction;
pub use crate::transaction_builder::{IsolationLevel, TransactionBuilder};
use crate::types::ToSql;
pub use fallible_iterator;
use std::sync::Arc;

pub mod binary_copy;
mod bind;
mod buf_stream;
mod cancel_query;
mod cancel_query_raw;
mod cancel_token;
pub(crate) mod client;
mod codec;
pub mod config;
mod connect;
mod connect_raw;
mod connect_socket;
mod connect_tls;
pub(crate) mod connection;
mod copy_format;
mod copy_in;
mod copy_out;
pub mod error;
mod escape;
mod generic_client;
#[cfg(not(target_arch = "wasm32"))]
mod keepalive;
mod live;
mod maybe_tls_stream;
mod passfile;
mod pool;
mod portal;
mod prepare;
mod query;
mod release;
pub mod replication;
pub mod row;
mod service;
mod simple_query;
mod socket;
mod statement;
pub mod tls;
#[cfg(feature = "tls")]
pub mod tls_rustls;
#[cfg(feature = "tls")]
mod tls_sansio;
mod to_statement;
mod transaction;
mod transaction_builder;
pub mod types;

// Test-only constructors for `Row` / `Statement` / `Column`, plus the
// serialized-loop transport the suite needs to reach that loop over
// plaintext.
//
// NOT behind a Cargo feature, deliberately. It was, and the flag split
// "the suite" into two different test sets: `cargo test -p compio-postgres`
// built 72 targets and 1628 tests, while the same command with
// `--features test-utils` built 73 and 1652. The 24 it silently dropped were
// all of `tests/serialized_loop.rs` - the fallback transport, i.e. exactly
// the code least likely to be covered another way. A flag that decides
// whether a whole transport is tested is worse than a doc-hidden module.
//
// `#[doc(hidden)]` keeps it off the published surface. That is the same
// trade `plugin-db` documents for its own bench-only items: still `pub`,
// because an external test or bench target cannot reach `pub(crate)`.
#[doc(hidden)]
pub mod test_utils;

/// An asynchronous notification.
#[derive(Clone, Debug)]
pub struct Notification {
    process_id: i32,
    channel: String,
    payload: String,
}

impl Notification {
    /// The process ID of the notifying backend process.
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// The name of the channel that the notify has been raised on.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The "payload" string passed from the notifying process.
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

/// An asynchronous message from the server.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AsyncMessage {
    /// A notice.
    ///
    /// Notices use the same format as errors, but aren't "errors" per-se.
    Notice(DbError),
    /// A notification.
    ///
    /// Connections can subscribe to notifications with the `LISTEN` command.
    Notification(Notification),
}

/// Message returned by the `SimpleQuery` stream.
#[derive(Debug)]
#[non_exhaustive]
pub enum SimpleQueryMessage {
    /// A row of data.
    Row(SimpleQueryRow),
    /// A statement in the query has completed.
    ///
    /// The number of rows modified or selected is returned.
    CommandComplete(u64),
    /// Column values of the proceeding row values
    RowDescription(Arc<[SimpleColumn]>),
}

/// A convenience function which parses a connection string and connects to the database.
///
/// See the documentation for [`Config`] for details on the connection string format.
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
    s: &'a [&'a (dyn ToSql + Sync)],
) -> impl ExactSizeIterator<Item = &'a (dyn ToSql + Sync)> + 'a {
    s.iter().copied()
}
