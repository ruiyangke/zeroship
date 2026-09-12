//! Plan and execute collection operations for Rust callers and the V8 adapter.
//!
//! Synchronous planning captures descriptors, records read dependencies and sanitizes
//! unmask hints before execution can yield. Async execution uses the captured route
//! for ordinary queries and search, then applies result protection and decoding.
//! V8 promise creation and delivery belong to the adapter.

use crate::value::Value;

use crate::assignments::AssignmentPlan;
use crate::exec::{exec_mutation_count_with_emit, exec_mutation_with_emit, exec_query};
use crate::sql::mapping;
use crate::sql::lifecycle::{concurrency_column, soft_delete_column};
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
pub async fn exec_aggregate_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: crate::sql::compiler::CompiledQuery,
    group_fields: Vec<String>,
    result_columns: Option<Vec<String>>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = exec_query(&route, bq).await?;
    read_pipeline::apply(
        &route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            unmask_columns: &[],
            schema_field_scope: if group_fields.is_empty() {
                read_pipeline::SchemaFieldScope::All
            } else {
                read_pipeline::SchemaFieldScope::Only(group_fields.as_slice())
            },
            // A `$group` result's keys are accumulator aliases, which no
            // descriptor declares, so the declared surface would drop
            // every one of them. This is the ONLY call site in the crate
            // that names a surface; every other one takes the default.
            row_surface: match &result_columns {
                Some(cols) => read_pipeline::RowSurface::Projected(cols.as_slice()),
                None => read_pipeline::RowSurface::Declared,
            },
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await
}

/// The ENGINE composition behind `distinct`. Also non-default `ApplyOptions`,
/// and different ones again from [`exec_aggregate_read`]: a DISTINCT over a
/// masked column selects the column holding the MASK, so the decrypt stage has
/// nothing to do and would be handed a mask string where it expects native ciphertext.
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

