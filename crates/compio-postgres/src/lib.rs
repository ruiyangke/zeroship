// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! compio-postgres — a native, asynchronous PostgreSQL client for compio/io_uring.
//!
//! Port of [`tokio-postgres`](https://github.com/rust-postgres/rust-postgres), adapted to
//! compio's completion-based, owned-buffer I/O model. Client/Connection split preserved;
//! pipelining and async notifications preserved.
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
//! # Pipelining
//!
//! The client supports *pipelined* requests. Pipelining can improve performance in use cases in which multiple,
//! independent queries need to be executed. In a traditional workflow, each query is sent to the server after the
//! previous query completes. In contrast, pipelining allows the client to send all of the queries to the server up
//! front, minimizing time spent by one side waiting for the other to finish sending data.
//!
//! # SSL/TLS support
//!
//! TLS support is implemented via external libraries. `Client::connect` and `Config::connect` take a TLS implementation
//! as an argument. The `NoTls` type in this crate can be used when TLS is not required.

#![warn(rust_2018_idioms, clippy::all)]
#![allow(clippy::needless_lifetimes)]
#![allow(missing_debug_implementations)]
#![allow(dead_code)]

pub use crate::cancel_token::CancelToken;
pub use crate::client::Client;
pub use crate::config::Config;
pub use crate::connection::Connection;
pub use crate::copy_in::CopyInSink;
pub use crate::copy_out::CopyOutStream;
use crate::error::DbError;
pub use crate::error::Error;
pub use crate::generic_client::GenericClient;
pub use crate::pool::{Pool, PoolConfig, PoolMetrics, PooledClient};
pub use crate::portal::Portal;
pub use crate::query::RowStream;
pub use crate::row::{Row, SimpleQueryRow};
pub use crate::simple_query::{SimpleColumn, SimpleQueryStream};
pub use crate::socket::Socket;
pub use crate::statement::{Column, Statement};
pub use crate::tls::NoTls;
pub use crate::to_statement::ToStatement;
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
mod copy_in;
mod copy_out;
pub mod error;
mod generic_client;
#[cfg(not(target_arch = "wasm32"))]
mod keepalive;
mod maybe_tls_stream;
mod pool;
mod portal;
mod prepare;
mod query;
pub mod row;
mod simple_query;
mod socket;
mod statement;
pub mod tls;
mod to_statement;
mod transaction;
mod transaction_builder;
pub mod types;

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
) -> impl ExactSizeIterator<Item = &'a dyn ToSql> + 'a {
    s.iter().map(|s| *s as _)
}
