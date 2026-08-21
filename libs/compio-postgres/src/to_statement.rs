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

    impl ToStatementType<'_> {
        pub(crate) async fn into_statement(self, client: &Arc<InnerClient>) -> Result<Statement, Error> {
            match self {
                ToStatementType::Statement(s) => Ok(s.clone()),
                ToStatementType::Query(s) => prepare::prepare_cached(client, s).await,
                ToStatementType::Uncached(s) => prepare::prepare(client, s, &[]).await,
            }
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
