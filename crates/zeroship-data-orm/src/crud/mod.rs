//! Plan and execute collection operations for Rust callers and the V8 adapter.
//!
//! Synchronous planning captures descriptors, records read dependencies and sanitizes
//! unmask hints before execution can yield. Async execution uses the captured route
//! for ordinary queries and search, then applies result protection and decoding.
//! V8 promise creation and delivery belong to the adapter.

use crate::value::Value;

use crate::assignments::AssignmentPlan;
use crate::exec::{exec_mutation_count_with_emit, exec_mutation_with_emit, exec_query};
use crate::sql::lifecycle::{concurrency_column, soft_delete_column};
use crate::sql::mapping;
use crate::tx_route::TxRoute;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

use crate::protection::{mask_pass, protection_floor, unmask};

pub(crate) mod assignment_pass;

pub(crate) mod aggregate;
mod bytes_pass;
pub(crate) mod delete;
mod identity;
pub(crate) mod insert;
pub(crate) mod internal;
pub(crate) mod predicate;
pub(crate) mod read;
pub mod read_pipeline;
pub(crate) mod resolved;
pub(crate) mod search;
pub(crate) mod update;
mod update_validation;
pub mod upsert;
mod write_pipeline;

#[cfg(test)]
pub use write_pipeline::{
    reset_write_path_counters_for_tests, write_path_counters_for_tests, WritePathCounters,
};

/// Execute a row-returning mutation, emit its change event and process the result.
/// Owned arguments let the returned future outlive the dispatch closure.
pub async fn exec_mutation_then_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: crate::sql::compiler::CompiledQuery,
    op: zeroship_data_orm::cdc::ChangeOp,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = exec_mutation_with_emit(bq, &route, &coll, op, &binding).await?;
    read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}

/// Execute an aggregate with its grouping and result-column metadata.
/// The owned metadata remains available while the read pipeline borrows it.
pub(crate) async fn exec_aggregate_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: crate::sql::compiler::CompiledQuery,
    group_fields: Vec<String>,
    result_projection: Option<aggregate::AggregateProjection>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let mut rows = exec_query(&route, bq).await?;
    if let Some(projection) = &result_projection {
        crate::orm::read::decode_scalars(route.sql_registration(), &projection.schema, &mut rows)?;
    }
    read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            unmask_columns: &[],
            schema_field_scope: match &result_projection {
                Some(_) => read_pipeline::SchemaFieldScope::Only(group_fields.as_slice()),
                None => read_pipeline::SchemaFieldScope::All,
            },
            // A `$group` result's keys are accumulator aliases, which no
            // descriptor declares, so the declared surface would drop
            // every one of them. This is the ONLY call site in the crate
            // that names a surface; every other one takes the default.
            row_surface: match &result_projection {
                Some(projection) => {
                    read_pipeline::RowSurface::Projected(projection.columns.as_slice())
                }
                None => read_pipeline::RowSurface::Declared,
            },
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await
}

/// Apply distinct-result decoding without decrypting a masked display value.
pub async fn exec_distinct_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: crate::sql::compiler::CompiledQuery,
    reads_masked_value: bool,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = exec_query(&route, bq).await?;
    read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            apply_decrypt: !reads_masked_value,
            wrap_masked: false,
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await
}

/// Record `(collection, filter)` into the active query's read-set.
///
/// Resolves the descriptor entry the predicate has to be lowered against - a
/// conjunct on a masked column compares against the mask, not the value the
/// caller wrote. An undeclared collection records nothing rather than recording
/// an unlowered predicate: the dispatch this call precedes is about to reject
/// with `collection_not_declared`, so there is no subscription to narrow, and a
/// predicate built without a schema is exactly the silent false negative
/// `read_set` refuses to produce.
fn record_read_set(binding: &DbBinding, collection: &str, filter: &Value) {
    if !crate::cdc::read_set::is_active() {
        return;
    }
    let Ok(schema) = crate::descriptor::collection_schema(binding, collection) else {
        return;
    };
    crate::cdc::read_set::record_if_active(collection, filter, &schema);
}

pub fn aggregate_group_fields(pipeline: &Value) -> Vec<String> {
    let Some(stages) = pipeline.as_array() else {
        return Vec::new();
    };
    let Some(group_val) = stages
        .iter()
        .find_map(|stage| stage.as_object().and_then(|obj| obj.get("$group")))
    else {
        return Vec::new();
    };
    let Some(group_obj) = group_val.as_object() else {
        return Vec::new();
    };
    match group_obj.get("by") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// find — read path
// ---------------------------------------------------------------------------

// `dispatch_find_one` does not exist: `Collection.findOne` was removed
// (Convex-style consolidation). The SDK reaches the same "first matching
// row" semantic via `find(filter).first()` / `.unique()` / `.last()` on
// the Query terminal, which composes the existing `dispatch_find` with
// `LIMIT 1` (or `LIMIT 2` for strict `.unique()`).

fn invalid_read_option(message: impl Into<String>) -> DbError {
    DbError::validation("invalid_read", message.into())
}

fn validate_dynamic_options(opts: &Value, operation: &str) -> Result<(), DbError> {
    if opts.is_null() || opts.is_object() {
        Ok(())
    } else {
        Err(invalid_read_option(format!(
            "{operation} options must be an object"
        )))
    }
}

fn parse_include_deleted(opts: &Value) -> Result<bool, DbError> {
    match opts.get("include_deleted") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid_read_option("include_deleted must be a boolean")),
        None => Ok(false),
    }
}

