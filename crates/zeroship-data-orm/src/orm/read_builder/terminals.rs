#![expect(
    clippy::future_not_send,
    reason = "ORM reads use thread-local compio sessions"
)]

use super::{
    count_rows, DbError, Future, Output, ReadBuilder, ReadProjection, ReadSelection, Value,
};
use crate::sql::statement::SelectSummary;

impl<P> ReadBuilder<P> {
    fn execute(self) -> impl Future<Output = Result<(Output, P), DbError>> {
        let sources = self.sources();
        let work = self
            .validate_schemas()
            .and_then(|()| (self.validate_selection)(&self.selection, &self.database, &sources))
            .map(|()| self.database.read(self.query));
        Box::pin(async move {
            if let Some(error) = self.error {
                return Err(error);
            }
            for expected in &self.schemas {
                expected.validate(&self.database)?;
            }
            (self.validate_selection)(&self.selection, &self.database, &sources)?;
            Ok((work?.await?, self.selection))
        })
    }

    fn summary(mut self, summary: SelectSummary) -> impl Future<Output = Result<i64, DbError>> {
        self.query.summary = Some(summary);
        self.query.order_by.clear();
        if self.query.projection.is_empty() {
            if self.query.group_by.is_empty() && self.query.having.mentions_aggregate() {
                count_rows().projections(&mut 0, &mut self.query.projection);
            } else {
                self.query.projection.push(ReadProjection::Presence {
                    output: "present".into(),
                });
            }
        }
        let work = self.execute();
        async move {
            let (Output::Count(count), _) = work.await? else {
                return Err(DbError::internal("summary returned rows"));
            };
            Ok(count)
        }
    }

    /// Count matching rows or groups independently of ordering and page bounds.
    ///
    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or database failures.
    pub fn count(self) -> impl Future<Output = Result<i64, DbError>> {
        self.summary(SelectSummary::Count)
    }

    /// Test for matching rows or groups independently of ordering and page bounds.
    ///
    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or database failures.
    pub fn exists(self) -> impl Future<Output = Result<bool, DbError>> {
        let work = self.summary(SelectSummary::Exists);
        async move { Ok(work.await? != 0) }
    }
}

impl<P: ReadSelection> ReadBuilder<P> {
    ///
    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or database failures.
    pub fn all(self) -> impl Future<Output = Result<Vec<P::Output>, DbError>> {
        let work = self.execute();
        async move {
            let (Output::Rows { rows, .. }, selection) = work.await? else {
                return Err(DbError::internal("read returned a count"));
            };
            rows.into_iter()
                .map(|row| {
                    let Value::Object(mut record) = row else {
                        return Err(DbError::internal("read returned a non-object"));
                    };
                    selection.decode(&mut 0, &mut record)
                })
                .collect()
        }
    }

    ///
    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or database failures.
    pub fn first(self) -> impl Future<Output = Result<Option<P::Output>, DbError>> {
        let work = self.limit(1).map(Self::all);
        async move { Ok(work?.await?.into_iter().next()) }
    }
}
