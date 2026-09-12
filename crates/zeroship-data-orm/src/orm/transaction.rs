//! Owned transactions for native services that settle outside a callback.

use super::{Database, DbError, Value};
use crate::{sql::compiler::CompiledQuery, transaction::AtomicWriteFrame};
use std::{cell::Cell, rc::Rc};

/// A scoped database and its transaction frame. Dropping an unsettled handle
/// abandons the frame through the ORM protocol and expires derived handles.
#[must_use = "settle the transaction with commit or rollback"]
#[derive(Debug)]
pub struct Transaction {
    database: Database,
    frame: Option<AtomicWriteFrame>,
}

impl Database {
    /// Open an owned transaction, or a savepoint when this database is scoped
    /// to an existing transaction. Use the returned database for model work.
    pub async fn begin_transaction(&self) -> Result<Transaction, DbError> {
        self.context
            .scope(async {
                self.check_scope()?;
                let frame =
                    AtomicWriteFrame::begin(self.capture_route().bind(self.backend.clone()))
                        .await?;
                let mut database = self.clone();
                database.scope = Some(Rc::new(Cell::new(true)));
                database.transaction_scope = Some(
                    crate::transaction::scope::TransactionScope::current(self.binding.app_id())?,
                );
                Ok(Transaction {
                    database,
                    frame: Some(frame),
                })
            })
            .await
    }
}

impl Transaction {
    /// Model and collection handles derived here expire when this frame settles.
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Execute a parameterized query in this frame. This low-level operation
    /// skips model protection and decoding; use it for SQL the model API cannot
    /// express, such as database-clock reads and row locks.
    pub async fn query_sql(&self, query: &CompiledQuery) -> Result<Vec<Value>, DbError> {
        self.database
            .context
            .scope(async {
                self.database.check_scope()?;
                crate::exec::run_sql(
                    self.frame.as_ref().expect("open transaction").route(),
                    query.sql(),
                    query.params(),
                )
                .await
            })
            .await
    }

    /// Execute parameterized SQL in this frame without model protection or CDC.
    /// Prefer collection mutations for application records.
    pub async fn execute_sql(&self, query: &CompiledQuery) -> Result<u64, DbError> {
        self.database
            .context
            .scope(async {
                self.database.check_scope()?;
                crate::exec::run_statement(
                    self.frame.as_ref().expect("open transaction").route(),
                    query.sql(),
                    query.params(),
                )
                .await
            })
            .await
    }

    pub async fn commit(self) -> Result<(), DbError> {
        self.finish(Ok(())).await
    }

    pub async fn rollback(mut self) -> Result<(), DbError> {
        self.expire();
        let frame = self.frame.take().expect("open transaction");
        self.database
            .context
            .scope(async {
                frame.route().check_scope()?;
                frame.rollback().await
            })
            .await
    }

    pub(super) async fn finish<T>(mut self, result: Result<T, DbError>) -> Result<T, DbError> {
        self.expire();
        let frame = self.frame.take().expect("open transaction");
        self.database
            .context
            .scope(async {
                frame.route().check_scope()?;
                frame.finish(result).await
            })
            .await
    }

    fn expire(&self) {
        self.database
            .scope
            .as_ref()
            .expect("transaction scope")
            .set(false);
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        self.expire();
        self.database.context.with(|| drop(self.frame.take()));
    }
}