fn parse_unmask_opt(opt: Option<&Value>) -> Result<Vec<String>, DbError> {
    let Some(value) = opt else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = value else {
        return Err(invalid_read_option("unmask must be an array"));
    };
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_read_option("unmask entries must be strings"))
        })
        .collect()
}

/// The eagerly-evaluated inputs of a `find`, produced by [`plan_find`] and
/// consumed by [`run_find`].
///
/// The read-set and V8-owned inputs are captured before asynchronous execution.
#[derive(Debug)]
pub struct FindPlan {
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<Value>,
    select: Option<Value>,
    unmask_columns: Vec<String>,
    unmask_actor: Option<Value>,
    unmask_rejected_claim: Option<Value>,
    unmask_reason: Option<String>,
    include_deleted: bool,
}

/// The EAGER half of `find`. Everything here must run while the dispatching
/// handler is still the active one on this thread.
///
/// The read-set is thread-local query state, so it must be recorded before the
/// returned future can be polled outside the dispatching handler.
pub fn plan_find(
    binding: &DbBinding,
    collection: &str,
    filter: &Value,
    opts: &Value,
) -> Result<FindPlan, DbError> {
    validate_dynamic_options(opts, "find")?;
    let limit = match opts.get("limit") {
        Some(value) => {
            let value = value
                .as_i64()
                .ok_or_else(|| invalid_read_option("limit must be an integer"))?;
            Some(
                crate::sql::RowLimit::new(value)
                    .map_err(|error| invalid_read_option(error.to_string()))?
                    .get(),
            )
        }
        None => Some(crate::sql::MAX_ROW_LIMIT),
    };
    let offset = match opts.get("offset") {
        Some(value) => {
            let value = value
                .as_i64()
                .ok_or_else(|| invalid_read_option("offset must be an integer"))?;
            Some(
                crate::sql::RowOffset::new(value)
                    .map_err(|error| invalid_read_option(error.to_string()))?
                    .get(),
            )
        }
        None => None,
    };
    let include_deleted = parse_include_deleted(opts)?;
    let unmask_reason = match opts.get("unmaskReason") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| invalid_read_option("unmaskReason must be a string"))?
                .to_string(),
        ),
        None => None,
    };
    let unmask_columns = parse_unmask_opt(opts.get("unmask"))?;

    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, filter);

    // DB-3: strip an app-supplied reserved `auto` system actor — a find with
    // `{unmask, actor:{kind:"auto"}}` must not impersonate the platform.
    let unmask_sanitized = crate::protection::unmask::sanitize_app_actor(
        opts.get("actor").cloned().filter(|v| !v.is_null()),
    );

    Ok(FindPlan {
        limit,
        offset,
        order_by: opts.get("orderBy").cloned(),
        select: opts.get("select").cloned(),
        unmask_columns,
        unmask_actor: unmask_sanitized.actor,
        unmask_rejected_claim: unmask_sanitized.rejected_claim,
        unmask_reason,
        include_deleted,
    })
}

