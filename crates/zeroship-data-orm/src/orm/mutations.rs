use super::*;

/// A nonempty upsert target made from columns of the same entity.
/// The database validates its uniqueness when the operation executes.
#[derive(Debug)]
pub struct ConflictTarget<E: Entity> {
    fields: Vec<&'static str>,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity> ConflictTarget<E> {
    pub fn new<C: Column<Entity = E>>(_field: Field<C>) -> Self {
        Self {
            fields: vec![C::NAME],
            entity: PhantomData,
        }
    }

    #[must_use]
    pub fn and<C: Column<Entity = E>>(mut self, _field: Field<C>) -> Self {
        self.fields.push(C::NAME);
        self
    }

    fn into_value(self) -> Value {
        Value::Array(
            self.fields
                .into_iter()
                .map(|field| Value::String(field.into()))
                .collect(),
        )
    }
}

impl<E: Entity> EntityCollection<E> {
    /// Insert every document atomically and decode the protected result rows.
    pub fn insert_many<D, R>(
        &self,
        documents: D,
    ) -> impl Future<Output = Result<Vec<R>, DbError>> + use<E, D, R>
    where
        D: IntoIterator,
        D::Item: Insertable<E>,
        R: FromRow<E>,
    {
        let future = self.validate().and_then(|()| {
            let mut records = Vec::new();
            for document in documents {
                if records.len() == crate::budgets::MAX_INSERT_MANY_BATCH {
                    return Err(DbError::validation(
                        "invalid_filter",
                        "insertMany exceeds the batch limit",
                    ));
                }
                records.push(Value::Object(document.into_record()?));
            }
            Ok(self.collection.execute(Operation::InsertMany {
                documents: Value::Array(records),
            }))
        });
        async move { decode_rows::<E, R>(future?.await?) }
    }

    /// Update matching rows and return their affected count.
    pub fn update_many<C: Changeset<E>>(
        &self,
        filter: Filter<E>,
        changes: C,
    ) -> impl Future<Output = Result<i64, DbError>> + use<E, C> {
        let future = self
            .validate()
            .and_then(|()| changes.into_changes())
            .map(|fields| {
                self.collection.update_model(
                    filter.into_predicate(),
                    Value::Object([("$set".into(), Value::Object(fields))].into()),
                    true,
                )
            });
        async move { decode_count(future?.await?) }
    }

    /// Apply the collection's deletion lifecycle to matching rows.
    pub fn delete_many(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        self.mutate_many(filter, Mutation::Delete)
    }

    /// Restore a matching soft-deleted row.
    pub fn restore<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        self.mutate_one(filter, Mutation::Restore)
    }

    /// Restore matching soft-deleted rows and return their affected count.
    pub fn restore_many(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        self.mutate_many(filter, Mutation::Restore)
    }

    /// Permanently remove a matching row regardless of its deletion marker.
    pub fn purge<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        self.mutate_one(filter, Mutation::Purge)
    }

    /// Permanently remove matching rows and return their affected count.
    pub fn purge_many(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        self.mutate_many(filter, Mutation::Purge)
    }

    /// Insert a document or update the row matching its unique target.
    pub fn upsert<I: Insertable<E>, R: FromRow<E>>(
        &self,
        document: I,
        target: ConflictTarget<E>,
    ) -> impl Future<Output = Result<R, DbError>> + use<E, I, R> {
        let future = self
            .validate()
            .and_then(|()| document.into_record())
            .map(|record| {
                self.collection.execute(Operation::Upsert {
                    document: Value::Object(record),
                    conflict_fields: target.into_value(),
                })
            });
        async move {
            decode_rows::<E, R>(future?.await?)?
                .pop()
                .ok_or_else(|| DbError::internal("upsert returned no row"))
        }
    }

    fn mutate_one<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
        mutation: Mutation,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        let future = self.validate().map(|()| {
            self.collection
                .mutate_model(filter.into_predicate(), mutation, false)
        });
        async move { Ok(decode_rows::<E, R>(future?.await?)?.pop()) }
    }

    fn mutate_many(
        &self,
        filter: Filter<E>,
        mutation: Mutation,
    ) -> impl Future<Output = Result<i64, DbError>> + use<E> {
        let future = self.validate().map(|()| {
            self.collection
                .mutate_model(filter.into_predicate(), mutation, true)
        });
        async move { decode_count(future?.await?) }
    }
}

fn decode_count(output: Output) -> Result<i64, DbError> {
    match output {
        Output::Count(count) => Ok(count),
        Output::Rows { .. } => Err(DbError::internal("expected an affected row count")),
    }
}

#[derive(Clone, Copy)]
pub(super) enum Mutation {
    Delete,
    Restore,
    Purge,
}

impl Mutation {
    pub(super) fn plan(
        self,
        binding: &DbBinding,
        route: &CapturedRoute,
        collection: &str,
        filter: model::ModelPredicate,
        actor_id: Option<&str>,
        many: bool,
    ) -> Result<CompiledQuery, DbError> {
        match self {
            Self::Delete => {
                crud::plan_delete_input(binding, route, collection, filter.into(), actor_id, many)
            }
            Self::Restore => {
                crud::plan_restore_input(binding, route, collection, filter.into(), actor_id, many)
            }
            Self::Purge => crud::plan_purge_input(binding, route, collection, filter.into(), many),
        }
    }

    pub(super) fn change_op(
        self,
        binding: &DbBinding,
        collection: &str,
    ) -> Result<ChangeOp, DbError> {
        Ok(match self {
            Self::Delete => {
                let schema = crate::descriptor::collection_schema(binding, collection)?;
                if crate::sql::lifecycle::soft_delete_column(&schema)?.is_some() {
                    ChangeOp::Update
                } else {
                    ChangeOp::Delete
                }
            }
            Self::Restore => ChangeOp::Update,
            Self::Purge => ChangeOp::Delete,
        })
    }
}
