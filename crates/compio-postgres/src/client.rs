// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Phase 4 surface: all user-facing query/prepare/execute/simple_query
// methods now dispatch to the real `query.rs`/`prepare.rs`/`simple_query.rs`
// modules. Transaction and copy methods are stubbed until Phase 5 lands
// `transaction.rs`, `copy_in.rs`, and `copy_out.rs`.

use crate::codec::{BackendMessages, FrontendMessage};
use crate::config::{SslMode, SslNegotiation};
use crate::connection::{Request, RequestMessages};
use crate::copy_in::CopyInSink;
use crate::copy_out::CopyOutStream;
use crate::keepalive::KeepaliveConfig;
use crate::query::RowStream;
use crate::simple_query::SimpleQueryStream;
use crate::tls::{MakeTlsConnect, TlsConnect};
use crate::types::{Oid, ToSql, Type};
use crate::{
    CancelToken, Error, Row, SimpleQueryMessage, Socket, Statement, ToStatement, Transaction,
    TransactionBuilder, copy_in, copy_out, prepare, query, simple_query, slice_iter,
};
use bytes::{Buf, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use postgres_types::{BorrowToSql, FromSqlOwned};
use std::collections::HashMap;
use std::fmt;
use std::future;
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

/// A stream of backend messages for a single in-flight request.
///
/// Yields messages one at a time via `next().await`. Internally the
/// connection task sends one `BackendMessages` batch per `RowDescription +
/// DataRow* + CommandComplete + ReadyForQuery` group, and `Responses`
/// flattens the inner `FallibleIterator` so the caller sees a linear
/// message stream. Matches tokio-postgres's `Responses` type.
pub struct Responses {
    receiver: mpsc::Receiver<BackendMessages>,
    cur: BackendMessages,
}

impl Responses {
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<Message, Error>> {
        loop {
            match self.cur.next().map_err(Error::parse)? {
                Some(Message::ErrorResponse(body)) => return Poll::Ready(Err(Error::db(body))),
                Some(message) => return Poll::Ready(Ok(message)),
                None => {}
            }

            match ready!(self.receiver.poll_next_unpin(cx)) {
                Some(messages) => self.cur = messages,
                None => return Poll::Ready(Err(Error::closed())),
            }
        }
    }

    /// Pull the next backend message from the response stream.
    pub async fn next(&mut self) -> Result<Message, Error> {
        future::poll_fn(|cx| self.poll_next(cx)).await
    }
}

/// A cache of type info and prepared statements for fetching type info.
#[derive(Default)]
pub(crate) struct CachedTypeInfo {
    /// A statement for basic information for a type from its OID.
    pub(crate) typeinfo: Option<Statement>,
    /// A statement for getting information for a composite type from its OID.
    pub(crate) typeinfo_composite: Option<Statement>,
    /// A statement for getting information for an enum type from its OID.
    pub(crate) typeinfo_enum: Option<Statement>,
    /// Cache of types already looked up.
    pub(crate) types: HashMap<Oid, Type>,
}

/// Shared inner state of a `Client`. Lives behind an `Arc` so helpers
/// like `bind`, `prepare`, and the Drop impls on `Statement`/`Portal`
/// can keep a `Weak<InnerClient>` back-reference.
pub struct InnerClient {
    sender: mpsc::UnboundedSender<Request>,
    cached_typeinfo: Mutex<CachedTypeInfo>,

    /// Scratch buffer for encoding frontend messages. `with_buf` locks
    /// this, hands the caller a `&mut BytesMut`, and clears on drop so
    /// the next caller sees a fresh buffer.
    buffer: Mutex<BytesMut>,

    /// Set when a fire-and-forget ROLLBACK has been queued (e.g. by
    /// `Transaction::drop`) but not yet observed to completion. The pool
    /// uses this at checkout time to run a barrier (`simple_query("")`)
    /// that drains the pending ROLLBACK before the connection is handed
    /// to the next caller. Cleared once the pool confirms the connection
    /// is clean, or on explicit commit/rollback (which await completion
    /// synchronously).
    ///
    /// `AtomicBool` with `Relaxed` ordering: the pool and the Client are
    /// on the same thread in the current usage model, but `Client` is
    /// nominally `Send`, so an atomic keeps the bound clean without
    /// invoking UB on a future cross-thread move.
    dirty: AtomicBool,
}

impl InnerClient {
    /// Send a batch of frontend messages to the connection task. Returns
    /// a `Responses` stream the caller drains with `next().await`.
    ///
    /// The connection task owns the socket; this method only enqueues
    /// into the unbounded `UnboundedSender<Request>` channel. Failure
    /// means the connection task has terminated (socket closed).
    pub fn send(&self, messages: RequestMessages) -> Result<Responses, Error> {
        let (sender, receiver) = mpsc::channel(1);
        let request = Request { messages, sender };
        self.sender
            .unbounded_send(request)
            .map_err(|_| Error::closed())?;

        Ok(Responses {
            receiver,
            cur: BackendMessages::empty(),
        })
    }

    pub(crate) fn typeinfo(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo.clone()
    }

    pub(crate) fn set_typeinfo(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo = Some(statement.clone());
    }

    pub(crate) fn typeinfo_composite(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo_composite.clone()
    }

    pub(crate) fn set_typeinfo_composite(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo_composite = Some(statement.clone());
    }

    pub(crate) fn typeinfo_enum(&self) -> Option<Statement> {
        self.cached_typeinfo.lock().typeinfo_enum.clone()
    }

    pub(crate) fn set_typeinfo_enum(&self, statement: &Statement) {
        self.cached_typeinfo.lock().typeinfo_enum = Some(statement.clone());
    }

    pub(crate) fn type_(&self, oid: Oid) -> Option<Type> {
        self.cached_typeinfo.lock().types.get(&oid).cloned()
    }

    pub(crate) fn set_type(&self, oid: Oid, type_: &Type) {
        self.cached_typeinfo.lock().types.insert(oid, type_.clone());
    }

    pub(crate) fn clear_type_cache(&self) {
        self.cached_typeinfo.lock().types.clear();
    }

    /// Lock the shared scratch buffer, run `f`, and clear the buffer on
    /// exit. Used by encoders that want to build a frontend message
    /// without allocating a fresh `BytesMut` each time.
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buffer = self.buffer.lock();
        let r = f(&mut buffer);
        buffer.clear();
        r
    }

    /// Mark the connection as "dirty" — a fire-and-forget message (e.g.
    /// ROLLBACK queued from `Transaction::drop`) is pending but not yet
    /// observed to completion.
    pub(crate) fn set_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Clear the dirty flag. Called after a confirmed-complete command
    /// (explicit `commit`/`rollback`) or after the pool's checkout barrier
    /// drains any pending fire-and-forget message.
    pub(crate) fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Relaxed);
    }

    /// Is a fire-and-forget message pending? If true, the pool must run a
    /// barrier (`simple_query("")`) before treating the connection as
    /// clean.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// Captured by [`CancelToken`] so a cancel request can reach the same