/// The DEFERRED half of `find`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// `route` is passed in rather than captured because the routing decision must
/// be frozen while the scope is live; see `crate::tx_route`.
pub async fn run_find(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: Value,
    plan: FindPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    // Unmask reads follow this operation's transaction route. Authorization and
    // audit writes use the backend separately so creator rollback cannot erase
    // an attempt's audit record.
    if !plan.unmask_columns.is_empty() {
        crate::protection::unmask::authorize_query_hint(
            &route,
            &binding,
            &coll,
            &plan.unmask_columns,
            &plan.unmask_actor,
            plan.unmask_rejected_claim.as_ref(),
            &plan.unmask_reason,
        )
        .await?;
    }

    // Resolve the deployment's descriptor before compiling its projection.
    // Default reads use visible value columns; raw storage requires unmask access.
    let schema_hint = crate::descriptor::collection_schema(&binding, &coll)?;
    let projected = plan
        .select
        .as_ref()
        .and_then(Value::as_array)
        .filter(|fields| !fields.is_empty())
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
    // Soft-delete auto-filter gate.
    let filter_soft_deleted = assignment_pass::should_filter_soft_deleted(plan.include_deleted);
    let bq = read::find(
        binding.schema(),
        &coll,
        &schema_hint,
        filter,
        plan.limit,
        plan.offset,
        plan.order_by.as_ref(),
        plan.select.as_ref(),
        &plan.unmask_columns,
        filter_soft_deleted,
        route.sql_registration(),
    )
    .map_err(DbError::from)?;
    let rows = exec_query(&route, bq).await?;
    let result = read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            unmask_columns: &plan.unmask_columns,
            schema_field_scope: read_pipeline::SchemaFieldScope::All,
            row_surface: projected
                .as_ref()
                .map_or(read_pipeline::RowSurface::Declared, |fields| {
                    read_pipeline::RowSurface::Projected(fields)
                }),
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await?;
    // The audit runs AFTER the rows are in hand and BEFORE they are
    // lowered: a failure here must refuse the read, not log it and
    // return the plaintext anyway.
    if !plan.unmask_columns.is_empty() {
        crate::protection::unmask::audit_query_hint_granted(
            &route,
            &binding,
            &coll,
            &plan.unmask_columns,
            &plan.unmask_actor,
            plan.unmask_rejected_claim.as_ref(),
            &plan.unmask_reason,
        )
        .await?;
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// insert / insertMany — write paths returning the row(s)
// ---------------------------------------------------------------------------

/// Insert a document using the route and actor captured before async execution.
pub async fn run_insert(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    doc: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    let allocates_identity = identity::requires_allocation(&schema, &doc);
    route
        .sql_registration()
        .check(&insert::requirements(allocates_identity))
        .map_err(mapping::QueryError::from)?;
    let frame;
    let route = if allocates_identity {
        frame = Some(crate::transaction::AtomicWriteFrame::begin(route).await?);
        frame.as_ref().expect("opened write frame").route()
    } else {
        frame = None;
        &route
    };
    let result = async {
        let mut doc = doc;
        // The write pipeline's encryption stage takes a key store, not a backend:
        // it issues no SQL of its own, so it has no routing decision to make. The
        // store comes off the route this insert will run on, which is the handle
        // that will store the ciphertext.
        write_pipeline::apply(
            route.backend().key_store(),
            route,
            &binding,
            &coll,
            &mut doc,
            write_pipeline::ApplyMode::Insert {
                actor_id: actor_id.as_deref(),
            },
        )
        .await?;
        let bq = insert::build_one(
            binding.schema(),
            &coll,
            &schema,
            doc,
            route.sql_registration(),
        )
        .map_err(DbError::from)?;
        let rows = exec_mutation_with_emit(
            bq,
            route,
            &coll,
            zeroship_data_orm::cdc::ChangeOp::Insert,
            &binding,
        )
        .await?;
        read_pipeline::apply(
            route,
            &binding,
            &coll,
            rows,
            read_pipeline::ApplyOptions::default(),
        )
        .await
    }
    .await;
    match frame {
        Some(frame) => frame.finish(result).await,
        None => result,
    }
}

/// Insert documents atomically using the captured actor for assignments.
pub async fn run_insert_many(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    docs: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    let allocates_identity = identity::requires_allocation(&schema, &docs);
    route
        .sql_registration()
        .check(&insert::requirements(allocates_identity))
        .map_err(mapping::QueryError::from)?;
    let frame = crate::transaction::AtomicWriteFrame::begin(route).await?;
    let route = frame.route();
    let result = async {
        let mut docs = docs;
        prepare_insert_many_docs_for_binding(
            route.backend().key_store(),
            route,
            &mut docs,
            &binding,
            &coll,
            actor_id.as_deref(),
        )
        .await?;
        let queries = insert::build_many(
            binding.schema(),
            &coll,
            &schema,
            docs,
            route.sql_registration(),
        )
        .map_err(DbError::from)?;
        let mut rows = Vec::new();
        for query in queries {
            rows.extend(
                exec_mutation_with_emit(
                    query,
                    route,
                    &coll,
                    zeroship_data_orm::cdc::ChangeOp::Insert,
                    &binding,
                )
                .await?,
            );
        }
        read_pipeline::apply(
            route,
            &binding,
            &coll,
            rows,
            read_pipeline::ApplyOptions::default(),
        )
        .await
    }
    .await;
    frame.finish(result).await
}

// ---------------------------------------------------------------------------
// updateOne / updateMany — write paths
// ---------------------------------------------------------------------------

/// Update one row and retain whether its result contains masked fields.
pub(crate) async fn run_update_one(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: predicate::Input,
    update: Value,
    actor_id: Option<String>,
) -> Result<(Vec<Value>, bool), DbError> {
    let mut update = update;
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    write_pipeline::inspect_update(&schema, &mut update)?;
    // A concurrency predicate must identify one row.
    let concurrency = extract_concurrency_guard(&filter, &coll, &schema)?;
    if !filter.has_non_null_equality("id") {
        if let Some(guard) = &concurrency {
            return Err(DbError::multi_row_concurrency_filter_unsupported(
                &coll,
                &guard.column,
            ));
        }
    }

    update_validation::validate(&schema, &update)?;
    crate::sql::codecs::prepare_update(&schema, &mut update)?;
    let per_row_encrypted_update =
        write_pipeline::update_requires_per_row_encryption(&schema, &update);
    let target_row = if per_row_encrypted_update {
        let target_rows =
            write_pipeline::resolve_target_row_ids(&route, &coll, filter.clone(), 1, &schema)
                .await?;
        let Some(target_row) = target_rows.first().cloned() else {
            if let Some(guard) = &concurrency {
                let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                return Err(DbError::concurrency_mismatch(
                    &coll,
                    row_id,
                    &guard.column,
                    guard.expected,
                ));
            }
            // An absent match has no row to decode or masked value to rehydrate.
            return Ok((Vec::new(), false));
        };
        Some(target_row)
    } else {
        None
    };

    let row_pk = target_row.as_ref().map_or("", |row| row.row_pk.as_str());
    // Key store off the route, for the reason given on [`run_insert`].
    write_pipeline::apply(
        route.backend().key_store(),
        &route,
        &binding,
        &coll,
        &mut update,
        write_pipeline::ApplyMode::Update { row_pk },
    )
    .await?;
    let sql_filter: predicate::Input = if let Some(target_row) = target_row {
        let mut sql_filter = crate::value!({ "id": target_row.id_value });
        if let Some(guard) = &concurrency {
            sql_filter[&guard.column] = Value::from(guard.expected);
        }
        sql_filter.into()
    } else {
        filter.clone()
    };
    // Compile descriptor-declared write assignments after supplied values for
    // those fields have been removed.
    let autobump = AssignmentPlan::from_schema(&schema)?.write_assignments(
        &schema,
        actor_id.as_deref(),
        false,
        false,
    );
    let built = update::build_one(
        binding.schema(),
        &coll,
        &schema,
        sql_filter,
        update,
        &autobump,
        route.sql_registration(),
    );
    let bq = built.map_err(DbError::from)?;
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_data_orm::cdc::ChangeOp::Update,
        &binding,
    )
    .await?;
    let result = read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await?;
    // An empty result with a concurrency predicate is a CAS failure.
    if let Some(guard) = &concurrency {
        if result.rows.is_empty() {
            let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
            return Err(DbError::concurrency_mismatch(
                &coll,
                row_id,
                &guard.column,
                guard.expected,
            ));
        }
        // The required `id` primary key bounds this update to one row.
        if result.rows.len() > 1 {
            tracing::error!(
                collection = %coll,
                row_count = result.rows.len(),
                "concurrency_mismatch_unexpected_multi_row: CAS update returned >1 row"
            );
            return Err(DbError::internal(
                "concurrency_mismatch_unexpected_multi_row",
            ));
        }
    }
    Ok((result.rows, result.has_masked))
}

/// Update matching rows and return the database's affected-row count.
pub(crate) async fn run_update_many(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: predicate::Input,
    update: Value,
    actor_id: Option<String>,
) -> Result<u64, DbError> {
    let mut update = update;
    // Read off the route ONCE, here, because the per-row arm below MOVES the
    // route into `AtomicWriteFrame::begin` - the frame's route carries the same
    // stamp, so this is the same value either arm would read, taken before the
    // move rather than through two different accessors.
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    write_pipeline::inspect_update(&schema, &mut update)?;
    let concurrency = extract_concurrency_guard(&filter, &coll, &schema)?;
    if !filter.has_non_null_equality("id") {
        if let Some(guard) = &concurrency {
            return Err(DbError::multi_row_concurrency_filter_unsupported(
                &coll,
                &guard.column,
            ));
        }
    }

    update_validation::validate(&schema, &update)?;
    crate::sql::codecs::prepare_update(&schema, &mut update)?;
    let per_row_encrypted_update =
        write_pipeline::update_requires_per_row_encryption(&schema, &update);
    // No `skip_*` knob is set: the pass stripped every column the
    // charter re-assigns on write, so the patch cannot carry a
    // competing assignment for the builder to defer to.
    let autobump = AssignmentPlan::from_schema(&schema)?.write_assignments(
        &schema,
        actor_id.as_deref(),
        false,
        false,
    );
    if per_row_encrypted_update {
        let frame = crate::transaction::AtomicWriteFrame::begin(route).await?;
        let work_result: Result<u64, DbError> = async {
            let target_rows = write_pipeline::resolve_target_row_ids(
                frame.route(),
                &coll,
                filter.clone(),
                i64::try_from(crate::budgets::MAX_PER_ROW_UPDATE_TARGETS)
                    .expect("target budget must fit i64")
                    + 1,
                &schema,
            )
            .await?;
            let target_limit = crate::budgets::MAX_PER_ROW_UPDATE_TARGETS;
            if target_rows.len() > target_limit {
                return Err(DbError::validation_hinted(
                    "update_many_target_limit_exceeded",
                    format!(
                        "updateMany matched more than {} rows; the maximum is {}",
                        target_limit, target_limit
                    ),
                    format!(
                        "Narrow the updateMany filter so one call targets at most {} rows.",
                        target_limit
                    ),
                ));
            }
            if target_rows.is_empty() {
                if let Some(guard) = &concurrency {
                    let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                    return Err(DbError::concurrency_mismatch(
                        &coll,
                        row_id,
                        &guard.column,
                        guard.expected,
                    ));
                }
                return Ok(0);
            }

            let target_count = target_rows.len();
            let mut row_queries = Vec::with_capacity(target_count);
            for target_row in &target_rows {
                let row_pk = target_row.row_pk.clone();
                let row_id = target_row.id_value.clone();
                let mut row_update = update.clone();
                // The route moved into the frame, so the key store comes off
                // the frame's route - the same connection every statement in
                // this atomic write runs on.
                write_pipeline::apply(
                    frame.route().backend().key_store(),
                    frame.route(),
                    &binding,
                    &coll,
                    &mut row_update,
                    write_pipeline::ApplyMode::Update { row_pk: &row_pk },
                )
                .await?;
                let mut row_filter = crate::value!({ "id": row_id });
                if let Some(guard) = &concurrency {
                    row_filter[&guard.column] = Value::from(guard.expected);
                }
                // The probe resolved this row by its declared primary key, so
                // the per-row statement does not need a second bounded
                // subquery. Using the many builder here preserves the
                // ordinary column-grant surface while the primary key still
                // bounds the statement to this exact row.
                row_queries.push(
                    update::build_many(
                        binding.schema(),
                        &coll,
                        &schema,
                        row_filter.into(),
                        row_update,
                        &autobump,
                        frame.route().sql_registration(),
                    )
                    .map_err(DbError::from)?,
                );
            }

            let mut affected = 0u64;
            for built in row_queries {
                affected += exec_mutation_count_with_emit(
                    built,
                    frame.route(),
                    &coll,
                    zeroship_data_orm::cdc::ChangeOp::Update,
                )
                .await?;
            }

            if let Some(guard) = &concurrency {
                if affected != target_count as u64 {
                    let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                    return Err(DbError::concurrency_mismatch(
                        &coll,
                        row_id,
                        &guard.column,
                        guard.expected,
                    ));
                }
            }
            Ok(affected)
        }
        .await;
        // Settle the entire write even when a per-row operation fails.
        return frame.finish(work_result).await;
    }

    // The non-per-row arm never moved the route into a frame, so it is still
    // here to supply the key store.
    write_pipeline::apply(
        route.backend().key_store(),
        &route,
        &binding,
        &coll,
        &mut update,
        write_pipeline::ApplyMode::Update { row_pk: "" },
    )
    .await?;
    let bq = update::build_many(
        binding.schema(),
        &coll,
        &schema,
        filter.clone(),
        update,
        &autobump,
        route.sql_registration(),
    )
    .map_err(DbError::from)?;
    let affected =
        exec_mutation_count_with_emit(bq, &route, &coll, zeroship_data_orm::cdc::ChangeOp::Update)
            .await?;
    // A primary-key CAS miss has the same error contract as updateOne.
    if let Some(guard) = &concurrency {
        if affected == 0 {
            let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
            return Err(DbError::concurrency_mismatch(
                &coll,
                row_id,
                &guard.column,
                guard.expected,
            ));
        }
    }
    Ok(affected)
}

fn extract_concurrency_guard(
    filter: &predicate::Input,
    collection: &str,
    schema: &Value,
) -> Result<Option<assignment_pass::ConcurrencyGuard>, DbError> {
    if let Some(filter) = filter.dynamic() {
        return assignment_pass::extract_concurrency_guard(filter, collection, schema);
    }
    let Some(column) = concurrency_column(schema)? else {
        return Ok(None);
    };
    Ok(filter
        .conjunctive_value(column)
        .and_then(Value::as_i64)
        .map(|expected| assignment_pass::ConcurrencyGuard {
            column: column.to_owned(),
            expected,
        }))
}

/// Delete one row, applying soft-delete assignments when the descriptor enables them.
pub fn plan_delete_one(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    plan_delete_one_input(binding, route, collection, filter.into(), actor_id)
}

pub(crate) fn plan_delete_one_input(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: predicate::Input,
    actor_id: Option<&str>,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    // Resolve-then-build, folded into the one `Result` `run_op` already
    // rejects on: an undeclared collection cannot be soft-deleted through a
    // filter this deploy has no schema to lower.
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let autobump =
            AssignmentPlan::from_schema(&schema)?.write_assignments(&schema, actor_id, true, false);
        let builder = delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        );
        if soft_delete_column(&schema)?.is_none() {
            return builder
                .hard(filter, delete::Cardinality::One)
                .map_err(DbError::from);
        }
        builder
            .lifecycle(
                filter,
                soft_delete_column(&schema)?.expect("soft-delete column was resolved"),
                false,
                &autobump,
                delete::Cardinality::One,
            )
            .map_err(DbError::from)
    })
}

