// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The client-forwarding surface retains its tokio-postgres lineage, but the
// transaction lifecycle has diverged. This version scopes portals across
// savepoints, quotes and cancellation-guards savepoint commands, disarms
// rollback-on-drop only after enqueue, handles failed nested commits and
// COMMIT-as-ROLLBACK, and tracks pooled-session dirtiness.

use crate::copy_out::CopyOutStream;
use crate::portal::PortalScope;
use crate::query::RowStream;
use crate::types::{BorrowToSql, ToSql, Type};
use crate::{
    CancelToken, Client, CopyInSink, Error, Portal, Row, SimpleQueryMessage, Statement,
    ToStatement, bind, query, slice_iter,
};
use bytes::Buf;
use futures_util::TryStreamExt;

/// A representation of a PostgreSQL database transaction.
///
/// Transactions will implicitly roll back when dropped. Use the `commit`
/// method to commit the changes made in the transaction. Transactions can
/// be nested, with inner transactions implemented via savepoints.
pub struct Transaction<'a> {
    client: &'a mut Client,
    savepoint: Option<Savepoint>,
    portal_scope: PortalScope,
    parent_portal_scope: Option<PortalScope>,
    done: bool,
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Transaction")
            .field(
                "savepoint_name",
                &self.savepoint.as_ref().map(|savepoint| &savepoint.name),
            )
            .field(
                "savepoint_depth",
                &self.savepoint.as_ref().map(|savepoint| savepoint.depth),
            )
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
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

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }

        // Mark the client dirty *before* firing the rollback. At checkout the
        // pool runs `simple_query("")` as a FIFO barrier: success proves only
        // that the fire-and-forget command reached `ReadyForQuery`, not that
        // the session is clean. `return_client` isolates the next borrower by
        // queuing a ROLLBACK for every session that is not provably idle.
        // `__private_api_rollback` also sets the flag synchronously before it
        // encodes or sends anything.
        // That makes this caller-side ordering redundant by construction:
        // moving this mark below the call cannot become observable while the
        // callee owns the same pre-send mark. Keep it here because this acting
        // destructor owns the contract; adding a seam merely to distinguish
        // the duplicate marks would test the seam, not the invariant.
        self.client.inner().set_dirty();

        let name = self.savepoint.as_ref().map(|sp| sp.name.as_str());
        self.client.__private_api_rollback(name);
        self.portal_scope.invalidate();
    }
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(client: &'a mut Client) -> Transaction<'a> {
        Transaction {
            client,
            savepoint: None,
            portal_scope: PortalScope::new(),
            parent_portal_scope: None,
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
        let nested_failed = self.savepoint.is_some()
            && self.client.transaction_status() == Some(crate::TransactionStatus::Failed);
        let responses = if let Some(sp) = self.savepoint.as_ref() {
            let name = quote_identifier(&sp.name);
            let query = format!("RELEASE {name}");
            if nested_failed {
                crate::simple_query::start_batch_execute_with_error_cleanup(
                    self.client.inner(),
                    &query,
                    &crate::escape::rollback_savepoint(&sp.name),
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
        if let Some(parent) = self.parent_portal_scope.take() {
            if nested_failed {
                self.portal_scope.invalidate();
            } else {
                self.portal_scope.reparent(parent);
            }
        } else {
            self.portal_scope.invalidate();
        }
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
            crate::escape::rollback_savepoint(&sp.name)
        } else {
            "ROLLBACK".to_string()
        };
        let responses = crate::simple_query::start_batch_execute(self.client.inner(), &query)?;
        self.done = true;
        self.portal_scope.invalidate();
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
    /// `zeroship-data-v8` use this to run the autocommit CRUD path
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
            self.portal_scope.clone(),
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
            portal_scope: PortalScope::new(),
            parent_portal_scope: Some(self.portal_scope.clone()),
            done: false,
        })
    }

    /// Returns a reference to the underlying `Client`.
    pub fn client(&self) -> &Client {
        self.client
    }
}

#[cfg(test)]
mod tests {
    use super::{Savepoint, Transaction};
    use crate::Statement;
    use crate::client::{Client, ResponseMessages};
    use crate::codec::{BackendMessages, FrontendMessage};
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::{Request, RequestDisposition, RequestMessages, TransactionEffect};
    use crate::portal::{Portal, PortalScope};
    use bytes::BytesMut;
    use futures_channel::mpsc;
    use futures_util::StreamExt;

    fn test_client() -> (Client, mpsc::UnboundedReceiver<Request>) {
        let (sender, receiver) = mpsc::unbounded();
        let client = Client::new(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        (client, receiver)
    }

    fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(body.len() + 5);
        frame.push(tag);
        frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn portal_is_live(portal: &Portal, client: &Client) -> bool {
        portal.with_live_on(client.inner(), || Ok(())).is_ok()
    }

    /// `PostgreSQL`'s rollback destroys the server portal even if the local
    /// scope stays active, so every live-server test still gets an error. Keep
    /// the request queued but remove the server: the stale Rust handle itself
    /// must be rejected before it can enqueue any use of that missing portal.
    #[test]
    fn drop_invalidates_portals_before_server_cleanup_can_mask_them() {
        let (mut client, mut requests) = test_client();
        let transaction = Transaction::new(&mut client);
        let portal = Portal::new(
            transaction.client.inner(),
            "p_drop_scope".to_string(),
            Statement::unnamed(vec![], vec![]),
            transaction.portal_scope.clone(),
        );
        assert!(portal_is_live(&portal, transaction.client()));

        drop(transaction);

        let request = requests
            .try_recv()
            .expect("transaction drop did not enqueue its rollback");
        assert_eq!(request.disposition, RequestDisposition::Housekeeping);
        assert_eq!(request.transaction_effect, TransactionEffect::MayChange);
        let RequestMessages::Single(FrontendMessage::Raw(bytes)) = request.messages else {
            panic!("transaction drop did not encode one rollback batch");
        };
        assert_eq!(&bytes[5..], b"ROLLBACK\0");
        assert!(
            !portal_is_live(&portal, &client),
            "transaction drop left its portal scope locally active"
        );
    }

    /// A child scope starts active, so checking only that its portal survives
    /// the nested commit cannot detect a missing reparent. Check the other
    /// direction too: invalidating the saved parent must immediately make the
    /// child portal invalid without asking `PostgreSQL` to find its old name.
    #[compio::test]
    async fn nested_commit_reparents_portals_to_the_parent_scope() {
        let (mut client, mut requests) = test_client();
        let parent_scope = PortalScope::new();
        let child_scope = PortalScope::new();
        let transaction = Transaction {
            client: &mut client,
            savepoint: Some(Savepoint {
                name: "child_scope".to_string(),
                depth: 1,
            }),
            portal_scope: child_scope.clone(),
            parent_portal_scope: Some(parent_scope.clone()),
            done: false,
        };
        let portal = Portal::new(
            transaction.client.inner(),
            "p_child_scope".to_string(),
            Statement::unnamed(vec![], vec![]),
            child_scope,
        );

        let commit = transaction.commit();
        let respond = async {
            let mut request = requests
                .next()
                .await
                .expect("nested commit did not enqueue RELEASE");
            assert_eq!(request.disposition, RequestDisposition::Awaited);
            assert_eq!(request.transaction_effect, TransactionEffect::MayChange);
            let RequestMessages::Single(FrontendMessage::Raw(bytes)) = &request.messages else {
                panic!("nested commit did not encode one RELEASE batch");
            };
            assert_eq!(&bytes[5..], b"RELEASE \"child_scope\"\0");

            let mut bytes = BytesMut::new();
            bytes.extend_from_slice(&backend_frame(b'C', b"RELEASE\0"));
            bytes.extend_from_slice(&backend_frame(b'Z', b"T"));
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    bytes,
                )))
                .expect("deliver the scripted RELEASE response");
        };

        let (result, ()) = futures_util::join!(commit, respond);
        result.expect("scripted nested commit failed");
        assert!(
            portal_is_live(&portal, &client),
            "nested commit invalidated its child portal instead of preserving it"
        );

        parent_scope.invalidate();
        assert!(
            !portal_is_live(&portal, &client),
            "nested commit left the child scope active instead of parenting it"
        );
    }
}
