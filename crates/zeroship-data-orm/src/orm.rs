//! The database API shared by Rust applications and the V8 adapter.
//!
//! A collection resolves against a deployment's descriptor. Preparation captures
//! the request's identity, actor, read set and transaction route synchronously;
//! execution can then yield without consulting another request's context.

use std::{cell::Cell, future::Future, marker::PhantomData, rc::Rc};
use zeroship_core::change_event::ChangeOp;
use zeroship_data_orm::binding::DbBinding;
pub use zeroship_data_orm::error::DbError;
pub use zeroship_data_sql::value::Value;

use crate::{backend::BackendHandle, compile::BuiltQuery, crud, tx_route::CapturedRoute};

/// A database connection bound to an app deployment.
#[derive(Clone, Debug)]
pub struct Database {
    context: crate::OrmContext,
    binding: DbBinding,
    backend: BackendHandle,
    actor_id: Option<String>,
    scope: Option<Rc<Cell<bool>>>,
}

impl Database {
    /// Open the configured backend and bind the deployment's runtime metadata.
    pub async fn connect(
        binding: DbBinding,
        options: crate::ConnectOptions,
        collections: Vec<(String, Value)>,
    ) -> Result<Self, DbError> {
        Self::from_schema(binding, options.connect().await?, collections)
    }

    /// Bind an already installed runtime descriptor to a backend.
    pub fn new(context: crate::OrmContext, binding: DbBinding, backend: BackendHandle) -> Self {
        Self {
            context,
            binding,
            backend,
            actor_id: None,
            scope: None,
        }
    }

    /// Install the host's collection descriptors before exposing this database.
    /// Validation completes before publication, so failure leaves the previous
    /// descriptor intact.
    pub fn from_schema(
        binding: DbBinding,
        backend: BackendHandle,
        collections: Vec<(String, Value)>,
    ) -> Result<Self, DbError> {
        let context = crate::OrmContext::new();
        context.with(|| {
            crate::descriptor::install_collections(&binding, collections)?;
            Ok(Self::new(context.clone(), binding, backend))
        })
    }

    /// The owner of this database's schemas, policies and transaction lanes.
    pub fn context(&self) -> &crate::OrmContext {
        &self.context
    }

    /// Install the app's immutable startup policy in this database context.
    pub fn install_mask_policy(&self, policy: Value) -> Result<(), DbError> {
        self.context
            .with(|| crate::protection::mask_policy::install_mask_policy(&self.binding, policy))
    }

    /// Attribute subsequent writes to an actor resolved by the host.
    #[must_use]
    pub fn with_actor(mut self, actor_id: Option<String>) -> Self {
        self.actor_id = actor_id;
        self
    }

    pub fn collection(&self, name: &str) -> Result<Collection, DbError> {
        self.context.with(|| {
            crate::compile::validate_collection(name)?;
            crate::descriptor::collection_schema(&self.binding, name)?;
            Ok(Collection {
                database: self.clone(),
                name: name.to_owned(),
            })
        })
    }

    pub fn binding(&self) -> &DbBinding {
        &self.binding
    }

    /// Bind generated collection metadata to this deployment's descriptor.
    pub fn entity<E: Entity>(&self) -> Result<EntityCollection<E>, DbError> {
        self.context.with(|| {
            let collection = self.collection(E::COLLECTION)?;
            let schema = crate::descriptor::collection_schema(&self.binding, E::COLLECTION)?;
            if schema.as_ref() != E::schema() {
                return Err(schema_mismatch::<E>());
            }
            Ok(EntityCollection {
                collection,
                schema,
                entity: PhantomData,
            })
        })
    }

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
                let route = self.capture_route().bind(self.backend.clone());
                let frame = crate::transaction::AtomicWriteFrame::begin(route).await?;
                let active = Rc::new(Cell::new(true));
                let _guard = ScopeGuard(active.clone());
                let mut transaction = self.clone();
                transaction.scope = Some(active.clone());
                let result = body(transaction).await;
                active.set(false);
                frame.finish(result).await
            })
            .await
    }

    fn check_scope(&self) -> Result<(), DbError> {
        check_scope(self.scope.as_ref())
    }

    fn capture_route(&self) -> CapturedRoute {
        CapturedRoute::capture(
            self.scope.as_ref().map(|_| self.binding.app_id()),
            self.binding.app_id(),
            self.binding.schema().clone(),
            self.backend.dialect(),
        )
    }
}

struct ScopeGuard(Rc<Cell<bool>>);
impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn check_scope(scope: Option<&Rc<Cell<bool>>>) -> Result<(), DbError> {
    if scope.is_some_and(|active| !active.get()) {
        return Err(DbError::validation(
            "transaction_scope_expired",
            "the ORM transaction has settled",
        ));
    }
    Ok(())
}