/// Compile a bulk soft delete, or a hard delete when no lifecycle marker exists.
pub fn plan_delete_many(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let autobump =
            AssignmentPlan::from_schema(&schema)?.write_assignments(&schema, actor_id, true, false);
        let builder = delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        );
        if soft_delete_column(&schema)?.is_none() {
            return builder
                .hard(filter.into(), delete::Cardinality::Many)
                .map_err(DbError::from);
        }
        builder
            .lifecycle(
                filter.into(),
                soft_delete_column(&schema)?.expect("soft-delete column was resolved"),
                false,
                &autobump,
                delete::Cardinality::Many,
            )
            .map_err(DbError::from)
    })
}

/// Permanently remove a matching row regardless of its deletion marker.
pub fn plan_purge_one(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        )
        .hard(filter.into(), delete::Cardinality::One)
        .map_err(DbError::from)
    })
}

/// Compile a bulk hard delete.
pub fn plan_purge_many(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        )
        .hard(filter.into(), delete::Cardinality::Many)
        .map_err(DbError::from)
    })
}

/// Compile restoration of one soft-deleted row and its declared assignments.
pub fn plan_restore_one(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let autobump =
            AssignmentPlan::from_schema(&schema)?.write_assignments(&schema, actor_id, false, true);
        let marker = soft_delete_column(&schema)?.ok_or_else(|| {
            DbError::validation(
                "restore_not_supported",
                "collection has no soft-delete column",
            )
        })?;
        delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        )
        .lifecycle(
            filter.into(),
            marker,
            true,
            &autobump,
            delete::Cardinality::One,
        )
        .map_err(DbError::from)
    })
}

