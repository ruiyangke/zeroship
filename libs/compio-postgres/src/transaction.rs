// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Near-verbatim port of tokio-postgres's `transaction.rs`. The only
// adaptation is the `tokio::io::{AsyncRead, AsyncWrite}` bound on the
// deprecated `cancel_query_raw` method - swapped for compio's counterparts.

use crate::Socket;
use crate::copy_out::CopyOutStream;
use crate::query::RowStream;
use crate::tls::MakeTlsConnect;
use crate::tls::TlsConnect;
use crate::types::{BorrowToSql, ToSql, Type};
use crate::{
    CancelToken, Client, CopyInSink, Error, Portal, Row, SimpleQueryMessage, Statement,
    ToStatement, bind, query, slice_iter,
};
use bytes::Buf;
use compio::io::{AsyncRead, AsyncWrite};
use futures_util::TryStreamExt;

/// A representation of a PostgreSQL database transaction.
///
/// Transactions will implicitly roll back when dropped. Use the `commit`
/// method to commit the changes made in the transaction. Transactions can
/// be nested, with inner transactions implemented via savepoints.
pub struct Transaction<'a> {
    client: &'a mut Client,
    savepoint: Option<Savepoint>,
    done: bool,
}

/// A representation of a PostgreSQL database savepoint.
struct Savepoint {
    name: String,
    depth: u32,
}

/// Undoes a SAVEPOINT whose creation was abandoned before any `Transaction`
/// took ownership of the name.
struct SavepointCreationGuard<'a> {
    client: &'a Client,
    name: &'a str,
    done: bool,
}

impl Drop for SavepointCreationGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.client.__private_api_rollback(Some(self.name));
        }
    }
}

/// The command tag PostgreSQL answers with when it rolled a transaction back.
///
/// A `COMMIT` inside an aborted transaction block completes with THIS tag
/// rather than `COMMIT`, and with no `ErrorResponse`: the statement succeeded,
/// and every change it was asked to make is gone.
const ROLLBACK_TAG: &str = "ROLLBACK";

use crate::escape::quote_identifier;

