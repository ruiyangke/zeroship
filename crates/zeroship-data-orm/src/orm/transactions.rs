use super::*;

impl Database {
    /// Commit a successful callback or roll back its error. Nested calls use
    /// the transaction protocol's savepoint frames. Escaped handles expire
    /// when their callback finishes, including when its future is cancelled.
    pub async fn transaction<T, F, Fut>(&self, body: F) -> Result<T, DbError>
    where
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, DbError>>,
    {
        self.context
            .scope(async {
                self.check_scope()?;
                let route = self.capture_route().bind(self.backend.clone())?;
                let frame = crate::transaction::AtomicWriteFrame::begin(route).await?;
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