/// Compile restoration of matching soft-deleted rows and declared assignments.
pub fn plan_restore_many(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let autobump =
            AssignmentPlan::from_schema(&schema)?.write_assignments(&schema, actor_id, false, true);
        let marker = soft_delete_column(&schema)?.ok_or_else(|| {
            DbError::validation(
                "restore_not_supported",
                "collection has no soft-delete column",
            )
        })?;
        delete::Builder::new(
            binding.schema(),
            collection,
            &schema,
            route.sql_registration(),
        )
        .lifecycle(
            filter.into(),
            marker,
            true,
            &autobump,
            delete::Cardinality::Many,
        )
        .map_err(DbError::from)
    })
}

// ---------------------------------------------------------------------------
// aggregate / distinct / count — read paths
// ---------------------------------------------------------------------------

/// Compile an aggregate and return its result-column layout with the query.
pub(crate) fn plan_aggregate(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    pipeline: &Value,
    opts: &Value,
) -> Result<
    (
        crate::sql::compiler::CompiledQuery,
        Option<aggregate::AggregateProjection>,
    ),
    DbError,
> {
    validate_dynamic_options(opts, "aggregate")?;
    let include_deleted = parse_include_deleted(opts)?;

    // Record into the active query's read-set so the broker can
    // narrow events. If the first stage is `$match`, capture its filter;
    // otherwise record a coarse-grained entry (empty filter) — the
    // pipeline depends on the whole collection.
    {
        let captured_filter = pipeline
            .as_array()
            .and_then(|stages| stages.first())
            .and_then(|stage| stage.get("$match"))
            .cloned()
            .unwrap_or_else(|| Value::Object(crate::value::Map::new()));
        record_read_set(binding, collection, &captured_filter);
    }

    let filter_soft_deleted = assignment_pass::should_filter_soft_deleted(include_deleted);

    // The descriptor entry is the aggregate builder's identifier allowlist.
    // `$group.by` / `$sum` / `$sort` on a masked column read the field's own
    // column, which holds the mask - there is no sibling to lower to any more.
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        aggregate::build(
            binding.schema(),
            collection,
            pipeline,
            filter_soft_deleted,
            &schema,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

/// Prepare a distinct query using the descriptor's visible value column.
/// The result metadata identifies masked values for the caller's read pipeline.
pub fn plan_distinct(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    field: &str,
    filter: Value,
    opts: &Value,
) -> Result<(crate::sql::compiler::CompiledQuery, bool), DbError> {
    validate_dynamic_options(opts, "distinct")?;
    let include_deleted = parse_include_deleted(opts)?;
    let filter_soft_deleted = assignment_pass::should_filter_soft_deleted(include_deleted);

    // A DISTINCT over a masked column returns MASKS - the column with the
    // field's own name is the one it selects, and that column holds the mask.
    // So the read pipeline's decrypt stage has nothing to do for it, and would
    // be handed a mask string where it expects native ciphertext. Derived from the same
    // descriptor entry the builder uses; an undeclared collection rejects
    // before either.
    let schema_hint = crate::descriptor::collection_schema(binding, collection)?;
    let distinct_reads_masked_value = mapping::column_is_masked(field, &schema_hint);

    let built = read::distinct(
        binding.schema(),
        collection,
        &schema_hint,
        field,
        filter,
        filter_soft_deleted,
        route.sql_registration(),
    )
    .map_err(DbError::from)?;

    Ok((built, distinct_reads_masked_value))
}

/// Prepare a bounded count query and record its read dependency synchronously.
pub fn plan_count(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
    opts: &Value,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    validate_dynamic_options(opts, "count")?;
    let include_deleted = parse_include_deleted(opts)?;

    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, &filter);

    let filter_soft_deleted = assignment_pass::should_filter_soft_deleted(include_deleted);

    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        read::count(
            binding.schema(),
            collection,
            &schema,
            filter,
            filter_soft_deleted,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

// ---------------------------------------------------------------------------
// upsert — INSERT … ON CONFLICT path
// ---------------------------------------------------------------------------

/// Execute an upsert using the captured route and actor.
pub async fn run_upsert(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    doc: Value,
    conflict_fields: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    let guard_identity = write_pipeline::upsert_requires_conflict_probe(&schema, &doc);
    route
        .sql_registration()
        .check(&upsert::requirements(&schema, guard_identity))
        .map_err(mapping::QueryError::from)?;
    let frame;
    let route = if guard_identity {
        frame = Some(crate::transaction::AtomicWriteFrame::begin(route).await?);
        frame.as_ref().expect("opened write frame").route()
    } else {
        frame = None;
        &route
    };
    let result = async {
        let mut retry = guard_identity.then(|| doc.clone());
        let mut doc = doc;
        let assignments = AssignmentPlan::from_schema(&schema)?.write_assignments(
            &schema,
            actor_id.as_deref(),
            false,
            false,
        );
        loop {
            prepare_upsert_doc_for_write(
                &mut doc,
                &binding,
                route,
                &coll,
                actor_id.as_deref(),
                &conflict_fields,
            )
            .await?;
            let expected_id =
                if guard_identity {
                    Some(doc.get("id").cloned().ok_or_else(|| {
                        DbError::internal("encrypted upsert requires an identity")
                    })?)
                } else {
                    None
                };
            let bq = upsert::Builder::new(
                binding.schema(),
                &coll,
                &schema,
                &assignments,
                route.sql_registration(),
            )
            .build(std::mem::take(&mut doc), &conflict_fields, expected_id)
            .map_err(DbError::from)?;
            let rows = exec_mutation_with_emit(
                bq,
                route,
                &coll,
                zeroship_data_orm::cdc::ChangeOp::Update,
                &binding,
            )
            .await?;
            if guard_identity && rows.is_empty() {
                // The rejected conflict update holds the winning row's lock.
                // Resolve its identity and encrypt the original input again.
                doc = retry.take().ok_or_else(|| {
                    DbError::validation(
                        "upsert_identity_changed",
                        "upsert could not establish the conflicting row identity",
                    )
                })?;
                continue;
            }
            return read_pipeline::apply(
                route,
                &binding,
                &coll,
                rows,
                read_pipeline::ApplyOptions::default(),
            )
            .await;
        }
    }
    .await;
    match frame {
        Some(frame) => frame.finish(result).await,
        None => result,
    }
}

// ---------------------------------------------------------------------------
// search - vector entry point
// ---------------------------------------------------------------------------

/// The eagerly-decoded inputs of a vector `search`, produced by [`plan_search`]
/// and consumed by [`run_search`].
#[derive(Debug)]
pub struct SearchPlan {
    query: crate::sql::compiler::CompiledQuery,
}

/// Decode search arguments and lower filters using the deployment descriptor
/// before asynchronous execution.
pub fn plan_search(
    binding: &DbBinding,
    registration: &crate::sql::registration::SqlRegistration,
    collection: &str,
    args: &Value,
) -> Result<SearchPlan, DbError> {
    // Presence of `vector` selects the pgvector path.
    let Some(raw_vector) = args.get("vector") else {
        // Use `Configuration` because the failure is shape-level, not
        // data-level.
        return Err(DbError::Configuration {
            code: "invalid_search_args",
            message: "search: args must include `vector`".to_string(),
            hint: Some(
                "pass `{ vector: number[], k?: number, metric?, column?, filter? }` for vector search"
                    .to_string(),
            ),
        });
    };

    // Decode `vector` into `Vec<f32>`. Reject anything that's not a
    // homogeneous number array at the boundary so the impl can stay
    // typed.
    let Some(arr) = raw_vector.as_array() else {
        return Err(DbError::Configuration {
            code: "invalid_vector_arg",
            message: "search: `vector` must be an array of numbers".to_string(),
            hint: None,
        });
    };
    let mut vector: Vec<f32> = Vec::with_capacity(arr.len());
    for elem in arr {
        let Some(n) = elem.as_f64() else {
            return Err(DbError::Configuration {
                code: "invalid_vector_arg",
                message: "search: every element of `vector` must be a number".to_string(),
                hint: None,
            });
        };
        vector.push(n as f32);
    }

    let k = match args.get("k") {
        None => 10,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                DbError::config("invalid_search_args", "search: `k` must be an integer")
            })?,
    };
    let metric = match args.get("metric") {
        None => crate::backend::VectorMetric::Cosine,
        Some(value) => match value.as_str() {
            Some("cosine") => crate::backend::VectorMetric::Cosine,
            Some("l2") => crate::backend::VectorMetric::L2,
            Some("innerProduct") => crate::backend::VectorMetric::InnerProduct,
            _ => {
                return Err(DbError::config(
                    "invalid_search_args",
                    "search: `metric` must be cosine, l2, or innerProduct",
                ));
            }
        },
    };
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    let column = match args.get("column") {
        None => {
            let readable = crate::sql::descriptors::readable_fields(&schema);
            let mut vectors = schema
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(name, definition)| {
                    readable.contains(name.as_str())
                        && definition.get("type").and_then(Value::as_str) == Some("vector")
                })
                .map(|(name, _)| name.as_str());
            let field = vectors.next().filter(|_| vectors.next().is_none()).ok_or_else(|| {
                DbError::config(
                    "invalid_search_args",
                    "search: `column` is required unless the collection has one readable vector field",
                )
            })?;
            field.to_owned()
        }
        Some(value) => value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                DbError::config(
                    "invalid_search_args",
                    "search: `column` must be a non-empty string",
                )
            })?,
    };
    let filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(crate::value::Map::new()));
    let query = search::vector(
        binding.schema(),
        collection,
        &schema,
        &column,
        vector,
        k,
        metric,
        filter,
        registration,
    )?;
    Ok(SearchPlan { query })
}

