//! The database API shared by Rust applications and the V8 adapter.
//!
//! A collection resolves against a deployment's descriptor. Preparation captures
//! the request's identity, actor, read set and transaction route synchronously;
//! execution can then yield without consulting another request's context.

use crate::schema::{FieldMap, Schema};
pub use crate::value::Value;
use std::{cell::Cell, future::Future, marker::PhantomData, rc::Rc, sync::Arc};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::cdc::ChangeOp;
pub use zeroship_data_orm::error::DbError;

use crate::{
    backend::BackendHandle, crud, metrics::UsageSink, sql::compiler::CompiledQuery,
    tx_route::CapturedRoute,
};

/// A database connection bound to an app deployment.
#[derive(Clone, Debug)]
pub struct Database {
    identity: Rc<()>,
    context: crate::OrmContext,
    binding: DbBinding,
    backend: BackendHandle,
    actor_id: Option<String>,
    usage: Option<Arc<dyn UsageSink>>,
    scope: Option<Rc<Cell<bool>>>,
    transaction_scope: Option<crate::transaction::scope::TransactionScope>,
}

impl Database {
    /// Prepare a relational read while the request's transaction and read-set are active.
    pub fn read(&self, query: ReadQuery) -> impl Future<Output = Result<Output, DbError>> + use<> {
        let collection = query.source.collection.clone();
        Collection {
            database: self.clone(),
            name: collection,
        }
        .execute(Operation::Read(Box::new(query)))
    }
    /// Open the configured backend and install its native schema.
    pub async fn connect(
        binding: DbBinding,
        options: crate::ConnectOptions,
        schema: Schema,
    ) -> Result<Self, DbError> {
        Self::from_schema(binding, options.connect().await?, schema)
    }

    /// Bind an installed schema to a backend. The database reports no usage
    /// until a host attaches a sink with [`Self::with_usage_sink`].
    pub fn new(context: crate::OrmContext, binding: DbBinding, backend: BackendHandle) -> Self {
        Self {
            identity: Rc::new(()),
            context,
            binding,
            backend,
            actor_id: None,
            usage: None,
            scope: None,
            transaction_scope: None,
        }
    }

    /// Validate and install the host's schema in an independent context.
    pub fn from_schema(
        binding: DbBinding,
        backend: BackendHandle,
        schema: Schema,
    ) -> Result<Self, DbError> {
        let context = crate::OrmContext::new();
        context.with(|| {
            crate::descriptor::install_collections(&binding, schema)?;
            Ok(Self::new(context.clone(), binding, backend))
        })
    }

    /// The owner of this database's schemas, policies and transaction lanes.
    pub fn context(&self) -> &crate::OrmContext {
        &self.context
    }

    /// A handle on the same binding, backend, installed schema, mask policy,
    /// protection floors and usage sink, with a transaction lane of its own.
    ///
    /// Top-level transactions serialize through one lane per app, so a second
    /// transaction opened on this handle - or on a clone of it - waits for the
    /// first to settle. A fork admits its own, so a host that wants several
    /// transactions in flight at once takes one fork per concurrent unit of
    /// work. Concurrency is then bounded by the backend's connection pool: a
    /// fork whose transaction cannot get a connection fails with the pool's
    /// acquire timeout rather than running unbounded.
    ///
    /// Forks are separate handles, not clones: a read source built on one is
    /// refused by the other, which is what keeps a transaction's aliases from
    /// executing on a lane that is not its own.
    ///
    /// Locks are not forgiving here. A fork awaited from inside another
    /// transaction's callback can still block on rows that transaction holds,
    /// and only the lock timeout ends that.
    ///
    /// # Errors
    /// `unsupported_backend_feature` on a backend that reserves one
    /// transaction connection per app, where a second lane could only
    /// serialize invisibly or refuse at BEGIN; `transaction_scope_expired` on a
    /// settled transaction handle.
    pub fn independent(&self) -> Result<Self, DbError> {
        self.check_scope()?;
        if !self.backend.admits_concurrent_transactions() {
            return Err(unsupported_backend_feature("independent transaction lanes"));
        }
        Ok(Self {
            identity: Rc::new(()),
            context: self.context.fork_lanes(),
            binding: self.binding.clone(),
            backend: self.backend.clone(),
            actor_id: self.actor_id.clone(),
            usage: self.usage.clone(),
            scope: None,
            transaction_scope: None,
        })
    }