/// backend the original connection did. Phase 4's `cancel_query.rs`
/// reconnects using these fields.
#[derive(Clone)]
pub(crate) struct SocketConfig {
    pub addr: Addr,
    pub hostname: Option<String>,
    pub port: u16,
    pub connect_timeout: Option<Duration>,
    pub tcp_user_timeout: Option<Duration>,
    pub keepalive: Option<KeepaliveConfig>,
}

/// Resolved transport endpoint: either a concrete IP or a Unix socket
/// directory. Used by `connect_socket.rs` to pick the right stream type.
#[derive(Clone)]
pub(crate) enum Addr {
    Tcp(IpAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

/// An asynchronous PostgreSQL client handle.
///
/// The client is one half of what is returned when a connection is
/// established. Users interact with the database through this client;
/// the other half is the [`Connection`](crate::Connection) which drives
/// I/O on a background task.
pub struct Client {
    inner: Arc<InnerClient>,
    socket_config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    process_id: i32,
    secret_key: i32,
}

impl Client {
    pub(crate) fn new(
        sender: mpsc::UnboundedSender<Request>,
        ssl_mode: SslMode,
        ssl_negotiation: SslNegotiation,
        process_id: i32,
        secret_key: i32,
    ) -> Client {
        Client {
            inner: Arc::new(InnerClient {
                sender,
                cached_typeinfo: Default::default(),
                buffer: Default::default(),
                dirty: AtomicBool::new(false),
            }),
            socket_config: None,
            ssl_mode,
            ssl_negotiation,
            process_id,
            secret_key,
        }
    }

    pub(crate) fn inner(&self) -> &Arc<InnerClient> {
        &self.inner
    }

    pub(crate) fn set_socket_config(&mut self, socket_config: SocketConfig) {
        self.socket_config = Some(socket_config);
    }

    /// Creates a new prepared statement.
    ///
    /// Prepared statements can be executed repeatedly, and may contain query parameters (indicated by `$1`, `$2`, etc),
    /// which are set when executed. Prepared statements can only be used with the connection that created them.
    pub async fn prepare(&self, query: &str) -> Result<Statement, Error> {
        self.prepare_typed(query, &[]).await
    }

    /// Like `prepare`, but allows the types of query parameters to be explicitly specified.
    ///
    /// The list of types may be smaller than the number of parameters - the types of the remaining parameters will be
    /// inferred. For example, `client.prepare_typed(query, &[])` is equivalent to `client.prepare(query)`.
    pub async fn prepare_typed(
        &self,
        query: &str,
        parameter_types: &[Type],
    ) -> Result<Statement, Error> {
        prepare::prepare(&self.inner, query, parameter_types).await
    }

    /// Executes a statement, returning a vector of the resulting rows.
    ///
    /// A statement may contain parameters, specified by `$n`, where `n` is the index of the parameter of the list
    /// provided, 1-indexed.
    ///
    /// The `statement` argument can either be a `Statement`, or a raw query string. If the same statement will be
    /// repeatedly executed (perhaps with different query parameters), consider preparing the statement up front
    /// with the `prepare` method.
    pub async fn query<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.query_raw(statement, slice_iter(params))
            .await?
            .try_collect()
            .await
    }

    /// Returns a vector of scalars.
    pub async fn query_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<R>, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        let rows: Vec<Row> = self
            .query_raw(statement, slice_iter(params))
            .await?
            .try_collect()
            .await?;

        if let Some(row) = rows.first() {
            if row.len() != 1 {
                return Err(Error::column_count());
            }
        };

        rows.into_iter().map(|r| r.try_get(0)).collect()
    }

