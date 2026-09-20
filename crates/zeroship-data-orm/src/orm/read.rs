//! Shared relational reads and provenance-preserving result materialization.

use super::*;
use crate::schema::{ColumnSchema, LogicalType};
use crate::sql::statement::{
    ResolvedJoin, ResolvedOperand, ResolvedOrder, ResolvedPredicate, ResolvedPredicateValue,
    SelectParts, SelectStatement, SelectedExpression, Statement,
};
use crate::sql::{
    FieldPath, Ident, IdentRole, JoinKind, Operand, OrderKey, Predicate, RowLimit, RowOffset,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const MAX_READ_FIELDS: usize = 512;
pub const MAX_READ_RESULT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ReadSource {
    pub collection: String,
    pub alias: String,
    pub include_deleted: bool,
}
impl ReadSource {
    pub fn new(collection: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            collection: collection.into(),
            alias: alias.into(),
            include_deleted: false,
        }
    }
    pub fn column(&self, field: &str) -> Result<FieldPath, DbError> {
        Ok(FieldPath::column(ident(field, IdentRole::Column)?)
            .in_source(ident(&self.alias, IdentRole::Alias)?))
    }
}
#[derive(Debug, Clone)]
pub struct ReadJoin {
    pub kind: JoinKind,
    pub source: ReadSource,
    pub on: Predicate,
}

#[derive(Debug, Clone)]
pub enum ReadProjection {
    /// Project whether a row matched without reading its fields.
    Presence {
        output: String,
    },
    Row {
        output: String,
        source: String,
        fields: Option<Vec<String>>,
        optional: bool,
    },
    Scalar {
        output: String,
        expression: Operand,
    },
}

#[derive(Debug, Clone)]
pub struct ReadQuery {
    pub source: ReadSource,
    pub joins: Vec<ReadJoin>,
    pub projection: Vec<ReadProjection>,
    pub filter: Predicate,
    pub group_by: Vec<FieldPath>,
    pub having: Predicate,
    pub order_by: Vec<OrderKey>,
    pub limit: RowLimit,
    pub offset: RowOffset,
    pub(crate) summary: Option<crate::sql::statement::SelectSummary>,
    pub(crate) model_filter: Option<super::model::ModelPredicate>,
    pub(crate) lock: ReadLock,
}
impl ReadQuery {
    pub fn new(source: ReadSource) -> Self {
        Self {
            source,
            joins: Vec::new(),
            projection: Vec::new(),
            filter: Predicate::always(),
            group_by: Vec::new(),
            having: Predicate::always(),
            order_by: Vec::new(),
            limit: RowLimit::default(),
            offset: RowOffset::default(),
            model_filter: None,
            summary: None,
            lock: ReadLock::None,
        }
    }
}

/// Row locks requested through the typed Rust builders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum ReadLock {
    #[default]
    None,
    /// Exclusive locks on the rows of every source, or of the named aliases.
    Update { of: Vec<String> },
}

pub(super) fn invalid(message: impl Into<String>) -> DbError {
    DbError::validation("invalid_read", message)
}
pub(super) fn ident(name: &str, role: IdentRole) -> Result<Ident, DbError> {
    Ident::parse_as(name, role).map_err(|e| invalid(e.to_string()))
}

#[derive(Debug)]
struct SourceLayout {
    source: ReadSource,
    schema: Arc<FieldMap>,
    nullable: bool,
    fields: BTreeMap<String, String>,
    resolved: crate::crud::resolved::ResolvedTable,
}
#[derive(Debug)]
pub(super) struct PreparedRead {
    query: CompiledQuery,
    sources: Vec<SourceLayout>,
    projections: Vec<ReadProjection>,
    scalar_slots: BTreeMap<String, String>,
    scalar_schema: FieldMap,
    summary: bool,
}