/// The SQL that ends a savepoint's scope: undo its work, then take the name
/// back off the server's savepoint stack.
///
/// `ROLLBACK TO SAVEPOINT` deliberately LEAVES the savepoint defined - that is
/// what makes "roll back to it again later" possible, and it is the documented
/// behaviour, not a quirk. But a `Transaction` whose `rollback` has been
/// called is finished: its Rust value is consumed and no later call can name
/// it. Leaving the name defined lets it outlive the scope that owned it, and
/// PostgreSQL resolves a savepoint name to the MOST RECENTLY established one,
/// so a leftover shadows an enclosing savepoint of the same name and sends the
/// enclosing rollback to the wrong scope. It also leaves a subtransaction open
/// per rolled-back savepoint, which a retry loop accumulates.
///
/// The order matters and is not interchangeable: after a failed statement the
/// subtransaction is in an aborted state, where `RELEASE` is refused and
/// `ROLLBACK TO` is the statement that recovers it.
pub(crate) fn rollback_savepoint(name: &str) -> String {
    let name = quote_identifier(name);
    format!("ROLLBACK TO {name}; RELEASE {name}")
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }

        // Mark the client dirty *before* firing the rollback. The pool
        // inspects this flag at checkout time and drains any pending
        // ROLLBACK via a barrier (`simple_query("")`) so the next caller
        // never inherits a broken-tx state. `__private_api_rollback` also
        // sets the flag - we set it here too so the invariant holds even
        // if the encode step fails silently.
        self.client.inner().set_dirty();

        let name = self.savepoint.as_ref().map(|sp| sp.name.as_str());
        self.client.__private_api_rollback(name);
    }
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(client: &'a mut Client) -> Transaction<'a> {
        Transaction {
            client,
            savepoint: None,
            done: false,
        }
    }

    /// Consumes the transaction, committing all changes made within it.
    ///
    /// # Errors
    ///
    /// Returns an error for which [`Error::is_transaction_rolled_back`] is true
    /// when the server answered the `COMMIT` with the `ROLLBACK` command tag.
    /// That is not a failed statement - PostgreSQL runs a `COMMIT` sent inside
    /// an aborted transaction block, throws every change away, and reports
    /// success. Without the tag, this method returned `Ok(())` over discarded
    /// writes.
    pub async fn commit(mut self) -> Result<(), Error> {
        // A savepoint release has to be spelled differently depending on whether
        // the subtransaction is aborted, and the answer must be known BEFORE the
        // request is sent so the recovery can ride with it - see the `Failed`
        // arm below, which is what survives a cancellation after the send. When
        // the status is unsettled this barrier is what settles it: the
        // connection task decrements the in-flight count before it forwards the
        // batch carrying a `ReadyForQuery`, and requests are FIFO, so ONE empty
        // simple query settles every request enqueued before it. Measured
        // 2026-08-23, 40 of 40 rounds with one to four requests abandoned
        // mid-flight: `transaction_status()` was `Some` immediately afterwards
        // every time.
        //
        // THIS AWAIT MOVES WHERE CANCELLATION LANDS, and only on this branch.
        // With the status already settled the first poll of `commit()` enqueues
        // the `RELEASE` and sets `done`, so dropping the future keeps the
        // savepoint's writes; with it unsettled the first poll parks here with
        // `done` still false, so dropping the future rolls them back. Measured
        // the same day, one variable apart: 1 row kept versus 0. Rolling back is
        // the outcome `Transaction`'s drop contract documents, so the divergence
        // is recorded rather than removed - unconditionally awaiting the barrier
        // would make it uniform at the cost of a round trip on every nested
        // commit.
        if self.savepoint.is_some() && self.client.transaction_status().is_none() {
            self.client.simple_query("").await?;
        }
        let responses = if let Some(sp) = self.savepoint.as_ref() {
            let name = quote_identifier(&sp.name);
            let query = format!("RELEASE {name}");
            if self.client.transaction_status() == Some(crate::TransactionStatus::Failed) {
                crate::simple_query::start_batch_execute_with_error_cleanup(
                    self.client.inner(),
                    &query,
                    &rollback_savepoint(&sp.name),
                )?
            } else {
                crate::simple_query::start_batch_execute(self.client.inner(), &query)?
            }
        } else {
            crate::simple_query::start_batch_execute(self.client.inner(), "COMMIT")?
        };
        // `done` disarms the rollback-on-drop, so it must not be set until the
        // COMMIT has actually been enqueued. Setting it first - as this did
        // until 2026-08-21 - means anything that unwinds while the query is
        // still being built leaves the transaction open on the server with
        // nothing left to undo it.
        self.done = true;
        let tag = crate::simple_query::finish_batch_execute_reporting_tag(responses).await?;
        // batch_execute awaited the command to completion - the
        // connection is in a known-clean state. Clear any dirty flag
        // that a previous savepoint rollback or retry may have set.
        //
        // This happens before the tag is judged, not after: a `COMMIT` the
        // server turned into a rollback still ENDED the transaction, so the
        // session is exactly as clean as one that committed. The flag tracks
        // the connection, not the caller's outcome.
        self.client.inner().clear_dirty();
        // The test is "the server says it rolled back", NOT "the server did
        // not say COMMIT". The savepoint arm of this method sends `RELEASE`,
        // which answers with the tag `RELEASE` (measured), so the inverted
        // spelling would reject every healthy nested commit. Nothing narrows
        // this to the top-level arm, because nothing needs to: the cleanup
        // armed on a failed subtransaction sends its `ROLLBACK TO` as a
        // separate request whose responses never reach this stream.
        if tag.as_deref() == Some(ROLLBACK_TAG) {
            return Err(Error::transaction_rolled_back());
        }
        Ok(())
    }

    /// Rolls the transaction back, discarding all changes made within it.
    ///
    /// This is equivalent to `Transaction`'s `Drop` implementation, but
    /// provides any error encountered to the caller.
    pub async fn rollback(mut self) -> Result<(), Error> {
        let query = if let Some(sp) = self.savepoint.as_ref() {
            rollback_savepoint(&sp.name)
        } else {
            "ROLLBACK".to_string()
        };
        let responses = crate::simple_query::start_batch_execute(self.client.inner(), &query)?;
        self.done = true;
        let r = crate::simple_query::finish_batch_execute(responses).await;
        if r.is_ok() {
            // Explicit rollback awaited to completion - the connection is
            // clean regardless of what came before.
            self.client.inner().clear_dirty();
        }
        r
    }

    /// Like `Client::prepare`.
    pub async fn prepare(&self, query: &str) -> Result<Statement, Error> {
        self.client.prepare(query).await
    }

    /// Like `Client::prepare_typed`.
    pub async fn prepare_typed(
        &self,
        query: &str,
        parameter_types: &[Type],
    ) -> Result<Statement, Error> {
        self.client.prepare_typed(query, parameter_types).await
    }

    /// Like `Client::query`.
    pub async fn query<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.client.query(statement, params).await
    }

    /// Like `Client::query_one`.
    pub async fn query_one<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.client.query_one(statement, params).await
    }

    /// Like `Client::query_opt`.
    pub async fn query_opt<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.client.query_opt(statement, params).await
    }

    /// Like `Client::query_raw`.
    pub async fn query_raw<T, P, I>(&self, statement: &T, params: I) -> Result<RowStream, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        self.client.query_raw(statement, params).await
    }

    /// Like `Client::query_typed`.
    pub async fn query_typed(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>, Error> {
        self.client.query_typed(statement, params).await
    }

    /// Like `Client::query_text_params` - text-format params with
    /// implicit cast, run inside this transaction. Mirrors the
    /// `query_typed` passthrough; the JSON-driven query builders in
    /// `zeroship-plugin-db` use this to run the autocommit CRUD path
    /// inside an explicit transaction so `SET LOCAL` role/timeouts
    /// auto-revert on COMMIT/ROLLBACK (including the rollback-on-drop).
    pub async fn query_text_params(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, Error> {
        self.client.query_text_params(sql, params).await
    }

    /// Like `Client::query_typed_one`.
    pub async fn query_typed_one(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Row, Error> {
        self.client.query_typed_one(statement, params).await
    }

    /// Like `Client::query_typed_opt`.
    pub async fn query_typed_opt(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Option<Row>, Error> {
        self.client.query_typed_opt(statement, params).await
    }

    /// Like `Client::query_typed_raw`.
    pub async fn query_typed_raw<P, I>(&self, query: &str, params: I) -> Result<RowStream, Error>
    where
        P: BorrowToSql,
        I: IntoIterator<Item = (P, Type)>,
    {
        self.client.query_typed_raw(query, params).await
    }

    /// Like `Client::execute`.
    pub async fn execute<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.client.execute(statement, params).await
    }

    /// Like `Client::execute_typed`.
    pub async fn execute_typed(
        &self,
        statement: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<u64, Error> {
        self.client.execute_typed(statement, params).await
    }

    /// Like `Client::execute_iter`.
    pub async fn execute_raw<P, I, T>(&self, statement: &T, params: I) -> Result<u64, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        self.client.execute_raw(statement, params).await
    }

    /// Binds a statement to a set of parameters, creating a `Portal` which
    /// can be incrementally queried.
    ///
    /// Portals only last for the duration of the transaction in which they
    /// are created, and can only be used on the connection that created
    /// them.
    ///
    /// # Errors
    ///
    /// Returns an error, rather than panicking, when the number of parameters
    /// provided does not match the number the statement expects. That is the
    /// same outcome `query`, `execute` and their `_raw` forms produce for the
    /// same mistake; one arity error reported two ways would be decided by
    /// which method the caller happened to reach for.
    pub async fn bind<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Portal, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.bind_raw(statement, slice_iter(params)).await
    }

    /// A maximally flexible version of [`bind`].
    ///
    /// [`bind`]: #method.bind
    pub async fn bind_raw<P, T, I>(&self, statement: &T, params: I) -> Result<Portal, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let params = params.into_iter();
        // Binding through Transaction can never replay safely after 0A000:
        // PostgreSQL has already poisoned this transaction, so discard only
        // the implicit cache provenance and keep the caller's Statement.
        let execution = statement
            .__convert()
            .into_statement(self.client.inner())
            .await?
            // A portal bind is one validated statement use: repeated binds
            // pay the Parse/Describe cost that promotion is meant to remove.
            .finalize_probationary(self.client.inner(), params.len())
            .await?;
        bind::bind(
            self.client.inner(),
            execution.statement,
            params,
            execution.unnamed_sql,
        )
        .await
    }

    /// Continues execution of a portal, returning a stream of the resulting
    /// rows.
    ///
    /// Unlike `query`, portals can be incrementally evaluated by limiting
    /// the number of rows returned in each call to `query_portal`. If the
    /// requested number is negative or 0, all rows will be returned.
    pub async fn query_portal(&self, portal: &Portal, max_rows: i32) -> Result<Vec<Row>, Error> {
        self.query_portal_raw(portal, max_rows)
            .await?
            .try_collect()
            .await
    }

    /// The maximally flexible version of [`query_portal`].
    ///
    /// [`query_portal`]: #method.query_portal
    pub async fn query_portal_raw(
        &self,
        portal: &Portal,
        max_rows: i32,
    ) -> Result<RowStream, Error> {
        query::query_portal(self.client.inner(), portal, max_rows).await
    }

    /// Like `Client::copy_in`.
    pub async fn copy_in<T, U>(&self, statement: &T) -> Result<CopyInSink<U>, Error>
    where
        T: ?Sized + ToStatement,
        U: Buf + 'static + Send,
    {
        self.client.copy_in(statement).await
    }

    /// Like `Client::copy_out`.
    pub async fn copy_out<T>(&self, statement: &T) -> Result<CopyOutStream, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.client.copy_out(statement).await
    }

    /// Like `Client::simple_query`.
    pub async fn simple_query(&self, query: &str) -> Result<Vec<SimpleQueryMessage>, Error> {
        self.client.simple_query(query).await
    }

    /// Like `Client::batch_execute`.
    pub async fn batch_execute(&self, query: &str) -> Result<(), Error> {
        self.client.batch_execute(query).await
    }

    /// Like `Client::cancel_token`.
    pub fn cancel_token(&self) -> CancelToken {
        self.client.cancel_token()
    }

    /// Like `Client::cancel_query`.
    #[deprecated(since = "0.6.0", note = "use Transaction::cancel_token() instead")]
    pub async fn cancel_query<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        #[allow(deprecated)]
        self.client.cancel_query(tls).await
    }

    /// Like `Client::cancel_query_raw`.
    #[deprecated(since = "0.6.0", note = "use Transaction::cancel_token() instead")]
    pub async fn cancel_query_raw<S, T>(&self, stream: S, tls: T) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        #[allow(deprecated)]
        self.client.cancel_query_raw(stream, tls).await
    }

    /// Like `Client::transaction`, but creates a nested transaction via a
    /// savepoint.
    pub async fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        self._savepoint(None).await
    }

    /// Like `Client::transaction`, but creates a nested transaction via a
    /// savepoint with the specified name.
    pub async fn savepoint<I>(&mut self, name: I) -> Result<Transaction<'_>, Error>
    where
        I: Into<String>,
    {
        self._savepoint(Some(name.into())).await
    }

    async fn _savepoint(&mut self, name: Option<String>) -> Result<Transaction<'_>, Error> {
        let depth = self.savepoint.as_ref().map_or(0, |sp| sp.depth) + 1;
        let name = name.unwrap_or_else(|| format!("sp_{depth}"));
        let quoted_name = quote_identifier(&name);
        let query = format!("SAVEPOINT {quoted_name}");
        // Between the SAVEPOINT reaching the connection and this future being
        // polled to completion, no `Transaction` exists yet, so nothing owns
        // the name. Dropping the future there left it defined on the server,
        // where it shadows an enclosing savepoint of the same name and keeps a
        // subtransaction open. The guard is armed only across that window.
        let responses = crate::simple_query::start_batch_execute(self.client.inner(), &query)?;
        {
            let mut cleanup = SavepointCreationGuard {
                client: self.client,
                name: &name,
                done: false,
            };
            let result = crate::simple_query::finish_batch_execute(responses).await;
            cleanup.done = true;
            result?;
        }

        Ok(Transaction {
            client: self.client,
            savepoint: Some(Savepoint { name, depth }),
            done: false,
        })
    }

    /// Returns a reference to the underlying `Client`.
    pub fn client(&self) -> &Client {
        self.client
    }
}