    /// Confirm the backend is reachable, in one round trip on an autocommit
    /// lease.
    ///
    /// It takes no transaction lane, installs no session authority and reads no
    /// table, so it answers while this handle's transaction is open. The wait
    /// is the backend's connection wait: on a pool, the acquire timeout.
    ///
    /// # Errors
    /// The backend's connection failure, or `transaction_scope_expired` on a
    /// settled transaction handle.
    pub async fn check_connection(&self) -> Result<(), DbError> {
        self.check_scope()?;
        let backend = self.backend.clone();
        self.context
            .scope(async move { backend.check_connection().await })
            .await
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

    /// Report this binding's usage to `sink`, including work done in the
    /// transactions it opens. The sink carries the host's attribution; the ORM
    /// derives none from the binding.
    #[must_use]
    pub fn with_usage_sink(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage = Some(sink);
        self
    }

    pub fn collection(&self, name: &str) -> Result<Collection, DbError> {
        self.context.with(|| {
            self.check_scope()?;
            crate::sql::mapping::validate_collection(name)?;
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

    /// Check the native model against the host's installed schema.
    pub fn entity<E: Entity>(&self) -> Result<EntityCollection<E>, DbError> {
        self.context.with(|| {
            let collection = self.collection(E::COLLECTION)?;
            let schema = crate::descriptor::collection_schema(&self.binding, E::COLLECTION)?;
            if schema.as_ref() != E::schema().fields() {
                let field = schema
                    .keys()
                    .chain(E::schema().keys())
                    .find(|name| schema.get(*name) != E::schema().get(*name))
                    .expect("unequal field maps differ at a field");
                return Err(DbError::config(
                    "orm_schema_mismatch",
                    format!(
                        "collection '{}': column '{field}' differs from the installed schema",
                        E::COLLECTION,
                    ),
                ));
            }
            Ok(EntityCollection {
                collection,
                schema,
                entity: PhantomData,
            })
        })
    }

    fn check_scope(&self) -> Result<(), DbError> {
        check_scope(self.scope.as_ref())
    }

    fn capture_route(&self) -> CapturedRoute {
        CapturedRoute::capture(
            self.transaction_scope.as_ref(),
            self.binding.app_id(),
            self.binding.schema().clone(),
            self.backend.sql_registration().clone(),
            self.backend.connection_identity(),
            self.usage.clone(),
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

/// A feature that only runs inside a transaction was requested on a handle
/// that was not passed to a transaction callback.
pub(crate) fn transaction_required(feature: &str) -> DbError {
    DbError::validation_hinted(
        "transaction_required",
        format!("{feature} require a transaction handle"),
        "Use the database handle passed to a transaction callback.",
    )
}

/// The handle's backend cannot provide a requested feature.
pub(crate) fn unsupported_backend_feature(feature: &str) -> DbError {
    DbError::validation(
        "unsupported_backend_feature",
        format!("the configured database backend does not support {feature}"),
    )
}

/// An ORM collection. Schema resolution and protection apply to every method.
#[derive(Clone, Debug)]
pub struct Collection {
    database: Database,
    name: String,
}

impl Collection {
    fn dispatch(
        &self,
        prepared: Result<PreparedOperation, DbError>,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        let backend = self.database.backend.clone();
        let scope = self.database.scope.clone();
        async move {
            check_scope(scope.as_ref())?;
            prepared?.execute(backend).await
        }
    }

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
            self.dispatch(prepared)
        })
    }

    fn update_model(
        &self,
        filter: model::ModelPredicate,
        patch: crud::update::Input,
        many: bool,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        let db = &self.database;
        db.context.with(|| {
            let prepared = db.check_scope().and_then(|()| {
                PreparedOperation::new_model_update(
                    db.binding.clone(),
                    &self.name,
                    db.capture_route(),
                    db.actor_id.clone(),
                    filter,
                    patch,
                    many,
                )
            });
            self.dispatch(prepared)
        })
    }

    fn mutate_model(
        &self,
        filter: model::ModelPredicate,
        mutation: mutations::Mutation,
        many: bool,
    ) -> impl Future<Output = Result<Output, DbError>> + use<> {
        let db = &self.database;
        db.context.with(|| {
            let prepared = db.check_scope().and_then(|()| {
                PreparedOperation::new_model_mutation(
                    db.binding.clone(),
                    &self.name,
                    db.capture_route(),
                    db.actor_id.clone(),
                    filter,
                    mutation,
                    many,
                )
            });
            self.dispatch(prepared)
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

/// A collection checked against its native Rust metadata.
#[derive(Debug)]
pub struct EntityCollection<E: Entity> {
    collection: Collection,
    schema: std::sync::Arc<FieldMap>,
    entity: PhantomData<E>,
}
fn schema_mismatch_for(collection: &str) -> DbError {
    DbError::config(
        "orm_schema_mismatch",
        format!(
            "collection '{}': model metadata differs from the installed schema",
            collection,
        ),
    )
}
impl<E: Entity> EntityCollection<E> {
    fn validate(&self) -> Result<(), DbError> {
        read_builder::validate_bound_schema(&self.collection.database, E::COLLECTION, &self.schema)
    }

    #[expect(
        clippy::future_not_send,
        reason = "ORM operations use thread-local compio sessions"
    )]
    fn dispatch<F>(
        &self,
        prepared: Result<F, DbError>,
    ) -> impl Future<Output = Result<Output, DbError>> + use<E, F>
    where
        F: Future<Output = Result<Output, DbError>>,
    {
        let database = self.collection.database.clone();
        let schema = self.schema.clone();
        async move {
            read_builder::validate_bound_schema(&database, E::COLLECTION, &schema)?;
            prepared?.await
        }
    }
    pub fn find<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
        options: FindOptions,
    ) -> impl Future<Output = Result<Vec<R>, DbError>> + use<E, R> {
        self.query().filter(filter).with_options(options).all()
    }
    pub fn insert<I: Insertable<E>, R: FromRow<E>>(
        &self,
        document: I,
    ) -> impl Future<Output = Result<R, DbError>> + use<E, I, R> {
        let future = self
            .validate()
            .and_then(|()| document.into_record())
            .map(|record| self.collection.insert(Value::Object(record)));
        let future = self.dispatch(future);
        async move {
            decode_rows::<E, R>(future.await?)?
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
            .map(|patch| {
                self.collection
                    .update_model(filter.into_predicate(), patch.into_update(), false)
            });
        let future = self.dispatch(future);
        async move { Ok(decode_rows::<E, R>(future.await?)?.pop()) }
    }
    pub fn delete<R: FromRow<E>>(
        &self,
        filter: Filter<E>,
    ) -> impl Future<Output = Result<Option<R>, DbError>> + use<E, R> {
        self.mutate_one(filter, mutations::Mutation::Delete)
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
mod postgres;
pub use postgres::{AdvisoryKey, Postgres, SessionLease, TransactionSetting};
mod timestamp;
pub use timestamp::TimestampExpr;
mod model;
mod mutations;
mod relations;
pub use mutations::ConflictTarget;
mod transactions;
pub use crate::error::IsolationLevel;
pub use codecs::{sql_types, Decimal, Point, Protected};
pub use model::*;
pub use transactions::TransactionOptions;
pub mod read;
pub use read::{ReadJoin, ReadProjection, ReadQuery, ReadSource};
mod read_builder;
mod read_input;
pub use crate::value::Record;
pub use read_builder::*;
pub use zeroship_data_macros::{schema, Changeset, FromRow, Insertable};

#[cfg(test)]
mod tests;

/// Semantic collection operations. Neither SQL nor a physical namespace is
/// accepted from the caller; the host's binding supplies database authority.
#[derive(Clone, Debug)]
pub enum Operation {
    Read(Box<ReadQuery>),
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
    Read(Box<read::PreparedRead>),
    Find {
        filter: Value,
        plan: Box<crud::FindPlan>,
        relations: Option<Box<relations::FindRelations>>,
    },
    Insert(Value),
    InsertMany(Value),
    Update {
        filter: crud::predicate::Input,
        patch: crud::update::Input,
        many: bool,
    },
    Mutation {
        query: CompiledQuery,
        operation: ChangeOp,
        many: bool,
    },
    Upsert {
        document: Value,
        conflict_fields: Value,
    },
    Count(CompiledQuery),
    Aggregate {
        query: CompiledQuery,
        groups: Vec<String>,
        projection: Option<crud::aggregate::AggregateProjection>,
    },
    Distinct {
        query: CompiledQuery,
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
        validate_target(&binding, collection, &route)?;
        let plan = match operation {
            Operation::Read(query) => {
                if query.source.collection != collection {
                    return Err(read::invalid("read root does not match its collection"));
                }
                Plan::Read(Box::new(read::PreparedRead::new(
                    &binding,
                    route.sql_registration(),
                    route.in_tx(),
                    *query,
                )?))
            }
            Operation::Find {
                filter,
                mut options,
            } => {
                let relations = relations::FindRelations::new(
                    &binding,
                    route.sql_registration(),
                    collection,
                    &mut options,
                )?
                .map(Box::new);
                let plan = crud::plan_find(&binding, collection, &filter, &options)?;
                Plan::Find {
                    filter,
                    plan: Box::new(plan),
                    relations,
                }
            }
            Operation::Insert { document } => Plan::Insert(document),
            Operation::InsertMany { documents } => Plan::InsertMany(documents),
            Operation::Update {
                filter,
                patch,
                many,
            } => Plan::Update {
                filter: filter.into(),
                patch: patch.into(),
                many,
            },
            Operation::Delete { filter, many } => Plan::Mutation {
                query: if many {
                    crud::plan_delete_many
                } else {
                    crud::plan_delete_one
                }(&binding, &route, collection, filter, actor_id.as_deref())?,
                operation: mutations::Mutation::Delete.change_op(&binding, collection)?,
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
                let (query, projection) =
                    crud::plan_aggregate(&binding, &route, collection, &pipeline, &options)?;
                Plan::Aggregate {
                    query,
                    projection,
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
                route.sql_registration(),
                collection,
                &arguments,
            )?),
            Operation::Near { arguments } => Plan::Near(crud::plan_near(
                &binding,
                route.sql_registration(),
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

    fn new_model_update(
        binding: DbBinding,
        collection: &str,
        route: CapturedRoute,
        actor_id: Option<String>,
        filter: model::ModelPredicate,
        patch: crud::update::Input,
        many: bool,
    ) -> Result<Self, DbError> {
        validate_target(&binding, collection, &route)?;
        Ok(Self {
            context: crate::orm_context::current(),
            binding,
            collection: collection.to_owned(),
            route,
            actor_id,
            plan: Plan::Update {
                filter: filter.into(),
                patch,
                many,
            },
        })
    }

    fn new_model_mutation(
        binding: DbBinding,
        collection: &str,
        route: CapturedRoute,
        actor_id: Option<String>,
        filter: model::ModelPredicate,
        mutation: mutations::Mutation,
        many: bool,
    ) -> Result<Self, DbError> {
        validate_target(&binding, collection, &route)?;
        let query = mutation.plan(
            &binding,
            &route,
            collection,
            filter,
            actor_id.as_deref(),
            many,
        )?;
        let operation = mutation.change_op(&binding, collection)?;
        Ok(Self {
            context: crate::orm_context::current(),
            binding,
            collection: collection.to_owned(),
            route,
            actor_id,
            plan: Plan::Mutation {
                query,
                operation,
                many,
            },
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
        let route = route.bind(backend)?;
        let result = match plan {
            Plan::Read(plan) => Box::pin(plan.execute(&binding, &route)).await?,
            Plan::Find {
                filter,
                plan,
                relations,
            } => {
                if let Some(relations) = &relations {
                    relations.validate_schemas(&binding)?;
                }
                let mut result = Box::pin(crud::run_find(
                    binding.clone(),
                    collection,
                    route.clone(),
                    filter,
                    *plan,
                ))
                .await?;
                if let Some(relations) = relations {
                    Box::pin(relations.apply(&binding, &route, &mut result)).await?;
                }
                result.into()
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
                Box::pin(crate::exec::exec_mutation_count_with_emit(
                    query,
                    &route,
                    &collection,
                    operation,
                ))
                .await? as i64,
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
                projection,
            } => Box::pin(crud::exec_aggregate_read(
                binding, collection, route, query, groups, projection,
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

fn validate_target(
    binding: &DbBinding,
    collection: &str,
    route: &CapturedRoute,
) -> Result<(), DbError> {
    route.validate_binding(binding)?;
    crate::sql::mapping::validate_collection(collection)?;
    crate::descriptor::collection_schema(binding, collection)?;
    Ok(())
}