    /// Executes a statement which returns a single row, returning it.
    ///
    /// Returns an error if the query does not return exactly one row.
    pub async fn query_one<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.query_opt(statement, params)
            .await
            .and_then(|res| res.ok_or_else(Error::row_count))
    }

    /// Like [`Client::query_one`] but returns one scalar.
    pub async fn query_one_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<R, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        let row = self.query_one(statement, params).await?;

        if row.len() != 1 {
            return Err(Error::column_count());
        }

        row.try_get(0)
    }

    /// Executes a statements which returns zero or one rows, returning it.
    ///
    /// Returns an error if the query returns more than one row.
    pub async fn query_opt<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        let mut stream = pin!(self.query_raw(statement, slice_iter(params)).await?);

        let mut first = None;
        while let Some(row) = stream.try_next().await? {
            if first.is_some() {
                return Err(Error::row_count());
            }

            first = Some(row);
        }

        Ok(first)
    }

    /// Like [`Client::query_opt`] but returns an optional scalar.
    pub async fn query_opt_scalar<R: FromSqlOwned, T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<R>, Error>
    where
        T: ?Sized + ToStatement + fmt::Debug,
    {
        let row = self.query_opt(statement, params).await?;

        if let Some(row) = &row {
            if row.len() != 1 {
                return Err(Error::column_count());
            }
        }

        row.map(|x| x.try_get::<_, R>(0)).transpose()
    }

    /// The maximally flexible version of [`query`].
    ///
    /// [`query`]: #method.query
    pub async fn query_raw<T, P, I>(&self, statement: &T, params: I) -> Result<RowStream, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let statement = statement.__convert().into_statement(&self.inner).await?;
        query::query(&self.inner, statement, params).await
    }

    /// Like `query`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed(
        &self,
        query: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, Error> {
        self.query_typed_raw(query, params.iter().map(|(v, t)| (*v, t.clone())))
            .await?
            .try_collect()
            .await
    }

    /// Run a query with text-format string parameters.
    ///
    /// Parse is sent with no type hints — the server infers each parameter's
    /// type from its position in the SQL. This matches the legacy
    /// `zeroship-pg` `query_text_params` API and is what JSON-driven query
    /// builders rely on (they pass everything as strings and expect
    /// `UPDATE t SET c = $1` to implicit-cast regardless of `c`'s column
    /// type). Results come back in binary format.
    ///
    /// ```no_run
    /// # async fn demo(client: &compio_postgres::Client) -> Result<(), compio_postgres::Error> {
    /// let rows = client
    ///     .query_text_params(
    ///         "SELECT * FROM users WHERE id = $1::uuid",
    ///         &["550e8400-e29b-41d4-a716-446655440000"],
    ///     )
    ///     .await?;
    /// # let _ = rows; Ok(()) }
    /// ```
    pub async fn query_text_params(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Row>, Error> {
        query::query_text_params(&self.inner, sql, params)
            .await?
            .try_collect()
            .await
    }

    /// Like `query_one`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed_one(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Row, Error> {
        self.query_typed_opt(statement, params)
            .await
            .and_then(|res| res.ok_or_else(Error::row_count))
    }

    /// Like `query_opt`, but requires the types of query parameters to be explicitly specified.
    pub async fn query_typed_opt(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Option<Row>, Error> {
        let mut stream = pin!(
            self.query_typed_raw(statement, params.iter().map(|(v, t)| (*v, t.clone())))
                .await?
        );

        let mut first = None;
        while let Some(row) = stream.try_next().await? {
            if first.is_some() {
                return Err(Error::row_count());
            }

            first = Some(row);
        }

        Ok(first)
    }

    /// The maximally flexible version of [`query_typed`].
    ///
    /// [`query_typed`]: #method.query_typed
    pub async fn query_typed_raw<P, I>(&self, query: &str, params: I) -> Result<RowStream, Error>
    where
        P: BorrowToSql,
        I: IntoIterator<Item = (P, Type)>,
    {
        query::query_typed(&self.inner, query, params).await
    }

    /// Executes a statement, returning the number of rows modified.
    pub async fn execute<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.execute_raw(statement, slice_iter(params)).await
    }

    /// Like `execute`, but requires the types of query parameters to be explicitly specified.
    pub async fn execute_typed(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<u64, Error> {
        query::execute_typed(
            &self.inner,
            statement,
            params.iter().map(|(v, t)| (*v, t.clone())),
        )
        .await
    }

    /// The maximally flexible version of [`execute`].
    ///
    /// [`execute`]: #method.execute
    pub async fn execute_raw<T, P, I>(&self, statement: &T, params: I) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let statement = statement.__convert().into_statement(&self.inner).await?;
        query::execute(self.inner(), statement, params).await
    }

    /// Executes a `COPY FROM STDIN` statement, returning a sink used to write the copy data.
    ///
    /// PostgreSQL does not support parameters in `COPY` statements. The copy *must* be explicitly
    /// completed via the `Sink::close` or `finish` methods. If it is not, the copy will be aborted.
    pub async fn copy_in<T, U>(&self, statement: &T) -> Result<CopyInSink<U>, Error>
    where
        T: ?Sized + ToStatement,
        U: Buf + 'static + Send,
    {
        let statement = statement.__convert().into_statement(&self.inner).await?;
        copy_in::copy_in(self.inner(), statement).await
    }

    /// Executes a `COPY TO STDOUT` statement, returning a stream of the resulting data.
    pub async fn copy_out<T>(&self, statement: &T) -> Result<CopyOutStream, Error>
    where
        T: ?Sized + ToStatement,
    {
        let statement = statement.__convert().into_statement(&self.inner).await?;
        copy_out::copy_out(self.inner(), statement).await
    }

    /// Executes a sequence of SQL statements using the simple query protocol, returning the resulting rows.
    ///
    /// Statements should be separated by semicolons. If an error occurs, execution of the sequence will stop at that
    /// point. The simple query protocol returns the values in rows as strings rather than in their binary encodings,
    /// so the associated row type doesn't work with the `FromSql` trait.
    pub async fn simple_query(&self, query: &str) -> Result<Vec<SimpleQueryMessage>, Error> {
        self.simple_query_raw(query).await?.try_collect().await
    }

    /// Like `simple_query`, but returns a stream rather than eagerly collecting.
    pub async fn simple_query_raw(&self, query: &str) -> Result<SimpleQueryStream, Error> {
        simple_query::simple_query(self.inner(), query).await
    }

    /// Executes a sequence of SQL statements using the simple query protocol.
    ///
    /// Statements should be separated by semicolons. If an error occurs, execution of the sequence will stop at that
    /// point. This is intended for use when, for example, initializing a database schema.
    pub async fn batch_execute(&self, query: &str) -> Result<(), Error> {
        simple_query::batch_execute(self.inner(), query).await
    }

    /// Check that the connection is alive and wait for the confirmation.
    pub async fn check_connection(&self) -> Result<(), Error> {
        query::sync(self.inner()).await
    }

    /// Begins a new database transaction.
    ///
    /// The transaction will roll back by default - use the `commit` method to commit it.
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self.build_transaction().start().await
    }

    /// Returns a builder for a transaction with custom settings.
    pub fn build_transaction(&mut self) -> TransactionBuilder<'_> {
        TransactionBuilder::new(self)
    }

    /// Returns the process ID of the server backend connected to this
    /// client.
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// Constructs a cancellation token that can later be used to request
    /// cancellation of a query running on this connection.
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            socket_config: self.socket_config.clone(),
            ssl_mode: self.ssl_mode,
            ssl_negotiation: self.ssl_negotiation,
            process_id: self.process_id,
            secret_key: self.secret_key,
        }
    }

    /// Attempts to cancel an in-progress query.
    ///
    /// The server provides no information about whether a cancellation attempt was successful or not. An error will
    /// only be returned if the client was unable to connect to the database.
    #[deprecated(since = "0.6.0", note = "use Client::cancel_token() instead")]
    pub async fn cancel_query<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        self.cancel_token().cancel_query(tls).await
    }

    /// Like `cancel_query`, but uses a stream which is already connected to the server rather than opening a new
    /// connection itself.
    #[deprecated(since = "0.6.0", note = "use Client::cancel_token() instead")]
    pub async fn cancel_query_raw<S, T>(&self, stream: S, tls: T) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        self.cancel_token().cancel_query_raw(stream, tls).await
    }

    /// Clears the client's type information cache.
    ///
    /// When user-defined types are used in a query, the client loads their definitions from the database and caches
    /// them for the lifetime of the client. If those definitions are changed in the database, this method can be used
    /// to flush the local cache and allow the new, updated definitions to be loaded.
    pub fn clear_type_cache(&self) {
        self.inner().clear_type_cache();
    }

    /// Determines if the connection to the server has already closed.
    /// In that case, all future queries will fail.
    pub fn is_closed(&self) -> bool {
        self.inner.sender.is_closed()
    }

    /// Is a fire-and-forget command (e.g. ROLLBACK from `Transaction::drop`)
    /// pending on this client? Used by the pool to decide whether a checkout
    /// barrier is required.
    pub fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    /// Clear the dirty flag. The pool calls this after a checkout barrier
    /// (or after an explicit commit/rollback confirms the connection is in
    /// a clean state).
    pub(crate) fn clear_dirty(&self) {
        self.inner.clear_dirty();
    }

    #[doc(hidden)]
    pub fn __private_api_rollback(&self, name: Option<&str>) {
        // Mark the connection dirty *before* firing the rollback so that a
        // concurrent pool return sees the flag even if the send itself fails
        // (in which case `is_closed()` will also be true, but the flag
        // remains correct).
        self.inner.set_dirty();

        let buf = self.inner().with_buf(|buf| {
            let sql = match name {
                Some(name) => format!("ROLLBACK TO {name}"),
                None => "ROLLBACK".to_string(),
            };
            // H6: Don't panic on NUL in savepoint names. `frontend::query`
            // returns Err if the SQL contains an interior NUL byte; in that
            // case we emit a log line and return an empty buffer — the
            // connection will remain dirty (no ROLLBACK actually queued) and
            // the pool's next-get barrier will still detect + evict it.
            if let Err(e) = frontend::query(&sql, buf) {
                log::error!("compio-postgres: failed to encode ROLLBACK: {e}");
                buf.clear();
            }
            buf.split().freeze()
        });

        // Empty buf => encoding failed; skip the send (nothing to send). The
        // dirty flag remains set so the pool will run the barrier on the
        // next checkout.
        if buf.is_empty() {
            return;
        }

        let _ = self
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(buf)));
    }

    #[doc(hidden)]
    pub fn __private_api_close(&mut self) {
        self.inner.sender.close_channel()
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish()
    }
}
