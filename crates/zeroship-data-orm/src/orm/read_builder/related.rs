#![expect(
    clippy::future_not_send,
    reason = "ORM reads use thread-local compio sessions"
)]

use super::*;
use crate::orm::relations::PreparedRelations;

/// A parent query with one declared forward reference loaded in a batch.
#[derive(Debug)]
pub struct RelatedQuery<E: Entity, R: Relation<Source = E>> {
    query: EntityQuery<E>,
    reference: PhantomData<fn() -> R>,
}

impl<E: Entity, R: Relation<Source = E>> RelatedQuery<E, R> {
    pub(super) fn new(query: EntityQuery<E>) -> Self {
        Self {
            query,
            reference: PhantomData,
        }
    }

    #[must_use]
    pub fn filter(mut self, filter: Filter<E>) -> Self {
        self.query = self.query.filter(filter);
        self
    }
    #[must_use]
    pub fn order_by(mut self, key: FieldOrder<E>) -> Self {
        self.query = self.query.order_by(key);
        self
    }
    ///
    /// # Errors
    /// Returns the corresponding parent query errors.
    pub fn limit(mut self, limit: i64) -> Result<Self, DbError> {
        self.query = self.query.limit(limit)?;
        Ok(self)
    }
    ///
    /// # Errors
    /// Returns the corresponding parent query errors.
    pub fn offset(mut self, offset: i64) -> Result<Self, DbError> {
        self.query = self.query.offset(offset)?;
        Ok(self)
    }
    #[must_use]
    pub fn include_deleted(mut self) -> Self {
        self.query = self.query.include_deleted();
        self
    }
    /// Count matching parents without loading the relation projection.
    ///
    /// # Errors
    /// Returns the corresponding parent query errors.
    pub fn count(self) -> impl Future<Output = Result<i64, DbError>> + use<E, R> {
        self.query.count()
    }
    /// Test for matching parents without loading the relation projection.
    ///
    /// # Errors
    /// Returns the corresponding parent query errors.
    pub fn exists(self) -> impl Future<Output = Result<bool, DbError>> + use<E, R> {
        self.query.exists()
    }

    pub fn all<P: FromRow<E>, T: FromRow<R::Target>>(
        self,
    ) -> impl Future<Output = Result<Vec<(P, Option<T>)>, DbError>> + use<E, R, P, T> {
        let work = self.query.into_builder().and_then(|mut builder| {
            if builder.query.lock != read::ReadLock::None {
                return Err(read::invalid("row locks cannot load relations"));
            }
            let database = builder.database;
            database.context.with(|| {
                database.check_scope()?;
                let source_schema =
                    crate::descriptor::collection_schema(&database.binding, E::COLLECTION)?;
                let target = database.entity::<R::Target>()?;
                let matches = source_schema
                    .get(R::FIELD)
                    .and_then(|column| column.reference.as_ref())
                    .is_some_and(|reference| {
                        reference.name.as_deref() == Some(R::NAME)
                            && reference.collection == R::Target::COLLECTION
                            && reference.column == R::TARGET_COLUMN
                    });
                if !matches {
                    return Err(schema_mismatch_for(E::COLLECTION));
                }
                let mut fields = P::COLUMNS
                    .iter()
                    .map(|field| (*field).to_owned())
                    .collect::<Vec<_>>();
                if !fields.iter().any(|field| field == R::FIELD) {
                    fields.push(R::FIELD.into());
                }
                builder.query.projection = vec![ReadProjection::Row {
                    output: "parent".into(),
                    source: builder.query.source.alias.clone(),
                    fields: Some(fields),
                    optional: false,
                }];
                let route = database.capture_route();
                let related = PreparedRelations::new(
                    &database.binding,
                    route.sql_registration(),
                    E::COLLECTION,
                    &[R::FIELD.into()],
                )?;
                let parent = read::PreparedRead::new(
                    &database.binding,
                    route.sql_registration(),
                    route.in_tx(),
                    builder.query,
                )?;
                Ok((
                    database.clone(),
                    source_schema,
                    target.schema,
                    route,
                    parent,
                    related,
                ))
            })
        });
        Box::pin(async move {
            let (database, source_schema, target_schema, captured, parent, related) = work?;
            database.check_scope()?;
            validate_bound_schema(&database, E::COLLECTION, &source_schema)?;
            validate_bound_schema(&database, R::Target::COLLECTION, &target_schema)?;
            database
                .context
                .clone()
                .scope(async move {
                    let route = captured.bind(database.backend.clone())?;
                    let Output::Rows { rows, .. } =
                        parent.execute(&database.binding, &route).await?
                    else {
                        return Err(DbError::internal("related parent query returned a count"));
                    };
                    let parents = rows
                        .into_iter()
                        .map(|row| {
                            let Value::Object(mut row) = row else {
                                return Err(DbError::internal(
                                    "related parent query returned a non-record",
                                ));
                            };
                            match row.swap_remove("parent") {
                                Some(value @ Value::Object(_)) => Ok(value),
                                _ => Err(DbError::internal("related parent projection is missing")),
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    database.check_scope()?;
                    let mut loaded = related.load(&database.binding, &route, &parents).await?;
                    if loaded.len() != 1 {
                        return Err(DbError::internal(
                            "related query returned an unexpected relation set",
                        ));
                    }
                    let loaded = loaded.pop().unwrap();
                    if loaded.field != R::FIELD || loaded.rows.len() != parents.len() {
                        return Err(DbError::internal("related query returned unaligned rows"));
                    }
                    parents
                        .into_iter()
                        .zip(loaded.rows)
                        .map(|(parent, target)| {
                            let parent = decode::<E, P>(parent)?;
                            let target = target.map(decode::<R::Target, T>).transpose()?;
                            Ok((parent, target))
                        })
                        .collect()
                })
                .await
        })
    }

    pub fn first<P: FromRow<E>, T: FromRow<R::Target>>(
        mut self,
    ) -> impl Future<Output = Result<Option<(P, Option<T>)>, DbError>> + use<E, R, P, T> {
        self.query = self
            .query
            .limit(1)
            .expect("a single row is within the query limit");
        let work = self.all();
        async move { Ok(work.await?.into_iter().next()) }
    }
}

fn decode<E: Entity, R: FromRow<E>>(value: Value) -> Result<R, DbError> {
    let Value::Object(fields) = value else {
        return Err(DbError::internal("related query returned a non-record"));
    };
    R::from_row(Row::new(fields))
}