/// An ORM collection. Schema resolution and protection apply to every method.
#[derive(Clone, Debug)]
pub struct Collection {
    database: Database,
    name: String,
}

impl Collection {
    /// Prepare immediately, before returning the future to an executor.
    pub fn execute(
        &self,
        operation: Operation,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        let db = &self.database;
        db.context.with(|| {
            let prepared = db.check_scope().and_then(|()| {
                PreparedOperation::new(
                    db.binding.clone(),
                    &self.name,
                    db.capture_route(),
                    db.actor_id.clone(),
                    operation,
                )
            });
            let backend = db.backend.clone();
            let scope = db.scope.clone();
            async move {
                check_scope(scope.as_ref())?;
                prepared?.execute(backend).await
            }
        })
    }

    pub fn find(
        &self,
        filter: Value,
        options: Value,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        self.execute(Operation::Find { filter, options })
    }

    pub fn insert(&self, document: Value) -> impl Future<Output = Result<Output, DbError>> + use<> {
        self.execute(Operation::Insert { document })
    }

    pub fn update(
        &self,
        filter: Value,
        patch: Value,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        self.execute(Operation::Update {
            filter,
            patch,
            many: false,
        })
    }

    pub fn delete(&self, filter: Value) -> impl Future<Output = Result<Output, DbError>> + use<> {
        self.execute(Operation::Delete {
            filter,
            many: false,
        })
    }

    pub fn count(
        &self,
        filter: Value,
        options: Value,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        self.execute(Operation::Count { filter, options })
    }
}

/// A collection checked against migration-derived Rust metadata.
#[derive(Debug)]
pub struct EntityCollection<E: Entity> {
    collection: Collection,
    schema: std::sync::Arc<Value>,
    entity: PhantomData<E>,
}
fn schema_mismatch<E: Entity>() -> DbError {
    DbError::config(
        "orm_schema_mismatch",
        format!(
            "collection '{}': Rust metadata differs from the installed runtime descriptor; regenerate from the deployment's schema.runtime.json",
            E::COLLECTION,
        ),
    )
}
impl<E: Entity> EntityCollection<E> {
    fn validate(&self) -> Result<(), DbError> {
        self.collection.database.context.with(|| {
            let current = crate::descriptor::collection_schema(
                &self.collection.database.binding,
                E::COLLECTION,
            )?;
            if std::sync::Arc::ptr_eq(&current, &self.schema)
                || current.as_ref() == self.schema.as_ref()
            {
                Ok(())
            } else {
                Err(schema_mismatch::<E>())
            }
        })
    }
    pub fn find<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
        options: FindOptions,
    ) -> impl Future<Output = Result<Vec<R>, DbError>> + use<E, R> {
        let future = self.validate().map(|()| {
            self.collection
                .find(filter.into_value(), options.into_value::<E, R>())
        });
        async move { decode_rows::<E, R>(future?.await?) }
    }
    pub fn insert<I: Insertable<E>, R: FromRow<E>>(
        &self,
        document: I,
    ) -> impl Future<Output = Result<R, DbError>> + use<E, I, R> {
        let future = self
            .validate()
            .and_then(|()| document.into_record())
            .map(|record| self.collection.insert(Value::Object(record)));
        async move {
            decode_rows::<E, R>(future?.await?)?
                .pop()
                .ok_or_else(|| DbError::internal("insert returned no row"))
        }
    }
    pub fn update<C: Changeset<E>, R: FromRow<E>>(
        &self,
        filter: Filter<E>,
        changes: C,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, C, R> {
        let future = self
            .validate()
            .and_then(|()| changes.into_changes())
            .map(|fields| {
                self.collection.update(
                    filter.into_value(),
                    Value::Object([("$set".into(), Value::Object(fields))].into()),
                )
            });
        async move { Ok(decode_rows::<E, R>(future?.await?)?.pop()) }
    }
    pub fn delete<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        let future = self
            .validate()
            .map(|()| self.collection.delete(filter.into_value()));
        async move { Ok(decode_rows::<E, R>(future?.await?)?.pop()) }
    }
}
fn decode_rows<E: Entity, R: FromRow<E>>(output: Output) -> Result<Vec<R>, DbError> {
    let Output::Rows { rows, .. } = output else {
        return Err(DbError::internal("expected model rows"));
    };
    rows.into_iter()
        .map(|row| match row {
            Value::Object(fields) => R::from_row(Row::new(fields)),
            _ => Err(DbError::internal("database returned a non-record row")),
        })
        .collect()
}
mod codecs;
mod model;
pub use codecs::{Decimal, Point, Protected, sql_types};
pub use model::*;
pub use zeroship_data_macros::{Changeset, FromRow, Insertable, schema};
pub use zeroship_data_sql::value::Record;

