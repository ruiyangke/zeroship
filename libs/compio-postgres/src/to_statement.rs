// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::Statement;
use crate::to_statement::private::{Sealed, ToStatementType};

mod private {
    use std::sync::Arc;

    use crate::{Error, Statement, client::InnerClient, prepare};

    pub trait Sealed {}

    pub enum ToStatementType<'a> {
        Statement(&'a Statement),
        Query(&'a str),
        Uncached(&'a str),
    }

    pub(crate) struct StatementExecution<'a> {
        pub(crate) statement: Statement,
        pub(crate) cache_sql: Option<&'a str>,
        pub(crate) unnamed_sql: Option<&'a str>,
    }

    impl<'a> ToStatementType<'a> {
        pub(crate) async fn into_statement(
            self,
            client: &Arc<InnerClient>,
        ) -> Result<StatementExecution<'a>, Error> {
            match self {
                // Explicit Statements stay caller-owned. Repreparing one would
                // silently replace its identity and result metadata underneath
                // the caller instead of making them opt into a fresh prepare.
                ToStatementType::Statement(statement) => {
                    match statement.owner() {
                        Some(owner) if Arc::ptr_eq(&owner, client) => {}
                        Some(_) => return Err(Error::statement_owner_mismatch()),
                        // Internal unnamed descriptors have no connection owner.
                        // They are metadata for an unnamed Parse performed by the
                        // same operation, not reusable prepared-statement handles.
                        None if statement.name().is_empty() => {}
                        None => return Err(Error::statement_owner_dropped()),
                    }

                    Ok(StatementExecution {
                        statement: statement.clone(),
                        cache_sql: None,
                        unnamed_sql: None,
                    })
                }
                ToStatementType::Query(sql) => {
                    let cached = prepare::prepare_cached_with_origin(client, sql).await?;
                    Ok(StatementExecution {
                        statement: cached.statement,
                        cache_sql: cached.cache_hit.then_some(sql),
                        unnamed_sql: cached.unnamed.then_some(sql),
                    })
                }
                ToStatementType::Uncached(sql) => Ok(StatementExecution {
                    statement: prepare::prepare(client, sql, &[]).await?,
                    cache_sql: None,
                    unnamed_sql: None,
                }),
            }
        }
    }

    impl<'a> StatementExecution<'a> {
        /// Turn discovery-only unnamed metadata into one counted, validated
        /// cache use. Named/default and caller-owned statements bypass this.
        pub(crate) async fn finalize_probationary(
            mut self,
            client: &Arc<InnerClient>,
            parameter_count: usize,
        ) -> Result<Self, Error> {
            let Some(sql) = self.unnamed_sql else {
                return Ok(self);
            };
            let expected = self.statement.params().len();
            if parameter_count != expected {
                return Err(Error::parameters(parameter_count, expected));
            }

            let finalized = prepare::finalize_probationary(client, sql, self.statement).await?;
            self.statement = finalized.statement;
            self.cache_sql = finalized.cache_hit.then_some(sql);
            self.unnamed_sql = finalized.unnamed.then_some(sql);
            Ok(self)
        }
    }
}

/// A raw SQL string which bypasses the connection's prepared-statement cache
/// for one operation.
///
/// The statement is prepared normally, used once, and closed when the
/// operation releases its last clone. It neither looks up nor populates a
/// configured cache.
#[derive(Clone, Copy, Debug)]
#[must_use = "Uncached only bypasses the cache when passed to an operation"]
pub struct Uncached<'a> {
    query: &'a str,
}

impl<'a> Uncached<'a> {
    /// Marks `query` as a one-shot statement for the next operation.
    pub const fn new(query: &'a str) -> Self {
        Self { query }
    }
}

/// A trait abstracting over prepared and unprepared statements.
///
/// Many methods are generic over this bound, so that they support both a raw query string as well as a statement which
/// was prepared previously.
///
/// This trait is "sealed" and cannot be implemented by anything outside this crate.
pub trait ToStatement: Sealed {
    #[doc(hidden)]
    fn __convert(&self) -> ToStatementType<'_>;
}

impl ToStatement for Statement {
    fn __convert(&self) -> ToStatementType<'_> {
        ToStatementType::Statement(self)
    }
}

impl Sealed for Statement {}

impl ToStatement for str {
    fn __convert(&self) -> ToStatementType<'_> {
        ToStatementType::Query(self)
    }
}

impl Sealed for str {}

impl ToStatement for String {
    fn __convert(&self) -> ToStatementType<'_> {
        ToStatementType::Query(self)
    }
}

impl Sealed for String {}

impl ToStatement for Uncached<'_> {
    fn __convert(&self) -> ToStatementType<'_> {
        ToStatementType::Uncached(self.query)
    }
}

impl Sealed for Uncached<'_> {}