impl PreparedRead {
    /// `in_transaction` is the captured route's decision; row locks need it.
    pub(super) fn new(
        binding: &DbBinding,
        registration: &crate::sql::registration::SqlRegistration,
        in_transaction: bool,
        input: ReadQuery,
    ) -> Result<Self, DbError> {
        if input.joins.len() >= crate::sql::MAX_READ_SOURCES {
            return Err(invalid("read exceeds its source budget"));
        }
        if input.projection.is_empty() || input.projection.len() > MAX_READ_FIELDS {
            return Err(invalid("read projection exceeds its field budget"));
        }
        let mut sources = Vec::new();
        for (source, nullable) in std::iter::once((&input.source, false)).chain(
            input
                .joins
                .iter()
                .map(|j| (&j.source, j.kind == JoinKind::Left)),
        ) {
            ident(&source.collection, IdentRole::Collection)?;
            ident(&source.alias, IdentRole::Alias)?;
            if sources
                .iter()
                .any(|s: &SourceLayout| s.source.alias == source.alias)
            {
                return Err(invalid("duplicate read source alias"));
            }
            let schema = crate::descriptor::collection_schema(binding, &source.collection)?;
            let resolved = crate::crud::resolved::ResolvedTable::aliased(
                binding.schema(),
                &source.collection,
                &source.alias,
                &schema,
                registration,
            )?;
            sources.push(SourceLayout {
                source: source.clone(),
                nullable,
                schema,
                fields: BTreeMap::new(),
                resolved,
            });
        }
        let mut joins = Vec::new();
        for join in &input.joins {
            validate_predicate(&join.on, &sources, true)?;
            let mut on = join.on.clone();
            if !join.source.include_deleted {
                on = Predicate::And(vec![
                    on,
                    visible(
                        &join.source,
                        &sources
                            .iter()
                            .find(|s| s.source.alias == join.source.alias)
                            .unwrap()
                            .schema,
                    )?,
                ]);
            }
            joins.push(ResolvedJoin {
                kind: join.kind,
                table: sources
                    .iter()
                    .find(|source| source.source.alias == join.source.alias)
                    .expect("validated join source")
                    .resolved
                    .table
                    .clone(),
                on: resolve_predicate(&on, &sources, registration)?,
            });
        }
        validate_predicate(&input.filter, &sources, false)?;
        validate_predicate(&input.having, &sources, false)?;
        for path in &input.group_by {
            check_field(path, &sources, false)?;
            check_capability(path, &sources, ReadCapability::Filterable)?;
            check_grouping(path, &sources)?;
        }
        for key in &input.order_by {
            check_field(&key.path, &sources, false)?;
            check_capability(&key.path, &sources, ReadCapability::Sortable)?;
            check_sorting(&key.path, &sources)?;
        }

        let aggregating = input.projection.iter().any(|p| {
            matches!(
                p,
                ReadProjection::Scalar {
                    expression: Operand::Aggregate(_),
                    ..
                }
            )
        });
        let mut output_names = BTreeSet::new();
        let mut projected = Vec::new();
        let mut scalar_slots = BTreeMap::new();
        let mut scalar_schema = FieldMap::new();
        for projection in &input.projection {
            let output = match projection {
                ReadProjection::Presence { output }
                | ReadProjection::Row { output, .. }
                | ReadProjection::Scalar { output, .. } => output,
            };
            ident(output, IdentRole::Alias)?;
            if !output_names.insert(output.clone()) {
                return Err(invalid("duplicate output name"));
            }
            match projection {
                ReadProjection::Presence { .. } => {
                    if aggregating {
                        return Err(invalid("row presence requires an ungrouped read"));
                    }
                    projected.push(SelectedExpression {
                        expression: ResolvedOperand::RowPresence,
                        alias: ident(output, IdentRole::Alias)?,
                    });
                }
                ReadProjection::Row {
                    source,
                    fields,
                    optional,
                    ..
                } => {
                    if aggregating {
                        return Err(invalid("grouped reads require scalar projections"));
                    }
                    let layout = sources
                        .iter_mut()
                        .find(|s| &s.source.alias == source)
                        .ok_or_else(|| invalid("unknown projection source"))?;
                    if layout.nullable && !optional {
                        return Err(invalid(
                            "a nullable source requires an optional row projection",
                        ));
                    }
                    let allowed = crate::sql::descriptors::readable_fields(&layout.schema);
                    let selected = fields
                        .clone()
                        .unwrap_or_else(|| allowed.iter().cloned().collect());
                    if selected.is_empty() || selected.len() > MAX_READ_FIELDS {
                        return Err(invalid("row projection exceeds its field budget"));
                    }
                    for field in selected {
                        if !allowed.contains(&field) {
                            return Err(invalid(format!("unreadable projection field '{field}'")));
                        }
                        add_field(layout, &field, &mut projected)?;
                    }
                    add_field(layout, "id", &mut projected)?;
                }
                ReadProjection::Scalar { expression, .. } => {
                    if !aggregating {
                        let Operand::Path(path) = expression else {
                            return Err(invalid("scalar projection requires a column"));
                        };
                        let layout = source_for(path, &sources)?;
                        let alias = layout.source.alias.clone();
                        let layout = sources
                            .iter_mut()
                            .find(|s| s.source.alias == alias)
                            .unwrap();
                        let allowed = crate::sql::descriptors::readable_fields(&layout.schema);
                        if !allowed.contains(path.root().as_str()) {
                            return Err(invalid("unreadable scalar projection"));
                        }
                        add_field(layout, path.root().as_str(), &mut projected)?;
                        add_field(layout, "id", &mut projected)?;
                    } else {
                        let slot = ident(&format!("v{}", projected.len()), IdentRole::Alias)?;
                        scalar_schema.insert(
                            slot.as_str().into(),
                            scalar_definition(expression, &sources)?,
                        );
                        projected.push(SelectedExpression {
                            expression: resolve_operand(expression, &sources)?,
                            alias: slot.clone(),
                        });
                        scalar_slots.insert(output.clone(), slot.as_str().into());
                    }
                }
            }
        }
        if projected.len() > MAX_READ_FIELDS {
            return Err(invalid("read projection exceeds its field budget"));
        }
        let lock = row_lock(&input, &sources, aggregating, in_transaction, registration)?;
        let locked = lock != crate::sql::statement::RowLock::None;
        let mut filter = match input.model_filter {
            Some(filter) => crate::crud::predicate::resolve_model(
                filter,
                &sources[0].schema,
                &sources[0].resolved,
                registration,
            )?,
            None => resolve_predicate(&input.filter, &sources, registration)?,
        };
        if !input.source.include_deleted {
            filter = ResolvedPredicate::and(vec![
                filter,
                resolve_predicate(
                    &visible(&input.source, &sources[0].schema)?,
                    &sources,
                    registration,
                )?,
            ]);
        }
        let statement = SelectStatement::new(SelectParts {
            table: sources[0].resolved.table.clone(),
            joins,
            projection: projected,
            predicate: filter,
            group_by: input
                .group_by
                .iter()
                .map(|path| resolve_path(path, &sources).map(ResolvedOperand::Column))
                .collect::<Result<_, _>>()?,
            having: resolve_predicate(&input.having, &sources, registration)?,
            order_by: input
                .order_by
                .iter()
                .map(|order| {
                    Ok(ResolvedOrder {
                        expression: ResolvedOperand::Column(resolve_path(&order.path, &sources)?),
                        direction: order.direction,
                        nulls: order.nulls,
                    })
                })
                .collect::<Result<_, DbError>>()?,
            limit: Some(input.limit.get()),
            offset: Some(input.offset.get()),
            distinct: false,
            lock,
        })
        .map_err(|error| invalid(error.to_string()))?;
        let statement = match input.summary {
            Some(summary) => statement.summarize(summary),
            None => statement,
        };
        let query = registration
            .compile(Statement::select(statement))
            .map_err(|error| match error {
                crate::sql::compiler::CompileError::Unsupported(feature) if locked => {
                    super::unsupported_backend_feature(feature)
                }
                error => invalid(error.to_string()),
            })?;
        for source in &sources {
            crate::cdc::read_set::record_if_active(
                &source.source.collection,
                &Value::Null,
                &source.schema,
            );
        }
        Ok(Self {
            query,
            sources,
            projections: input.projection,
            scalar_slots,
            scalar_schema,
            summary: input.summary.is_some(),
        })
    }

