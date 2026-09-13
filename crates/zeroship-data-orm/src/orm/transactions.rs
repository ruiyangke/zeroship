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
    /// Commit a successful callback or roll back its error. Nested calls use
    /// the transaction protocol's savepoint frames. Escaped handles expire
    /// when their callback finishes, including when its future is cancelled.
    pub async fn transaction<T, F, Fut>(&self, body: F) -> Result<T, DbError>
    where
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, DbError>>,
    {
        self.transaction_with_options(TransactionOptions::default(), body)
            .await
    }

    /// Open a transaction with explicit settings. Isolation can only be selected
    /// on the outermost transaction; nested callbacks use savepoints.
    pub async fn transaction_with_options<T, F, Fut>(
        &self,
        options: TransactionOptions,
        body: F,
    ) -> Result<T, DbError>
    where
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, DbError>>,
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
                    crate::transaction::scope::TransactionScope::current(self.binding.app_id())?,
                );
                let result = body(transaction).await;
                active.set(false);
                frame.finish(result).await
            })
            .await
    }
}
