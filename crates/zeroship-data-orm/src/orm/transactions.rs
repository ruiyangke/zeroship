use super::*;

/// Settings for opening a transaction. Nested callbacks inherit their parent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransactionOptions {
    /// Omission uses the backend default or inherits the enclosing transaction.
    pub isolation_level: Option<IsolationLevel>,
}

impl TransactionOptions {
    #[must_use]
    pub fn isolation_level(mut self, level: IsolationLevel) -> Self {
        self.isolation_level = Some(level);
        self
    }
}

impl Database {
    /// Commit a callback that succeeds with nothing it started still running,
    /// and roll back any other. Nested calls use the transaction protocol's
    /// savepoint frames. Escaped handles expire when their callback finishes,
    /// including when its future is cancelled.
    ///
    /// A transaction runs one operation at a time: an operation first polled
    /// while another one of the same callback is outstanding, a nested
    /// transaction included, fails with `transaction_connection_busy`; see
    /// [`Collection::execute`] for how `join!` and `select!` order that.
    ///
    /// The transaction does not end while an operation its callback started
    /// is outstanding. Settlement waits for it, then rolls back: a callback
    /// that returned `Ok` with an operation outstanding fails with
    /// `transaction_work_unfinished`, and one that returned `Err` hands back
    /// its own error. The wait lasts until the operation's future completes or
    /// is dropped, so a future the callback polled and then left unpolled
    /// holds settlement until the transaction's execution deadline ends it.
    /// That fails with `transaction_deadline_expired` on SQLite; on PostgreSQL
    /// the parked statement is still on the wire, the session is withdrawn,
    /// and the failure is `commit_failed_indeterminate`.
    ///
    /// The callback chooses its own error type, so a domain refusal that must
    /// roll back is returned as itself: `Err(refusal)` rolls back and hands the
    /// refusal to the caller, while `Ok(Err(refusal))` commits the work done
    /// before it. Database failures reach the caller through
    /// `E: From<DbError>`. A callback that only ever fails with [`DbError`]
    /// states so at one of its `Ok` arms, because nothing else fixes `E`.
    pub async fn transaction<T, E, F, Fut>(&self, body: F) -> Result<T, E>
    where
        E: From<DbError>,
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.transaction_with_options(TransactionOptions::default(), body)
            .await
    }

    /// Open a transaction with explicit settings. Isolation can only be selected
    /// on the outermost transaction; nested callbacks use savepoints.
    ///
    /// A top-level transaction opened on a root handle while a callback on the
    /// same context holds that app's lane is refused with
    /// `nested_top_level_transaction`; see [`Database::independent`] for the
    /// handle that runs one concurrently instead.
    pub async fn transaction_with_options<T, E, F, Fut>(
        &self,
        options: TransactionOptions,
        body: F,
    ) -> Result<T, E>
    where
        E: From<DbError>,
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.context
            .scope(async {
                self.check_scope()?;
                let route = self.capture_route().bind(self.backend.clone())?;
                let frame = crate::transaction::AtomicWriteFrame::begin_with_isolation(
                    route,
                    options.isolation_level,
                )
                .await?;
                let active = Rc::new(Cell::new(true));
                let _guard = ScopeGuard(active.clone());
                let mut transaction = self.clone();
                transaction.scope = Some(active.clone());
                transaction.transaction_scope = Some(
                    crate::transaction::scope::TransactionScope::current(&self.binding.route())?,
                );
                let result =
                    crate::transaction::in_callback(&self.binding.route(), body(transaction)).await;
                active.set(false);
                frame.finish(result).await
            })
            .await
    }
}