    pub(super) async fn execute(
        self,
        binding: &DbBinding,
        route: &crate::tx_route::TxRoute,
    ) -> Result<Output, DbError> {
        if self.summary {
            return crate::exec::exec_count(route, self.query)
                .await
                .map(Output::Count);
        }
        let mut rows = crate::exec::exec_query(route, self.query).await?;
        let mut budget = MAX_READ_RESULT_BYTES;
        for row in &rows {
            consume_budget(row, &mut budget)?;
        }
        if !self.scalar_slots.is_empty() {
            decode_scalars(route.sql_registration(), &self.scalar_schema, &mut rows)?;
        }
        let mut blocks: Vec<Vec<Value>> = Vec::new();
        let mut has_masked = false;
        for source in &self.sources {
            let mut present = Vec::new();
            let mut positions = Vec::new();
            for (index, row) in rows.iter().enumerate() {
                if source.fields.is_empty() {
                    continue;
                }
                let slot = source
                    .fields
                    .get("id")
                    .ok_or_else(|| DbError::internal("read layout has no identity"))?;
                let id = row
                    .get(slot)
                    .ok_or_else(|| DbError::internal("missing read identity"))?;
                if id.is_null() {
                    if !source.nullable {
                        return Err(DbError::internal("required source has no identity"));
                    }
                    continue;
                }
                let mut fields = Record::new();
                for (field, slot) in &source.fields {
                    fields.insert(
                        field.clone(),
                        row.get(slot)
                            .cloned()
                            .ok_or_else(|| DbError::internal("read result is missing a field"))?,
                    );
                }
                present.push(Value::Object(fields));
                positions.push(index);
            }
            let fields: Vec<_> = source.fields.keys().cloned().collect();
            let decoded = crate::crud::read_pipeline::apply(
                route,
                binding,
                &source.source.collection,
                present,
                crate::crud::read_pipeline::ApplyOptions {
                    schema_field_scope: crate::crud::read_pipeline::SchemaFieldScope::Only(&fields),
                    ..Default::default()
                },
            )
            .await?;
            has_masked |= decoded.has_masked;
            let mut block = vec![Value::Null; rows.len()];
            for (index, row) in positions.into_iter().zip(decoded.rows) {
                block[index] = row;
            }
            blocks.push(block);
        }
        let mut result = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let mut output = Record::new();
            for projection in &self.projections {
                match projection {
                    ReadProjection::Presence { output: name } => {
                        output.insert(name.clone(), Value::Bool(true));
                    }
                    ReadProjection::Row {
                        output: name,
                        source,
                        fields,
                        ..
                    } => {
                        let slot = self
                            .sources
                            .iter()
                            .position(|s| &s.source.alias == source)
                            .unwrap();
                        let mut value = blocks[slot][index].clone();
                        if let (Some(fields), Some(record)) = (fields, value.as_object_mut()) {
                            record.retain(|field, _| fields.contains(field));
                        }
                        output.insert(name.clone(), value);
                    }
                    ReadProjection::Scalar {
                        output: name,
                        expression,
                    } => {
                        let value = if let Some(slot) = self.scalar_slots.get(name) {
                            row.get(slot)
                                .cloned()
                                .ok_or_else(|| DbError::internal("missing aggregate result"))?
                        } else {
                            let Operand::Path(path) = expression else {
                                return Err(DbError::internal("invalid scalar layout"));
                            };
                            let slot = self
                                .sources
                                .iter()
                                .position(|s| {
                                    path.source().is_some_and(|a| a.as_str() == s.source.alias)
                                })
                                .unwrap();
                            blocks[slot][index]
                                .get(path.root().as_str())
                                .cloned()
                                .unwrap_or(Value::Null)
                        };
                        output.insert(name.clone(), value);
                    }
                }
            }
            result.push(Value::Object(output));
        }
        let mut budget = MAX_READ_RESULT_BYTES;
        for row in &result {
            consume_budget(row, &mut budget)?;
        }
        Ok(Output::Rows {
            rows: result,
            has_masked,
        })
    }
}

/// Resolve a requested row lock against the prepared sources.
///
/// Every refusal happens before SQL, so a refused locking read leaves its
/// transaction usable.
fn row_lock(
    input: &ReadQuery,
    sources: &[SourceLayout],
    aggregating: bool,
    in_transaction: bool,
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<crate::sql::statement::RowLock, DbError> {
    let ReadLock::Update { of } = &input.lock else {
        return Ok(crate::sql::statement::RowLock::None);
    };
    if !in_transaction {
        return Err(super::transaction_required("row locks"));
    }
    if input.summary.is_some() {
        return Err(invalid("row locks cannot be combined with count or exists"));
    }
    if aggregating || !input.group_by.is_empty() || !matches!(input.having, Predicate::Const(true))
    {
        return Err(invalid("row locks require an ungrouped read without aggregates"));
    }
    if of.is_empty() && sources.iter().any(|source| source.nullable) {
        return Err(invalid(
            "an unqualified row lock cannot lock the nullable side of a left join; \
             name the locked sources with for_update_of",
        ));
    }
    let mut targets: Vec<Ident> = Vec::with_capacity(of.len());
    for alias in of {
        let source = sources
            .iter()
            .find(|source| &source.source.alias == alias)
            .ok_or_else(|| invalid("a row lock target must be a read source"))?;
        if source.nullable {
            return Err(invalid(
                "a row lock target cannot be the nullable side of a left join",
            ));
        }
        let target = ident(alias, IdentRole::Alias)?;
        if targets.contains(&target) {
            return Err(invalid("duplicate row lock target"));
        }
        targets.push(target);
    }
    if !registration.support().row_locks {
        return Err(super::unsupported_backend_feature("row locks"));
    }
    Ok(crate::sql::statement::RowLock::Required { of: targets })
}

pub(super) fn consume_budget(value: &Value, budget: &mut usize) -> Result<(), DbError> {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        let size = match value {
            Value::Object(fields) => {
                stack.extend(fields.values());
                fields.keys().map(String::len).sum::<usize>()
            }
            Value::Array(values) => {
                stack.extend(values);
                values.len()
            }
            Value::String(v) | Value::Decimal(v) | Value::Json(v) => v.len(),
            Value::Bytes(v) => v.len(),
            _ => std::mem::size_of::<Value>(),
        };
        *budget = budget
            .checked_sub(size)
            .ok_or_else(|| invalid("read result exceeds its size budget"))?;
    }
    Ok(())
}