/// Run vector search on the captured transaction route and process the result.
pub async fn run_search(
    route: &crate::tx_route::TxRoute,
    binding: DbBinding,
    coll: String,
    plan: SearchPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = crate::backend_handle::routed_vector_search(route, &binding, plan.query).await?;

    // Count the successful database read even if later result decoding fails.
    crate::metrics::emit_db_metric(route.meter(), crate::metrics::DB_READS, 1);

    read_pipeline::apply(
        route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}

/// The eagerly-decoded inputs of a spatial `near`, produced by [`plan_near`] and
/// consumed by [`run_near`].
#[derive(Debug)]
pub struct NearPlan {
    field: String,
    point: crate::backend::GeoPoint,
    radius_m: f64,
    limit: usize,
    query: crate::sql::compiler::CompiledQuery,
}

/// Validate spatial arguments and compile the broad database query.
pub fn plan_near(
    binding: &DbBinding,
    registration: &crate::sql::registration::SqlRegistration,
    collection: &str,
    args: &Value,
) -> Result<NearPlan, DbError> {
    let field = match args.get("field").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Err(DbError::Configuration {
                code: "invalid_near_args",
                message: "near: `field` must be a non-empty string".to_string(),
                hint: Some("pass `{ field, point, radius, filter?, limit? }`".to_string()),
            });
        }
    };

    let point_obj = args.get("point");
    let lat = point_obj.and_then(|p| p.get("lat")).and_then(Value::as_f64);
    let lng = point_obj.and_then(|p| p.get("lng")).and_then(Value::as_f64);
    let (Some(lat), Some(lng)) = (lat, lng) else {
        return Err(DbError::Configuration {
            code: "invalid_near_args",
            message: "near: `point` must be `{ lat: number, lng: number }`".to_string(),
            hint: None,
        });
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(DbError::Configuration {
            code: "invalid_near_args",
            message: format!(
                "near: `point` out of range: lat must be in [-90,90] and lng in [-180,180], got lat={lat} lng={lng}"
            ),
            hint: None,
        });
    }

    let radius_m = match args.get("radius").and_then(Value::as_f64) {
        Some(r) if r > 0.0 && r.is_finite() => r,
        _ => {
            return Err(DbError::Configuration {
                code: "invalid_near_args",
                message: "near: `radius` must be a positive number (metres)".to_string(),
                hint: None,
            });
        }
    };

    let limit = match args.get("limit") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    DbError::config("invalid_near_args", "near: `limit` must be an integer")
                })?,
        ),
    };
    let limit = search::spatial_limit(limit)?;
    let filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(crate::value::Map::new()));

    let schema = crate::descriptor::collection_schema(binding, collection)?;
    let point = crate::backend::GeoPoint { lat, lng };
    let query = search::spatial(
        binding.schema(),
        collection,
        &schema,
        &field,
        point,
        radius_m,
        filter,
        limit,
        registration,
    )?;

    Ok(NearPlan {
        field,
        point,
        radius_m,
        limit,
        query,
    })
}

