// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::{Client, Error, Transaction};

/// The isolation level of a database transaction.
#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
pub enum IsolationLevel {
    /// Equivalent to `ReadCommitted`.
    ReadUncommitted,

    /// An individual statement in the transaction will see rows committed before it began.
    ReadCommitted,

    /// All statements in the transaction will see the same view of rows committed before the first query in the
    /// transaction.
    RepeatableRead,

    /// The reads and writes in this transaction must be able to be committed as an atomic "unit" with respect to reads
    /// and writes of all other concurrent serializable transactions without interleaving.
    Serializable,
}

/// A builder for database transactions.
#[derive(Debug)]
pub struct TransactionBuilder<'a> {
    client: &'a mut Client,
    isolation_level: Option<IsolationLevel>,
    read_only: Option<bool>,
    deferrable: Option<bool>,
}

impl<'a> TransactionBuilder<'a> {
    pub(crate) fn new(client: &'a mut Client) -> TransactionBuilder<'a> {
        TransactionBuilder {
            client,
            isolation_level: None,
            read_only: None,
            deferrable: None,
        }
    }

    /// Sets the isolation level of the transaction.
    pub fn isolation_level(mut self, isolation_level: IsolationLevel) -> Self {
        self.isolation_level = Some(isolation_level);
        self
    }

    /// Sets the access mode of the transaction.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = Some(read_only);
        self
    }

    /// Sets the deferrability of the transaction.
    ///
    /// If the transaction is also serializable and read only, creation of the transaction may block, but when it
    /// completes the transaction is able to run with less overhead and a guarantee that it will not be aborted due to
    /// serialization failure.
    pub fn deferrable(mut self, deferrable: bool) -> Self {
        self.deferrable = Some(deferrable);
        self
    }

    /// Begins the transaction.
    ///
    /// The transaction will roll back by default - use the `commit` method to commit it.
    pub async fn start(self) -> Result<Transaction<'a>, Error> {
        let mut query = "START TRANSACTION".to_string();
        let mut first = true;

        if let Some(level) = self.isolation_level {
            first = false;

            query.push_str(" ISOLATION LEVEL ");
            let level = match level {
                IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
                IsolationLevel::ReadCommitted => "READ COMMITTED",
                IsolationLevel::RepeatableRead => "REPEATABLE READ",
                IsolationLevel::Serializable => "SERIALIZABLE",
            };
            query.push_str(level);
        }

        if let Some(read_only) = self.read_only {
            if !first {
                query.push(',');
            }
            first = false;

            let s = if read_only {
                " READ ONLY"
            } else {
                " READ WRITE"
            };
            query.push_str(s);
        }

        if let Some(deferrable) = self.deferrable {
            if !first {
                query.push(',');
            }

            let s = if deferrable {
                " DEFERRABLE"
            } else {
                " NOT DEFERRABLE"
            };
            query.push_str(s);
        }

        struct RollbackIfNotDone<'me> {
            client: &'me Client,
            done: bool,
        }

        impl Drop for RollbackIfNotDone<'_> {
            fn drop(&mut self) {
                if self.done {
                    return;
                }

                // Mark dirty before firing - the pool's checkout barrier
                // will drain this queued ROLLBACK. See `Transaction::drop`
                // for the full lifecycle.
                self.client.inner().set_dirty();
                self.client.__private_api_rollback(None);
            }
        }

        // The future this method returns can be dropped after `RequestMessages`
        // has synchronously reached the `Connection` but before `Responses` is
        // polled to completion. No `Transaction` exists yet in that case, so
        // nothing else would roll the START back.
        //
        // The guard is armed only once the request is ENQUEUED, which is why
        // the send is split out of the await. Arming it across the encode and
        // the logging - both of which can fail or unwind - would fire a
        // ROLLBACK for a START that never left this process, and that rollback
        // lands on whatever transaction the caller already had open.
        let responses = crate::simple_query::start_batch_execute(self.client.inner(), &query)?;
        {
            let mut cleaner = RollbackIfNotDone {
                client: self.client,
                done: false,
            };
            let result = crate::simple_query::finish_batch_execute(responses).await;
            cleaner.done = true;
            result?;
        }

        Ok(Transaction::new(self.client))
    }
}

#[cfg(test)]
mod tests {
    use crate::Client;
    use crate::config::{SslMode, SslNegotiation};
    use futures_channel::mpsc;
    use std::task::{Context, Poll, Waker};

    /// Dropping the `start()` future after the START has been enqueued must
    /// queue a ROLLBACK and mark the connection dirty.
    ///
    /// The START reaches the connection synchronously, but no `Transaction`
    /// exists until the future completes - so if the caller drops it while
    /// awaiting the response, nothing else in the type system would roll that
    /// START back and the session would be left inside an open transaction
    /// that the next pool borrower inherits.
    ///
    /// The dirty flag matters as much as the ROLLBACK: it is what makes the
    /// pool's checkout barrier drain the queued frame before reuse.
    #[compio::test]
    async fn dropping_start_after_enqueue_rolls_the_transaction_back() {
        let (request_sender, mut requests) = mpsc::unbounded();
        let mut client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        assert!(!client.is_dirty(), "a fresh client was already dirty");

        let mut starting = Box::pin(client.build_transaction().start());
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(starting.as_mut().poll(&mut context), Poll::Pending),
            "the START completed without any response being delivered"
        );
        requests
            .try_recv()
            .expect("the START never reached the connection");

        // Drop while the response is still outstanding: the guard is armed.
        drop(starting);

        assert!(
            requests.try_recv().is_ok(),
            "dropping the START queued no ROLLBACK, leaving the session in a transaction"
        );
        assert!(
            client.is_dirty(),
            "the queued ROLLBACK was not announced to the pool's checkout barrier"
        );
    }
}