fn add_field(
    source: &mut SourceLayout,
    field: &str,
    projection: &mut Vec<SelectedExpression>,
) -> Result<(), DbError> {
    if !source.fields.contains_key(field) {
        let slot = format!("v{}", projection.len());
        let input = source
            .resolved
            .inputs
            .get(field)
            .ok_or_else(|| invalid("projection field has no physical column"))?;
        projection.push(SelectedExpression {
            expression: ResolvedOperand::Column(
                source
                    .resolved
                    .table
                    .column(&input.column)
                    .map_err(|error| invalid(error.to_string()))?,
            ),
            alias: ident(&slot, IdentRole::Alias)?,
        });
        source.fields.insert(field.into(), slot);
    }
    Ok(())
}

fn resolve_path(
    path: &FieldPath,
    sources: &[SourceLayout],
) -> Result<crate::sql::statement::Column, DbError> {
    let source = source_for(path, sources)?;
    let input = source
        .resolved
        .inputs
        .get(path.root().as_str())
        .ok_or_else(|| invalid("read field has no physical column"))?;
    source
        .resolved
        .table
        .column(&input.column)
        .map_err(|error| invalid(error.to_string()))
}

fn resolve_operand(
    operand: &Operand,
    sources: &[SourceLayout],
) -> Result<ResolvedOperand, DbError> {
    Ok(match operand {
        Operand::Path(path) => ResolvedOperand::Column(resolve_path(path, sources)?),
        Operand::Aggregate(aggregate) => {
            if let Some(path) = aggregate.argument() {
                check_field(path, sources, true)?;
                check_capability(path, sources, ReadCapability::Aggregateable)?;
                let definition = &source_for(path, sources)?.schema[path.root().as_str()];
                if !crate::sql::descriptors::supports_aggregate(definition, aggregate.func()) {
                    return Err(invalid(
                        "aggregate requires a compatible portable scalar column",
                    ));
                }
            }
            ResolvedOperand::Aggregate {
                function: aggregate.func(),
                column: aggregate
                    .argument()
                    .map(|path| resolve_path(path, sources))
                    .transpose()?,
                distinct: aggregate.is_distinct(),
            }
        }
        Operand::Lit(_) => return Err(invalid("literal cannot be used as a resolved expression")),
    })
}