/// Implementation support for generated metadata.
#[doc(hidden)]
pub mod __private {
    pub fn schema_value(json: &str) -> super::Value {
        serde_json::from_str(json).expect("schema macro emitted validated descriptor JSON")
    }
}

#[cfg(test)]
mod tests;

/// Semantic collection operations. Neither SQL nor a physical namespace is
/// accepted from the caller; the host's binding supplies database authority.
#[derive(Clone, Debug)]
pub enum Operation {
    Find {
        filter: Value,
        options: Value,
    },
    Insert {
        document: Value,
    },
    InsertMany {
        documents: Value,
    },
    Update {
        filter: Value,
        patch: Value,
        many: bool,
    },
    Delete {
        filter: Value,
        many: bool,
    },
    Purge {
        filter: Value,
        many: bool,
    },
    Restore {
        filter: Value,
        many: bool,
    },
    Upsert {
        document: Value,
        conflict_fields: Value,
    },
    Count {
        filter: Value,
        options: Value,
    },
    Aggregate {
        pipeline: Value,
        options: Value,
    },
    Distinct {
        field: String,
        filter: Value,
        options: Value,
    },
    Search {
        arguments: Value,
    },
    Near {
        arguments: Value,
    },
}

/// Result data before the adapter encodes it for its language runtime.
#[derive(Debug)]
pub enum Output {
    Rows { rows: Vec<Value>, has_masked: bool },
    Count(i64),
}

impl From<crud::read_pipeline::ApplyResult> for Output {
    fn from(result: crud::read_pipeline::ApplyResult) -> Self {
        Self::Rows {
            rows: result.rows,
            has_masked: result.has_masked,
        }
    }
}

enum Plan {
    Find {
        filter: Value,
        plan: Box<crud::FindPlan>,
    },
    Insert(Value),
    InsertMany(Value),
    Update {
        filter: Value,
        patch: Value,
        many: bool,
    },
    Mutation {
        query: BuiltQuery,
        operation: ChangeOp,
        many: bool,
    },
    Upsert {
        document: Value,
        conflict_fields: Value,
    },
    Count(BuiltQuery),
    Aggregate {
        query: BuiltQuery,
        groups: Vec<String>,
        columns: Option<Vec<String>>,
    },
    Distinct {
        query: BuiltQuery,
        masked: bool,
    },
    Search(crud::SearchPlan),
    Near(crud::NearPlan),
}

/// A single-use operation with its dispatch context frozen.
pub struct PreparedOperation {
    context: crate::OrmContext,
    binding: DbBinding,
    collection: String,
    route: CapturedRoute,
    actor_id: Option<String>,
    plan: Plan,
}

impl std::fmt::Debug for PreparedOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedOperation")
            .field("binding", &self.binding)
            .field("collection", &self.collection)
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

impl PreparedOperation {
    /// The adapter calls this while its request context and read-set are active.
    /// An unrelated binding cannot be combined with a captured route.
    pub fn new(
        binding: DbBinding,
        collection: &str,
        route: CapturedRoute,
        actor_id: Option<String>,
        operation: Operation,
    ) -> Result<Self, DbError> {
        if route.app_id() != binding.app_id() || route.schema() != binding.schema() {
            return Err(DbError::internal(
                "ORM binding does not match the captured database route",
            ));
        }
        crate::compile::validate_collection(collection)?;
        crate::descriptor::collection_schema(&binding, collection)?;
        let plan = match operation {
            Operation::Find { filter, options } => {
                let plan = crud::plan_find(&binding, collection, &filter, &options);
                Plan::Find {
                    filter,
                    plan: Box::new(plan),
                }
            }
            Operation::Insert { document } => Plan::Insert(document),
            Operation::InsertMany { documents } => Plan::InsertMany(documents),
            Operation::Update {
                filter,
                patch,
                many,
            } => Plan::Update {
                filter,
                patch,
                many,
            },
            Operation::Delete { filter, many } => Plan::Mutation {
                query: if many {
                    crud::plan_delete_many
                } else {
                    crud::plan_delete_one
                }(&binding, &route, collection, filter, actor_id.as_deref())?,
                operation: ChangeOp::Update,
                many,
            },
            Operation::Purge { filter, many } => Plan::Mutation {
                query: if many {
                    crud::plan_purge_many
                } else {
                    crud::plan_purge_one
                }(&binding, &route, collection, filter)?,
                operation: ChangeOp::Delete,
                many,
            },
            Operation::Restore { filter, many } => Plan::Mutation {
                query: if many {
                    crud::plan_restore_many
                } else {
                    crud::plan_restore_one
                }(&binding, &route, collection, filter, actor_id.as_deref())?,
                operation: ChangeOp::Update,
                many,
            },
            Operation::Upsert {
                document,
                conflict_fields,
            } => Plan::Upsert {
                document,
                conflict_fields,
            },
            Operation::Count { filter, options } => Plan::Count(crud::plan_count(
                &binding, &route, collection, filter, &options,
            )?),
            Operation::Aggregate { pipeline, options } => {
                let (query, columns) =
                    crud::plan_aggregate(&binding, &route, collection, &pipeline, &options)?;
                Plan::Aggregate {
                    query,
                    columns,
                    groups: crud::aggregate_group_fields(&pipeline),
                }
            }
            Operation::Distinct {
                field,
                filter,
                options,
            } => {
                let (query, masked) =
                    crud::plan_distinct(&binding, &route, collection, &field, filter, &options)?;
                Plan::Distinct { query, masked }
            }
            Operation::Search { arguments } => Plan::Search(crud::plan_search(
                &binding,
                route.dialect(),
                collection,
                &arguments,
            )?),
            Operation::Near { arguments } => Plan::Near(crud::plan_near(
                &binding,
                route.dialect(),
                collection,
                &arguments,
            )?),
        };
        Ok(Self {
            context: crate::orm_context::current(),
            binding,
            collection: collection.to_owned(),
            route,
            actor_id,
            plan,
        })
    }