/// Extract `opts.unmask` into a `Vec<String>`. Returns
/// empty when the field is absent, null, or not an array of strings —
/// malformed `unmask` shapes are tolerated as
/// "no hint" rather than an error so a stale SDK build doesn't bring
/// down the find path.
fn parse_unmask_opt(opt: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(arr)) = opt else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
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
pub fn plan_find(binding: &DbBinding, collection: &str, filter: &Value, opts: &Value) -> FindPlan {
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, filter);

    // DB-2: public `find` normalises an omitted limit here before calling the
    // builder. This does not protect internal builder callers; they must pass
    // their own explicit bound. Callers paginate past this page via `offset`.
    let limit = Some(
        opts.get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(crate::sql::MAX_ROW_LIMIT),
    );
    // DB-3: strip an app-supplied reserved `auto` system actor — a find with
    // `{unmask, actor:{kind:"auto"}}` must not impersonate the platform.
    let unmask_sanitized = crate::protection::unmask::sanitize_app_actor(
        opts.get("actor").cloned().filter(|v| !v.is_null()),
    );

    FindPlan {
        limit,
        offset: opts.get("offset").and_then(Value::as_i64),
        order_by: opts.get("orderBy").cloned(),
        select: opts.get("select").cloned(),
        unmask_columns: parse_unmask_opt(opts.get("unmask")),
        unmask_actor: unmask_sanitized.actor,
        unmask_rejected_claim: unmask_sanitized.rejected_claim,
        unmask_reason: opts
            .get("unmaskReason")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        include_deleted: opts
            .get("include_deleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
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
            route.backend(),
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
            route.backend(),
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

/// Shared dispatch for `insert`. The capability gate is the caller's
/// responsibility — `Collection::insert` calls
/// `refuse_if_query_capability` before reaching here.
/// The ENGINE half of `insert`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// `actor_id` is a PARAMETER rather than something this function looks up, and
/// that is load-bearing. `assignment_pass::current_actor_id` reads
/// `executing_request_id` off the runtime state, which is only guaranteed-set
/// on the pump turn that initiates the dispatch. This function awaits before it
/// writes (the encryption pass's `resolve_key` round-trip), so resolving the
/// actor in here would attribute the row to whichever request happens to be
/// current at first poll. `v8_classes::dispatch::dispatch_insert` reads it eagerly and passes it in.
pub async fn run_insert(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    doc: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    route
        .sql_registration()
        .check(&insert::requirements(&schema))
        .map_err(mapping::QueryError::from)?;
    let frame;
    let route = if identity::requires_allocation(&schema, &doc) {
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

/// Shared dispatch for `insertMany`. See `v8_classes::dispatch::dispatch_insert` for the
/// capability-gate contract.
/// The ENGINE half of `insertMany`. `actor_id` is eager for the reason given on
/// [`run_insert`]; it reaches the docs through
/// `prepare_insert_many_docs_for_binding`, not `write_pipeline::apply`.
pub async fn run_insert_many(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    docs: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    route
        .sql_registration()
        .check(&insert::requirements(&schema))
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

/// The ENGINE half of `updateOne`.
///
/// Returns a `(rows, has_masked)` PAIR rather than the [`read_pipeline::ApplyResult`]
/// the insert halves return. That is not a stylistic difference: the
/// probe-found-nothing arm below returns `(Vec::new(), false)`, a shape no
/// `ApplyResult` produces, and the `false` is load-bearing - see the comment at
/// that return. `actor_id` is eager for the reason given on [`run_insert`].
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
    // Detect creator-supplied CAS version + reject
    // the unsupported "version filter without id" shape eagerly.
    let cas_version = extract_cas_version(&filter, &coll, &schema)?;
    if cas_version.is_some() && !filter.has_non_null_equality("id") {
        return Err(DbError::multi_row_version_filter_unsupported(&coll));
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
            if let Some(expected_version) = cas_version {
                let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                return Err(DbError::version_mismatch(&coll, row_id, expected_version));
            }
            // An absent match has no row to decode or masked value to rehydrate.
            return Ok((Vec::new(), false));
        };
        Some(target_row)
    } else {
        None
    };

    let mut update = update;
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
        if let Some(expected_version) = cas_version {
            sql_filter[concurrency_column(&schema)?.expect("CAS column")] =
                Value::from(expected_version);
        }
        sql_filter.into()
    } else {
        filter.clone()
    };
    // Compile the descriptor's write assignments.
    // Actor flows into the `updated_by` bind; the `hints` from the
    // pre-pass tell the builder which auto-bumps to suppress.
    // No `skip_*` knob is set: the pass stripped every column the
    // charter re-assigns on write, so the patch cannot carry a
    // competing assignment for the builder to defer to.
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
    // Optimistic-concurrency check. When the
    // creator supplied a `version: N` predicate AND the
    // RETURNING set is empty, classify as a CAS failure
    // (the row exists at a different version, or the row
    // is missing — the SDK consumer retries either way).
    if let Some(expected_version) = cas_version {
        if result.rows.is_empty() {
            let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
            return Err(DbError::version_mismatch(&coll, row_id, expected_version));
        }
        // The `id` PK ensures at most one row matches
        // `{ id: ..., version: N }`; a result set >1 is
        // a regression in the dispatcher contract.
        if result.rows.len() > 1 {
            tracing::error!(
                collection = %coll,
                row_count = result.rows.len(),
                "version_mismatch_unexpected_multi_row: CAS update returned >1 row"
            );
            return Err(DbError::internal("version_mismatch_unexpected_multi_row"));
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
    let cas_version = extract_cas_version(&filter, &coll, &schema)?;
    if cas_version.is_some() && !filter.has_non_null_equality("id") {
        return Err(DbError::multi_row_version_filter_unsupported(&coll));
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
                if let Some(expected_version) = cas_version {
                    let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                    return Err(DbError::version_mismatch(&coll, row_id, expected_version));
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
                if let Some(expected_version) = cas_version {
                    row_filter[concurrency_column(&schema)?.expect("CAS column")] =
                        Value::from(expected_version);
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

            if let Some(expected_version) = cas_version {
                if affected != target_count as u64 {
                    let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
                    return Err(DbError::version_mismatch(&coll, row_id, expected_version));
                }
            }
            Ok(affected)
        }
        .await;
        // Settle the entire write even when a per-row operation fails.
        return frame.finish(work_result).await;
    }

    let mut update = update;
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
    if let Some(expected_version) = cas_version {
        if affected == 0 {
            let row_id = filter.conjunctive_value("id").and_then(Value::as_str);
            return Err(DbError::version_mismatch(&coll, row_id, expected_version));
        }
    }
    Ok(affected)
}

fn extract_cas_version(
    filter: &predicate::Input,
    collection: &str,
    schema: &Value,
) -> Result<Option<i64>, DbError> {
    if let Some(filter) = filter.dynamic() {
        return assignment_pass::extract_cas_version(filter, collection, schema);
    }
    let Some(column) = concurrency_column(schema)? else {
        return Ok(None);
    };
    Ok(filter.conjunctive_value(column).and_then(Value::as_i64))
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
        if soft_delete_column(&schema)?.is_none() {
            return delete::build_hard(
                binding.schema(),
                collection,
                &schema,
                filter,
                true,
                route.sql_registration(),
            )
            .map_err(DbError::from);
        }
        delete::build_lifecycle(
            binding.schema(),
            collection,
            &schema,
            filter,
            soft_delete_column(&schema)?.expect("soft-delete column was resolved"),
            false,
            &autobump,
            true,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

/// Shared dispatch for `deleteMany`. Resolves with the count of
/// affected rows as a JS `number`.
/// The ENGINE half of `delete_many`. Identical in shape to [`plan_delete_one`];
/// only the builder differs.
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
        if soft_delete_column(&schema)?.is_none() {
            return delete::build_hard(
                binding.schema(),
                collection,
                &schema,
                filter.into(),
                false,
                route.sql_registration(),
            )
            .map_err(DbError::from);
        }
        delete::build_lifecycle(
            binding.schema(),
            collection,
            &schema,
            filter.into(),
            soft_delete_column(&schema)?.expect("soft-delete column was resolved"),
            false,
            &autobump,
            false,
            route.sql_registration(),
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
        delete::build_hard(
            binding.schema(),
            collection,
            &schema,
            filter.into(),
            true,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

/// Bulk-purge entry point.
/// The ENGINE half of `purge_many`. Peer of [`plan_purge_one`]: a hard delete,
/// so no `actor_id`.
pub fn plan_purge_many(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    filter: Value,
) -> Result<crate::sql::compiler::CompiledQuery, DbError> {
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        delete::build_hard(
            binding.schema(),
            collection,
            &schema,
            filter.into(),
            false,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

/// Restore a soft-deleted row.
/// The ENGINE half of `restore_one`.
///
/// NOT a copy of [`plan_delete_one`]: the autobump here also sets
/// `dispatch_write: true`. Templating this family from a sibling would drop that
/// flag silently, so each plan is transcribed from its own dispatch.
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
        delete::build_lifecycle(
            binding.schema(),
            collection,
            &schema,
            filter.into(),
            marker,
            true,
            &autobump,
            true,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

/// Bulk-restore entry point.
/// The ENGINE half of `restore_many`. Like [`plan_restore_one`], the autobump
/// sets `dispatch_write: true`; only the builder differs.
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
        delete::build_lifecycle(
            binding.schema(),
            collection,
            &schema,
            filter.into(),
            marker,
            true,
            &autobump,
            false,
            route.sql_registration(),
        )
        .map_err(DbError::from)
    })
}

// ---------------------------------------------------------------------------
// aggregate / distinct / count — read paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `aggregate`. Pipeline is a JSON array of stage
/// objects.
///
/// `opts.include_deleted: true` opts out of the auto
/// soft-delete `$match` (per Q-SF-J -- every read-side
/// method auto-filters for consistency).
/// The ENGINE half of `aggregate`.
///
/// Returns a PAIR, unlike the seven single-`CompiledQuery` plans in this file:
/// `build_aggregate_with_result_columns` yields the result-column list alongside
/// the query, and the adapter needs it to shape the response. `distinct` is the
/// other pair-returning member of this group.
///
/// `aggregate_group_fields(&pipeline)` deliberately stays on the adapter side for
/// now. It is pipeline analysis and belongs here, but moving it would make this a
/// three-value return; it travels with the outstanding second cut instead.
pub fn plan_aggregate(
    binding: &DbBinding,
    route: &crate::tx_route::CapturedRoute,
    collection: &str,
    pipeline: &Value,
    opts: &Value,
) -> Result<(crate::sql::compiler::CompiledQuery, Option<Vec<String>>), DbError> {
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

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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
    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, &filter);

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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

/// The ENGINE half of `upsert`. `actor_id` is eager for the reason given on
/// [`run_insert`]; `route` is borrowed twice here, so it is taken by value.
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
            let bq = upsert::build_upsert_with_registration(
                binding.schema(),
                &coll,
                &schema,
                std::mem::take(&mut doc),
                &conflict_fields,
                &assignments,
                expected_id,
                route.sql_registration(),
            )
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

/// Shared dispatch for `collection.search(args)`.
///
/// - `{ vector, k?, metric?, column?, filter? }` ->
///   [`crate::backend_handle::routed_vector_search`], routed to pgvector on PG
///   or the pure-Rust flat-scan implementation on SQLite, on the lane this
///   dispatch belongs to.
///
/// Resolves with a JSON array of rows; each row carries the
/// `_distance` synthetic column from pgvector. Errors are coded
/// (`vector_extension_missing` / `vector_unsupported` / standard
/// SQLSTATE) so the SDK can branch on `e.code`.
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

    let k = args
        .get("k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10);
    let metric_str = args
        .get("metric")
        .and_then(Value::as_str)
        .unwrap_or("cosine");
    let metric = match metric_str {
        "l2" => crate::backend::VectorMetric::L2,
        "innerProduct" | "ip" => crate::backend::VectorMetric::InnerProduct,
        _ => crate::backend::VectorMetric::Cosine,
    };
    let column = args
        .get("column")
        .and_then(Value::as_str)
        .unwrap_or("embedding")
        .to_string();
    let filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(crate::value::Map::new()));
    let schema = crate::descriptor::collection_schema(binding, collection)?;
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
    crate::metrics::emit_db_metric(binding.app_id(), crate::metrics::DB_READS, 1);

    read_pipeline::apply(
        route,
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}

/// Shared dispatch for the `Collection.near()` v8_method.
///
/// `args` shape (validated SDK-side):
/// ```js
/// { field: "location",
///   point: { lat: 51.5, lng: -0.1 },
///   radius: 1000,           // metres
///   filter?: {...},
///   limit?: 100 }
/// ```
///
/// Routes to [`crate::backend_handle::routed_spatial_near`], dispatching to
/// PG's `geography(POINT, 4326)` support or SQLite's pure-Rust haversine
/// flat-scan implementation, on the lane this dispatch belongs to. Each
/// returned row carries a synthetic `_distance_m` (`f64`) column.
/// The eagerly-decoded inputs of a spatial `near`, produced by [`plan_near`] and
/// consumed by [`run_near`].
#[derive(Debug)]
pub struct NearPlan {
    field: String,
    point: crate::backend::GeoPoint,
    radius_m: f64,
    limit: Option<usize>,
    query: crate::sql::compiler::CompiledQuery,
}

/// The EAGER half of `near`. Same rejection-folding as [`plan_search`]: the five
/// eagerly-spawned rejections are plain `Err`s, and `settle`'s error arm makes
/// the same `reject_op` call they did.
///
/// All four argument refusals share the `invalid_near_args` code; only the
/// message distinguishes them. That is transcribed from the original, not
/// tidied - the SDK branches on the code.
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

    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
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
    crate::metrics::emit_db_metric(binding.app_id(), crate::metrics::DB_READS, 1);

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
