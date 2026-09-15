#![expect(
    clippy::future_not_send,
    reason = "ORM queries use thread-local compio sessions"
)]

use super::{
    read, Column, DbError, Direction, Entity, EntityCollection, EntityProjection, Field, Filter,
    FindOptions, FromRow, Future, NullOrder, OrderKey, PhantomData, ReadBuilder, ReadOrigin,
    ReadQuery, ReadSource, RelatedQuery, Relation, RowLimit, SchemaExpectation,
};

/// An ordering key tied to its generated entity.
#[derive(Debug)]
pub struct FieldOrder<E> {
    field: &'static str,
    direction: Direction,
    nulls: NullOrder,
    entity: PhantomData<fn() -> E>,
}

impl<E> FieldOrder<E> {
    #[must_use]
    pub const fn nulls_first(mut self) -> Self {
        self.nulls = NullOrder::First;
        self
    }

    #[must_use]
    pub const fn nulls_last(mut self) -> Self {
        self.nulls = NullOrder::Last;
        self
    }
}

impl<C: Column> Field<C> {
    #[must_use]
    pub fn asc(self) -> FieldOrder<C::Entity> {
        FieldOrder {
            field: C::NAME,
            direction: Direction::Ascending,
            nulls: NullOrder::Last,
            entity: PhantomData,
        }
    }

    #[must_use]
    pub fn desc(self) -> FieldOrder<C::Entity> {
        FieldOrder {
            field: C::NAME,
            direction: Direction::Descending,
            nulls: NullOrder::First,
            entity: PhantomData,
        }
    }
}

/// A typed collection read compiled through the shared relational operation.
#[derive(Debug)]
pub struct EntityQuery<E: Entity> {
    entity: EntityCollection<E>,
    filter: Filter<E>,
    options: FindOptions,
    order: Vec<FieldOrder<E>>,
    lock: bool,
}

impl<E: Entity> EntityCollection<E> {
    #[must_use]
    pub fn query(&self) -> EntityQuery<E> {
        EntityQuery {
            entity: Self {
                collection: self.collection.clone(),
                schema: self.schema.clone(),
                entity: PhantomData,
            },
            filter: Filter::all(),
            options: FindOptions::default(),
            order: Vec::new(),
            lock: false,
        }
    }

    pub fn count(&self, filter: Filter<E>) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        self.query().filter(filter).count()
    }

    /// Test whether a visible row matches without loading model fields.
    ///
    /// # Errors
    /// Refuses invalid filters, changed metadata, expired transactions, or database failures.
    pub fn exists(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<bool, DbError>> + use<E> {
        self.query().filter(filter).exists()
    }
}

impl<E: Entity> EntityQuery<E> {
    #[must_use]
    pub fn with_related<R: Relation<Source = E>>(self, _: R) -> RelatedQuery<E, R> {
        RelatedQuery::new(self)
    }

    #[must_use]
    pub fn filter(mut self, filter: Filter<E>) -> Self {
        self.filter = self.filter.and(filter);
        self
    }

    #[must_use]
    pub fn order_by(mut self, key: FieldOrder<E>) -> Self {
        self.order.push(key);
        self
    }

    /// # Errors
    /// Refuses limits outside the supported row budget.
    pub fn limit(mut self, limit: i64) -> Result<Self, DbError> {
        RowLimit::new(limit).map_err(|error| read::invalid(error.to_string()))?;
        self.options.limit = Some(limit);
        Ok(self)
    }

    /// # Errors
    /// Refuses offsets outside the supported range.
    pub fn offset(mut self, offset: i64) -> Result<Self, DbError> {
        crate::sql::RowOffset::new(offset).map_err(|error| read::invalid(error.to_string()))?;
        self.options.offset = Some(offset);
        Ok(self)
    }

    #[must_use]
    pub const fn include_deleted(mut self) -> Self {
        self.options.include_deleted = true;
        self
    }

    /// Lock every returned row, exclusively, until the transaction settles.
    /// A competing lock waits within the transaction's lock timeout.
    ///
    /// # Errors
    /// `transaction_required` unless the collection came from a transaction
    /// handle. Execution refuses `count`, `exists` and relation loading with
    /// `invalid_read`, and backends without row locks with
    /// `unsupported_backend_feature`.
    pub fn for_update(mut self) -> Result<Self, DbError> {
        super::require_lock_receiver(&self.entity.collection.database)?;
        self.lock = true;
        Ok(self)
    }

    pub(in crate::orm) const fn with_options(mut self, options: FindOptions) -> Self {
        self.options = options;
        self
    }

    pub(super) fn into_builder(self) -> Result<ReadBuilder, DbError> {
        self.entity.validate()?;
        let mut query = ReadQuery::new(ReadSource::new(E::COLLECTION, "source"));
        query.source.include_deleted = self.options.include_deleted;
        query.model_filter = Some(self.filter.into_predicate());
        if self.lock {
            query.lock = read::ReadLock::Update { of: Vec::new() };
        }
        query.limit = self
            .options
            .limit
            .map(RowLimit::new)
            .transpose()
            .map_err(|error| read::invalid(error.to_string()))?
            .unwrap_or_default();
        query.offset = self
            .options
            .offset
            .map(crate::sql::RowOffset::new)
            .transpose()
            .map_err(|error| read::invalid(error.to_string()))?
            .unwrap_or_default();
        for key in self.order {
            query.order_by.push(OrderKey {
                path: query.source.column(key.field)?,
                direction: key.direction,
                nulls: key.nulls,
            });
        }
        Ok(ReadBuilder {
            schemas: vec![SchemaExpectation {
                collection: E::COLLECTION.into(),
                schema: self.entity.schema,
                scope: self.entity.collection.database.scope.clone(),
            }],
            database: self.entity.collection.database,
            query,
            selection: (),
            validate_selection: |(), database, _| database.check_scope(),
            error: None,
        })
    }

    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or failed row decoding.
    pub fn all<R: FromRow<E>>(self) -> impl Future<Output = Result<Vec<R>, DbError>> + use<E, R> {
        let work = self
            .into_builder()
            .and_then(|builder| {
                let origin = ReadOrigin {
                    database: builder.database.identity.clone(),
                    scope: builder.database.scope.clone(),
                    source: builder.query.source.clone(),
                    schema: builder.database.context.with(|| {
                        crate::descriptor::collection_schema(
                            &builder.database.binding,
                            E::COLLECTION,
                        )
                    })?,
                };
                builder.select(EntityProjection::<E, R, false> {
                    source: "source".into(),
                    origin,
                    entity: PhantomData,
                })
            })
            .map(ReadBuilder::all);
        async move { work?.await }
    }

    /// # Errors
    /// Returns the same query and decoding failures as [`Self::all`].
    pub fn first<R: FromRow<E>>(
        mut self,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        self.options.limit = Some(1);
        let work = self.all();
        async move { Ok(work.await?.into_iter().next()) }
    }

    /// Count matching visible rows independently of ordering and page bounds.
    ///
    /// # Errors
    /// Refuses invalid queries, changed metadata, expired transactions, or database failures.
    pub fn count(self) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        let work = self.into_builder().map(ReadBuilder::count);
        async move { work?.await }
    }

    /// Test matching visible rows independently of ordering and page bounds.
    ///
    /// # Errors
    /// Refuses invalid filters, changed metadata, expired transactions, or database failures.
    pub fn exists(self) -> impl Future<Output = Result<bool, DbError>> + use<E> {
        let work = self.into_builder().map(ReadBuilder::exists);
        async move { work?.await }
    }
}