    pub async fn execute(self, backend: BackendHandle) -> Result<Output, DbError> {
        self.context
            .clone()
            .scope(self.execute_in_context(backend))
            .await
    }
    async fn execute_in_context(self, backend: BackendHandle) -> Result<Output, DbError> {
        let Self {
            context: _,
            binding,
            collection,
            route,
            actor_id,
            plan,
        } = self;
        if route.dialect() != backend.dialect() {
            return Err(DbError::internal(
                "ORM backend does not match the prepared dialect",
            ));
        }
        let route = route.bind(backend);
        let result = match plan {
            Plan::Find { filter, plan } => {
                Box::pin(crud::run_find(binding, collection, route, filter, *plan))
                    .await?
                    .into()
            }
            Plan::Insert(document) => Box::pin(crud::run_insert(
                binding, collection, route, document, actor_id,
            ))
            .await?
            .into(),
            Plan::InsertMany(documents) => Box::pin(crud::run_insert_many(
                binding, collection, route, documents, actor_id,
            ))
            .await?
            .into(),
            Plan::Update {
                filter,
                patch,
                many: false,
            } => {
                let (rows, has_masked) = Box::pin(crud::run_update_one(
                    binding, collection, route, filter, patch, actor_id,
                ))
                .await?;
                Output::Rows { rows, has_masked }
            }
            Plan::Update {
                filter,
                patch,
                many: true,
            } => Output::Count(
                Box::pin(crud::run_update_many(
                    binding, collection, route, filter, patch, actor_id,
                ))
                .await? as i64,
            ),
            Plan::Mutation {
                query,
                operation,
                many: false,
            } => Box::pin(crud::exec_mutation_then_read(
                binding, collection, route, query, operation,
            ))
            .await?
            .into(),
            Plan::Mutation {
                query,
                operation,
                many: true,
            } => Output::Count(
                Box::pin(crate::exec::exec_mutation_with_emit(
                    query,
                    &route,
                    &collection,
                    operation,
                ))
                .await?
                .len() as i64,
            ),
            Plan::Upsert {
                document,
                conflict_fields,
            } => Box::pin(crud::run_upsert(
                binding,
                collection,
                route,
                document,
                conflict_fields,
                actor_id,
            ))
            .await?
            .into(),
            Plan::Count(query) => {
                Output::Count(Box::pin(crate::exec::exec_count(&route, query)).await?)
            }
            Plan::Aggregate {
                query,
                groups,
                columns,
            } => Box::pin(crud::exec_aggregate_read(
                binding, collection, route, query, groups, columns,
            ))
            .await?
            .into(),
            Plan::Distinct { query, masked } => {
                let result = Box::pin(crud::exec_distinct_read(
                    binding, collection, route, query, masked,
                ))
                .await?;
                let rows = result
                    .rows
                    .into_iter()
                    .filter_map(|row| match row {
                        Value::Object(map) => map.into_values().next(),
                        _ => None,
                    })
                    .collect();
                Output::Rows {
                    rows,
                    has_masked: result.has_masked,
                }
            }
            Plan::Search(plan) => Box::pin(crud::run_search(&route, binding, collection, plan))
                .await?
                .into(),
            Plan::Near(plan) => Box::pin(crud::run_near(&route, binding, collection, plan))
                .await?
                .into(),
        };
        Ok(result)
    }
}