fn resolve_predicate(
    predicate: &Predicate,
    sources: &[SourceLayout],
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<ResolvedPredicate, DbError> {
    Ok(match predicate {
        Predicate::And(children) => ResolvedPredicate::and(
            children
                .iter()
                .map(|child| resolve_predicate(child, sources, registration))
                .collect::<Result<_, _>>()?,
        ),
        Predicate::Or(children) => ResolvedPredicate::or(
            children
                .iter()
                .map(|child| resolve_predicate(child, sources, registration))
                .collect::<Result<_, _>>()?,
        ),
        Predicate::Not(child) => {
            ResolvedPredicate::Not(Box::new(resolve_predicate(child, sources, registration)?))
        }
        Predicate::Compare { lhs, op, rhs } => {
            let lhs = resolve_operand(lhs, sources)?;
            let rhs = match rhs {
                Operand::Lit(value) => ResolvedPredicateValue::Bind {
                    storage: lhs.storage().map_err(|error| invalid(error.to_string()))?,
                    value: encode_literal(value, &lhs, sources, registration)?,
                },
                rhs => ResolvedPredicateValue::Operand(resolve_operand(rhs, sources)?),
            };
            ResolvedPredicate::Compare { lhs, op: *op, rhs }
        }
        Predicate::Membership { lhs, op, set } => {
            let lhs = resolve_operand(lhs, sources)?;
            let values = set
                .values()
                .iter()
                .map(|value| encode_literal(value, &lhs, sources, registration))
                .collect::<Result<_, _>>()?;
            ResolvedPredicate::Membership {
                lhs,
                op: *op,
                values,
            }
        }
        Predicate::Pattern {
            lhs,
            op,
            pattern,
            escape,
        } => ResolvedPredicate::Pattern {
            lhs: resolve_operand(lhs, sources)?,
            op: *op,
            value: pattern.as_str().to_owned(),
            escape: escape.map(crate::sql::EscapeChar::get),
        },
        Predicate::IsNull { operand, negated } => ResolvedPredicate::IsNull {
            operand: resolve_operand(operand, sources)?,
            negated: *negated,
        },
        Predicate::Const(value) => ResolvedPredicate::Const(*value),
    })
}

fn encode_literal(
    literal: &crate::sql::Literal,
    operand: &ResolvedOperand,
    sources: &[SourceLayout],
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<Value, DbError> {
    let storage = operand
        .storage()
        .map_err(|error| invalid(error.to_string()))?;
    let mut value = match literal {
        crate::sql::Literal::Bool(value) => Value::Bool(*value),
        crate::sql::Literal::Int(value) => Value::from(*value),
        crate::sql::Literal::TimestampMicros(value) => Value::TimestampMicros(*value),
        crate::sql::Literal::Float(value) => {
            Value::try_from(value.get()).map_err(|_| invalid("non-finite filter value"))?
        }
        crate::sql::Literal::Text(value) if storage.decimal().is_some() => {
            Value::Decimal(value.clone())
        }
        crate::sql::Literal::Text(value) => Value::String(value.clone()),
        crate::sql::Literal::Json(value) => Value::Json(value.clone()),
        crate::sql::Literal::Bytes(value) => Value::Bytes(value.clone()),
        crate::sql::Literal::Vector(value) => Value::Array(
            value
                .elements()
                .iter()
                .map(|value| Value::try_from(f64::from(value.get())).expect("finite vector"))
                .collect(),
        ),
    };
    // MIN/MAX keep the column codec; COUNT/SUM/AVG bind against their result type.
    let path = match operand {
        ResolvedOperand::Column(column)
        | ResolvedOperand::Aggregate {
            function: crate::sql::AggregateFunc::Min | crate::sql::AggregateFunc::Max,
            column: Some(column),
            ..
        } => sources.iter().find_map(|source| {
            source.resolved.table.owns(column).then(|| {
                source
                    .resolved
                    .inputs
                    .iter()
                    .find(|(_, input)| input.column == column.name().as_str())
                    .map(|(field, _)| (field.as_str(), &source.schema))
            })
        }),
        ResolvedOperand::RowPresence
        | ResolvedOperand::Aggregate { .. }
        | ResolvedOperand::Comparison(_) => None,
    }
    .flatten();
    if let Some((field, schema)) = path {
        if let Some(definition) = schema.get(field) {
            crate::sql::codecs::prepare_value(field, definition, &mut value)
                .map_err(|error| invalid(error.to_string()))?;
        }
    }
    registration
        .encode(storage, value)
        .map_err(|error| invalid(error.to_string()))
}
fn visible(source: &ReadSource, schema: &FieldMap) -> Result<Predicate, DbError> {
    Ok(match crate::sql::lifecycle::soft_delete_column(schema)? {
        Some(column) => Predicate::is_null(Operand::Path(source.column(column)?)),
        None => Predicate::always(),
    })
}

fn source_for<'a>(
    path: &FieldPath,
    sources: &'a [SourceLayout],
) -> Result<&'a SourceLayout, DbError> {
    if path.is_nested() {
        return Err(invalid("nested read paths are not supported"));
    }
    let alias = path
        .source()
        .ok_or_else(|| invalid("read columns require a source alias"))?;
    sources
        .iter()
        .find(|s| s.source.alias == alias.as_str())
        .ok_or_else(|| invalid("unknown read source"))
}
fn check_field(path: &FieldPath, sources: &[SourceLayout], plain: bool) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    let field = path.root().as_str();
    if !crate::sql::descriptors::readable_fields(&source.schema).contains(field) {
        return Err(invalid(format!("unreadable field '{field}'")));
    }
    if source
        .schema
        .get(field)
        .is_some_and(crate::sql::descriptors::is_encrypted)
        || (plain && crate::sql::mapping::column_is_masked(field, &source.schema))
    {
        return Err(invalid("protected field is not valid in this expression"));
    }
    Ok(())
}
enum ReadCapability {
    Filterable,
    Sortable,
    Aggregateable,
}