/// Run geographic search on the captured transaction route and process the result.
pub async fn run_near(
    route: &crate::tx_route::TxRoute,
    binding: DbBinding,
    coll: String,
    plan: NearPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let NearPlan {
        field,
        point,
        radius_m,
        limit,
        query,
    } = plan;

    let rows = crate::backend_handle::routed_spatial_near(
        route, &binding, query, &field, point, radius_m, limit,
    )
    .await?;

    // Count the successful database read even if later result decoding fails.
    crate::metrics::emit_db_metric(route.meter(), crate::metrics::DB_READS, 1);

    read_pipeline::apply(
        route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}

/// Protect insert documents using the bound route.
pub async fn prepare_insert_many_docs_for_binding(
    keys: &crate::encryption::KeyStore,
    route: &TxRoute,
    docs: &mut Value,
    binding: &DbBinding,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    write_pipeline::apply(
        keys,
        route,
        binding,
        collection,
        docs,
        write_pipeline::ApplyMode::InsertMany { actor_id },
    )
    .await
}

/// Test helper that resolves the runtime data-access schema the way the CRUD
/// passes do — through [`crate::descriptor::collection_schema`], the data
/// plane's sole schema authority. Lets a test assert that the metadata the
/// read/write passes will act on is exactly the descriptor entry the deploy
/// installed, and that an undeclared collection is a typed refusal rather than
/// an absent schema.
#[cfg(test)]
pub fn runtime_schema_for_tests(app_id: &str, collection: &str) -> Result<Value, DbError> {
    let binding = DbBinding::cold_start(app_id);
    // Deep-clone out of the shared store: the helper's callers own and mutate
    // their copy, and handing them the isolate's `Arc` would let one test
    // observe another's edit.
    crate::descriptor::collection_schema(&binding, collection).map(|facts| (*facts).clone())
}

async fn prepare_upsert_doc_for_write(
    doc: &mut Value,
    binding: &DbBinding,
    route: &TxRoute,
    collection: &str,
    actor_id: Option<&str>,
    conflict_fields: &Value,
) -> Result<(), DbError> {
    // Use the key store bound to the backend that performs the write.
    write_pipeline::apply(
        route.backend().key_store(),
        route,
        binding,
        collection,
        doc,
        write_pipeline::ApplyMode::Upsert {
            actor_id,
            conflict_fields,
        },
    )
    .await
}

/// Encrypt declared fields with the host-supplied project key store.
/// The shared pass returns native ciphertext and retains mask inputs in a
/// zeroizing sidechannel; it performs no SQL or backend resolution.
async fn encryption_pass_dispatch(
    keys: &crate::encryption::KeyStore,
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    doc: &mut Value,
    sidechannel: &mut mask_pass::MaskPlaintextSidechannel,
) -> Result<(), DbError> {
    crate::protection::encryption_pass::encrypt_row_on_write_with_sidechannel(
        keys,
        app_id,
        collection,
        schema,
        row_pk,
        doc,
        sidechannel,
    )
    .await
}

/// Cheap walk: does any field def on `schema` carry `encrypted`?
fn schema_has_encrypted_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|o| o.values().any(crate::sql::descriptors::is_encrypted))
}

/// Cheap walk: does any field def on `schema` carry a
/// `mask` entry with `kind != "none"`? Drives the per-write mask pass.
fn schema_has_masked_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| {
            o.values().any(|def| {
                def.get("mask")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.get("kind")
                            .and_then(|k| k.as_str())
                            .map(|k| k != "none")
                            .unwrap_or(true) // missing kind defaults to "full" → masked
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