fn check_capability(
    path: &FieldPath,
    sources: &[SourceLayout],
    capability: ReadCapability,
) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    let definition = source
        .schema
        .get(path.root().as_str())
        .ok_or_else(|| invalid("unknown read field"))?;
    let (allowed, capability) = match capability {
        ReadCapability::Filterable => (definition.filterable, "filterable"),
        ReadCapability::Sortable => (definition.sortable, "sortable"),
        ReadCapability::Aggregateable => (definition.aggregateable, "aggregateable"),
    };
    if !allowed {
        return Err(invalid(format!(
            "field '{}' is not {capability}",
            path.root().as_str()
        )));
    }
    Ok(())
}
fn check_sorting(path: &FieldPath, sources: &[SourceLayout]) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    let definition = &source.schema[path.root().as_str()];
    crate::sql::descriptors::supports_sorting(definition)
        .then_some(())
        .ok_or_else(|| invalid("field has no portable sort order"))
}
fn check_grouping(path: &FieldPath, sources: &[SourceLayout]) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    let definition = &source.schema[path.root().as_str()];
    crate::sql::descriptors::supports_grouping(definition)
        .then_some(())
        .ok_or_else(|| invalid("field has no portable grouping equality"))
}
fn validate_predicate(
    predicate: &Predicate,
    sources: &[SourceLayout],
    join: bool,
) -> Result<(), DbError> {
    for path in crate::sql::joins::predicate_paths(predicate).map_err(|e| invalid(e.to_string()))? {
        check_field(path, sources, join)?;
        check_capability(path, sources, ReadCapability::Filterable)?;
    }
    let mut stack = vec![predicate];
    while let Some(predicate) = stack.pop() {
        match predicate {
            Predicate::And(children) | Predicate::Or(children) => stack.extend(children),
            Predicate::Not(child) => stack.push(child),
            Predicate::Compare {
                lhs: Operand::Path(a),
                op,
                rhs: Operand::Path(b),
            } => {
                for path in [a, b] {
                    check_field(path, sources, true)?;
                    check_predicate_operator(path, sources, comparison_operator(*op))?;
                }
                if scalar_kind(a, sources)? != scalar_kind(b, sources)? {
                    return Err(invalid("join columns have incompatible types"));
                }
            }
            Predicate::Compare { lhs, op, rhs } => {
                for operand in [lhs, rhs] {
                    if let Operand::Path(path) = operand {
                        check_predicate_operator(path, sources, comparison_operator(*op))?;
                    }
                }
            }
            Predicate::Membership {
                lhs: Operand::Path(path),
                ..
            } => {
                check_predicate_operator(
                    path,
                    sources,
                    crate::sql::descriptors::PredicateOperator::Equality,
                )?;
            }
            Predicate::Pattern {
                lhs: Operand::Path(path),
                ..
            } => {
                check_predicate_operator(
                    path,
                    sources,
                    crate::sql::descriptors::PredicateOperator::Pattern,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn comparison_operator(op: crate::sql::CompareOp) -> crate::sql::descriptors::PredicateOperator {
    if matches!(op, crate::sql::CompareOp::Eq | crate::sql::CompareOp::Ne) {
        crate::sql::descriptors::PredicateOperator::Equality
    } else {
        crate::sql::descriptors::PredicateOperator::Ordering
    }
}

fn check_predicate_operator(
    path: &FieldPath,
    sources: &[SourceLayout],
    operator: crate::sql::descriptors::PredicateOperator,
) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    let definition = &source.schema[path.root().as_str()];
    crate::sql::descriptors::supports_predicate_operator(definition, operator)
        .then_some(())
        .ok_or_else(|| invalid("predicate operator is not supported for this field type"))
}
fn scalar_kind(path: &FieldPath, sources: &[SourceLayout]) -> Result<LogicalType, DbError> {
    let source = source_for(path, sources)?;
    let field = path.root().as_str();
    let kind = source
        .schema
        .get(field)
        .map(|definition| definition.logical_type)
        .ok_or_else(|| invalid(format!("field '{field}' has no declared type")))?;
    match kind {
        LogicalType::Integer | LogicalType::BigInt => Ok(LogicalType::Integer),
        LogicalType::Text
        | LogicalType::Timestamp
        | LogicalType::Number
        | LogicalType::Boolean
        | LogicalType::Bytes
        | LogicalType::CalendarDate
        | LogicalType::Time => Ok(kind),
        _ => Err(invalid("join keys must be scalar columns")),
    }
}

fn scalar_definition(
    expression: &Operand,
    sources: &[SourceLayout],
) -> Result<ColumnSchema, DbError> {
    use crate::sql::AggregateFunc;
    let path = match expression {
        Operand::Path(path) => path,
        Operand::Aggregate(aggregate) => {
            if aggregate.func() == AggregateFunc::Count {
                return Ok(ColumnSchema::new(LogicalType::BigInt));
            }
            let path = aggregate
                .argument()
                .ok_or_else(|| invalid("aggregate requires a column"))?;
            let kind = scalar_kind(path, sources)?;
            let definition = &source_for(path, sources)?.schema[path.root().as_str()];
            if !crate::sql::descriptors::supports_aggregate(definition, aggregate.func()) {
                return Err(invalid(
                    "aggregate requires a compatible portable scalar column",
                ));
            }
            if matches!(aggregate.func(), AggregateFunc::Sum | AggregateFunc::Avg) {
                let kind = if aggregate.func() == AggregateFunc::Avg || kind == LogicalType::Number
                {
                    LogicalType::Number
                } else {
                    LogicalType::BigInt
                };
                return Ok(ColumnSchema {
                    required: false,
                    ..ColumnSchema::new(kind)
                });
            }
            path
        }
        Operand::Lit(_) => return Err(invalid("literal projections are not supported")),
    };
    source_for(path, sources)?
        .schema
        .get(path.root().as_str())
        .cloned()
        .ok_or_else(|| invalid("unknown read field"))
}

pub(crate) fn decode_scalars(
    registration: &crate::sql::registration::SqlRegistration,
    schema: &FieldMap,
    rows: &mut [Value],
) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        let Some(fields) = row.as_object_mut() else {
            continue;
        };
        for (slot, value) in fields {
            if let Value::Decimal(decimal) = value {
                *value = match schema.get(slot).map(|definition| definition.logical_type) {
                    Some(LogicalType::Integer | LogicalType::BigInt) => decimal
                        .parse::<i64>()
                        .map(Value::from)
                        .map_err(|_| invalid("aggregate integer is out of range"))?,
                    Some(LogicalType::Number) => {
                        let number = decimal
                            .parse::<f64>()
                            .map_err(|_| invalid("invalid numeric aggregate"))?;
                        Value::try_from(number)
                            .map_err(|_| invalid("aggregate number is out of range"))?
                    }
                    _ => continue,
                };
            }
        }
    }
    registration.decode_rows(schema, rows)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::CompareOp;
    use crate::value;

    #[test]
    fn row_presence_selects_a_constant_without_loading_fields() {
        use crate::schema::{CollectionSchema, Schema};
        use crate::sql::registration::SqlRegistration;
        let binding = crate::tests::fixtures::harness_binding(zeroship_core::app_id::AppId::mint().as_str());
        crate::OrmContext::new().with(|| {
            let schema = CollectionSchema::from_fields(&value!({
                "id":{"type":"string", "primaryKey":true, "required":true, "readable":false},
                "private":{"type":"bytes", "readable":false},
            }))
            .unwrap();
            crate::descriptor::install_collections(
                &binding,
                Schema::new([("records".into(), schema)]),
            )
            .unwrap();
            for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
                let mut query = ReadQuery::new(ReadSource::new("records", "r"));
                query.projection.push(ReadProjection::Presence {
                    output: "present".into(),
                });
                query.limit = RowLimit::new(1).unwrap();
                let prepared = PreparedRead::new(&binding, &registration, false, query).unwrap();
                assert!(prepared
                    .query
                    .sql()
                    .starts_with("SELECT 1 AS \"present\" FROM "));
                assert!(prepared.query.sql().contains(" LIMIT $1"));
                assert_eq!(prepared.query.params()[0], value!(1));
                assert!(prepared
                    .sources
                    .iter()
                    .all(|source| source.fields.is_empty()));
            }
        });
    }

    fn source(schema: FieldMap) -> SourceLayout {
        let schema = Arc::new(crate::schema::CollectionSchema::new(schema).into_fields());
        let registration = crate::sql::registration::SqlRegistration::postgres();
        let resolved = crate::crud::resolved::ResolvedTable::aliased(
            &crate::sql::SchemaName::new("read_tests").unwrap(),
            "records",
            "r",
            &schema,
            &registration,
        )
        .unwrap();
        SourceLayout {
            source: ReadSource::new("records", "r"),
            schema,
            nullable: false,
            fields: BTreeMap::new(),
            resolved,
        }
    }

    fn fields(definitions: &[(&str, LogicalType)]) -> FieldMap {
        definitions
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), ColumnSchema::new(*kind)))
            .collect()
    }

    #[test]
    fn column_types_come_only_from_declared_metadata() {
        let sources = vec![source(fields(&[
            ("id", LogicalType::Integer),
            ("created_at", LogicalType::Text),
            ("version", LogicalType::Bytes),
            ("event_time", LogicalType::Timestamp),
        ]))];
        for (field, expected) in [
            ("id", LogicalType::Integer),
            ("created_at", LogicalType::Text),
            ("version", LogicalType::Bytes),
            ("event_time", LogicalType::Timestamp),
        ] {
            assert_eq!(
                scalar_kind(&sources[0].source.column(field).unwrap(), &sources).unwrap(),
                expected
            );
        }
        for field in ["updated_at", "created_by", "deleted_at"] {
            assert!(scalar_kind(&sources[0].source.column(field).unwrap(), &sources).is_err());
        }
        assert_eq!(
            crate::sql::descriptors::readable_fields(&sources[0].schema),
            ["id", "created_at", "version", "event_time"]
                .map(String::from)
                .into()
        );
    }

    #[test]
    fn read_predicates_enforce_the_portable_operator_matrix() {
        let mut schema = fields(&[
            ("id", LogicalType::Text),
            ("enabled", LogicalType::Boolean),
            ("amount", LogicalType::Number),
            ("payload", LogicalType::Json),
            ("embedding", LogicalType::Vector),
            ("location", LogicalType::GeoPoint),
        ]);
        schema["amount"].precision = Some(18);
        schema["amount"].scale = Some(2);
        schema["embedding"].vector_dims = Some(2);
        let sources = vec![source(schema)];
        let literal =
            |value| Operand::Lit(crate::sql::Literal::try_from_value(value).unwrap().unwrap());
        for (field, op, value) in [
            ("enabled", CompareOp::Gt, value!(false)),
            ("amount", CompareOp::Lt, Value::Decimal("10.5".into())),
            ("payload", CompareOp::Gte, value!({"key":true})),
            ("embedding", CompareOp::Eq, value!([1, 2])),
            ("location", CompareOp::Eq, value!({"lat":1,"lng":2})),
        ] {
            let predicate = Predicate::compare(
                Operand::Path(sources[0].source.column(field).unwrap()),
                op,
                literal(value),
            );
            assert!(
                validate_predicate(&predicate, &sources, false).is_err(),
                "{field}"
            );
        }

        let predicate = Predicate::compare(
            Operand::Path(sources[0].source.column("payload").unwrap()),
            CompareOp::Eq,
            literal(value!({"key":true})),
        );
        assert!(validate_predicate(&predicate, &sources, false).is_ok());
    }

    #[test]
    fn typed_read_sorting_and_grouping_follow_portable_semantics() {
        let mut schema = fields(&[
            ("id", LogicalType::Text),
            ("enabled", LogicalType::Boolean),
            ("amount", LogicalType::Number),
            ("payload", LogicalType::Json),
            ("bytes", LogicalType::Bytes),
            ("score", LogicalType::Number),
        ]);
        schema["amount"].precision = Some(18);
        schema["amount"].scale = Some(2);
        let sources = vec![source(schema)];
        for field in ["enabled", "amount", "payload", "bytes"] {
            assert!(
                check_sorting(&sources[0].source.column(field).unwrap(), &sources).is_err(),
                "{field}"
            );
        }
        for field in ["amount", "payload"] {
            assert!(
                check_grouping(&sources[0].source.column(field).unwrap(), &sources).is_err(),
                "{field}"
            );
        }
        for field in ["id", "score"] {
            assert!(check_sorting(&sources[0].source.column(field).unwrap(), &sources).is_ok());
        }
        for field in ["id", "enabled", "bytes", "score"] {
            assert!(check_grouping(&sources[0].source.column(field).unwrap(), &sources).is_ok());
        }
    }

    #[test]
    fn familiar_names_do_not_bypass_projection_flags() {
        let mut schema = fields(&[("id", LogicalType::Text), ("version", LogicalType::Integer)]);
        schema["id"].readable = false;
        schema["version"].projectable = false;
        let sources = vec![source(schema)];
        for field in ["id", "version", "created_at"] {
            assert!(
                check_field(&sources[0].source.column(field).unwrap(), &sources, false).is_err()
            );
        }
    }

    #[test]
    fn familiar_names_do_not_bypass_expression_flags() {
        let mut schema = fields(&[
            ("created_at", LogicalType::Text),
            ("version", LogicalType::Integer),
        ]);
        schema["created_at"].filterable = false;
        schema["version"].sortable = false;
        let sources = vec![source(schema)];
        let path = sources[0].source.column("created_at").unwrap();
        assert!(
            validate_predicate(&Predicate::is_null(Operand::Path(path)), &sources, false).is_err()
        );
        let path = sources[0].source.column("version").unwrap();
        assert!(check_capability(&path, &sources, ReadCapability::Sortable).is_err());
    }

    #[test]
    fn native_integer_join_keys_share_a_scalar_kind() {
        let sources = vec![source(fields(&[
            ("small_id", LogicalType::Integer),
            ("large_id", LogicalType::BigInt),
            ("text_id", LogicalType::Text),
            ("payload", LogicalType::Json),
        ]))];
        let compare = |right| {
            Predicate::compare(
                Operand::Path(sources[0].source.column("small_id").unwrap()),
                CompareOp::Eq,
                Operand::Path(sources[0].source.column(right).unwrap()),
            )
        };
        assert!(validate_predicate(&compare("large_id"), &sources, true).is_ok());
        for right in ["text_id", "payload"] {
            assert!(validate_predicate(&compare(right), &sources, true).is_err());
        }
    }

    #[test]
    fn aggregate_result_fields_decode_without_collection_identity() {
        use crate::sql::{AggregateFunc, AggregateRef};
        let sources = vec![source(fields(&[
            ("amount", LogicalType::Integer),
            ("score", LogicalType::Number),
        ]))];
        let aggregate = |function, field| {
            Operand::Aggregate(
                AggregateRef::over_path(function, sources[0].source.column(field).unwrap(), false)
                    .unwrap(),
            )
        };
        let mut schema = FieldMap::new();
        for (slot, expression, kind) in [
            (
                "count",
                Operand::Aggregate(AggregateRef::count_rows()),
                LogicalType::BigInt,
            ),
            (
                "sum",
                aggregate(AggregateFunc::Sum, "amount"),
                LogicalType::BigInt,
            ),
            (
                "average",
                aggregate(AggregateFunc::Avg, "amount"),
                LogicalType::Number,
            ),
            (
                "minimum",
                aggregate(AggregateFunc::Min, "score"),
                LogicalType::Number,
            ),
        ] {
            let definition = scalar_definition(&expression, &sources).unwrap();
            assert_eq!(definition.logical_type, kind);
            schema.insert(slot.to_owned(), definition);
        }
        assert!(!schema.contains_key("id"));
        for registration in [
            crate::sql::registration::SqlRegistration::postgres(),
            crate::sql::registration::SqlRegistration::sqlite(),
        ] {
            let mut rows = vec![Value::Object(
                [
                    ("count".into(), Value::Decimal("2".into())),
                    ("sum".into(), Value::Decimal("12".into())),
                    ("average".into(), Value::Decimal("6.0".into())),
                    ("minimum".into(), Value::Null),
                ]
                .into(),
            )];
            decode_scalars(&registration, &schema, &mut rows).unwrap();
            assert_eq!(rows[0]["count"].as_i64(), Some(2));
            assert_eq!(rows[0]["sum"].as_i64(), Some(12));
            assert_eq!(rows[0]["average"].as_f64(), Some(6.0));
            assert!(rows[0]["minimum"].is_null());
            let mut invalid = vec![Value::Object(
                [("sum".into(), Value::Decimal("9223372036854775808".into()))].into(),
            )];
            assert!(decode_scalars(&registration, &schema, &mut invalid).is_err());
        }
    }

    #[test]
    fn count_comparisons_use_the_aggregate_result_type() {
        use crate::sql::{AggregateFunc, AggregateRef};
        let mut schema = fields(&[
            ("enabled", LogicalType::Boolean),
            ("day", LogicalType::CalendarDate),
            ("tags", LogicalType::Array),
            ("embedding", LogicalType::Vector),
            ("amount", LogicalType::Number),
        ]);
        schema["tags"].items = Some(LogicalType::Text);
        schema["embedding"].vector_dims = Some(2);
        schema["amount"].precision = Some(18);
        schema["amount"].scale = Some(2);
        let sources = vec![source(schema)];
        for registration in [
            crate::sql::registration::SqlRegistration::postgres(),
            crate::sql::registration::SqlRegistration::sqlite(),
        ] {
            for field in ["enabled", "day", "tags", "embedding", "amount"] {
                let aggregate = Operand::Aggregate(
                    AggregateRef::over_path(
                        AggregateFunc::Count,
                        sources[0].source.column(field).unwrap(),
                        false,
                    )
                    .unwrap(),
                );
                let predicate = Predicate::compare(
                    aggregate,
                    CompareOp::Gt,
                    Operand::Lit(crate::sql::Literal::Int(0)),
                );
                let result = resolve_predicate(&predicate, &sources, &registration);
                assert!(
                    matches!(result, Ok(ResolvedPredicate::Compare {
                    rhs: ResolvedPredicateValue::Bind { ref value, .. }, ..
                }) if value.as_i64() == Some(0)),
                    "{field}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn aggregate_operands_enforce_native_protection_and_capability() {
        use crate::sql::{AggregateFunc, AggregateRef};
        let mut schema = fields(&[
            ("plain", LogicalType::Integer),
            ("unreadable", LogicalType::Integer),
            ("masked", LogicalType::Integer),
            ("encrypted", LogicalType::Text),
            ("denied", LogicalType::Integer),
        ]);
        schema["unreadable"].readable = false;
        schema["masked"].mask = Some(crate::schema::MaskSchema {
            kind: "full".into(),
            classification: "pii".into(),
        });
        schema["encrypted"].encrypted = true;
        schema["denied"].aggregateable = false;
        let sources = vec![source(schema)];
        let aggregate = |field| {
            Operand::Aggregate(
                AggregateRef::over_path(
                    AggregateFunc::Count,
                    sources[0].source.column(field).unwrap(),
                    false,
                )
                .unwrap(),
            )
        };
        assert!(resolve_operand(&aggregate("plain"), &sources).is_ok());
        assert!(resolve_operand(&Operand::Aggregate(AggregateRef::count_rows()), &sources).is_ok());
        for field in ["unreadable", "masked", "encrypted", "denied"] {
            assert!(
                resolve_operand(&aggregate(field), &sources).is_err(),
                "{field}"
            );
            let having = Predicate::compare(
                aggregate(field),
                CompareOp::Gt,
                Operand::Lit(
                    crate::sql::Literal::try_from_value(Value::from(0))
                        .unwrap()
                        .unwrap(),
                ),
            );
            assert!(
                resolve_predicate(
                    &having,
                    &sources,
                    &crate::sql::registration::SqlRegistration::postgres(),
                )
                .is_err(),
                "HAVING: {field}"
            );
        }
    }
}
