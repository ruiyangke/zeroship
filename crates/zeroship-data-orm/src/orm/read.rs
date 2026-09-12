//! Shared relational reads and provenance-preserving result materialization.

use super::*;
use crate::sql::statement::{
    ResolvedJoin, ResolvedOperand, ResolvedOrder, ResolvedPredicate, ResolvedPredicateValue,
    SelectParts, SelectStatement, SelectedExpression, Statement, StorageType,
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
        }
    }
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
    schema: Arc<Value>,
    nullable: bool,
    fields: BTreeMap<String, String>,
    resolved: crate::crud::resolved::ResolvedTable,
}
#[derive(Debug)]
pub(super) struct PreparedRead {
    query: CompiledQuery,
    dialect: crate::sql::compile::SqlDialect,
    sources: Vec<SourceLayout>,
    projections: Vec<ReadProjection>,
    scalar_slots: BTreeMap<String, String>,
    scalar_schema: Value,
}

impl PreparedRead {
    pub(super) fn new(
        binding: &DbBinding,
        registration: &crate::sql::registration::SqlRegistration,
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
            check_capability(path, &sources, "filterable")?;
        }
        for key in &input.order_by {
            check_field(&key.path, &sources, false)?;
            check_capability(&key.path, &sources, "sortable")?;
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
        let mut scalar_schema = Record::new();
        for projection in &input.projection {
            let output = match projection {
                ReadProjection::Row { output, .. } | ReadProjection::Scalar { output, .. } => {
                    output
                }
            };
            ident(output, IdentRole::Alias)?;
            if !output_names.insert(output.clone()) {
                return Err(invalid("duplicate output name"));
            }
            match projection {
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
        let filter = if input.source.include_deleted {
            input.filter
        } else {
            Predicate::And(vec![
                input.filter,
                visible(&input.source, &sources[0].schema)?,
            ])
        };
        let statement = SelectStatement::new(SelectParts {
            table: sources[0].resolved.table.clone(),
            joins,
            projection: projected,
            predicate: resolve_predicate(&filter, &sources, registration)?,
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
        })
        .map_err(|error| invalid(error.to_string()))?;
        let query = registration
            .compile(Statement::Select(statement))
            .map_err(|error| invalid(error.to_string()))?;
        for source in &sources {
            crate::cdc::read_set::record_if_active(
                &source.source.collection,
                &Value::Null,
                &source.schema,
            );
        }
        Ok(Self {
            query,
            dialect: registration.dialect(),
            sources,
            projections: input.projection,
            scalar_slots,
            scalar_schema: Value::Object(scalar_schema),
        })
    }

    pub(super) async fn execute(
        self,
        binding: &DbBinding,
        route: &crate::tx_route::TxRoute,
    ) -> Result<Output, DbError> {
        let mut rows = crate::exec::exec_query(route, self.query).await?;
        let mut budget = MAX_READ_RESULT_BYTES;
        for row in &rows {
            consume_budget(row, &mut budget)?;
        }
        if !self.scalar_slots.is_empty() {
            decode_scalars(self.dialect, &self.scalar_schema, &mut rows)?;
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

fn consume_budget(value: &Value, budget: &mut usize) -> Result<(), DbError> {
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
        Operand::Aggregate(aggregate) => ResolvedOperand::Aggregate {
            function: aggregate.func(),
            column: aggregate
                .argument()
                .map(|path| resolve_path(path, sources))
                .transpose()?,
            distinct: aggregate.is_distinct(),
        },
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
        crate::sql::Literal::Float(value) => {
            Value::try_from(value.get()).map_err(|_| invalid("non-finite filter value"))?
        }
        crate::sql::Literal::Text(value) if storage == StorageType::Decimal => {
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
    let path = match operand {
        ResolvedOperand::Column(column) => sources.iter().find_map(|source| {
            source.resolved.table.owns(column).then(|| {
                source
                    .resolved
                    .inputs
                    .iter()
                    .find(|(_, input)| input.column == column.name().as_str())
                    .map(|(field, _)| (field.as_str(), &source.schema))
            })
        }),
        ResolvedOperand::Aggregate {
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
        ResolvedOperand::Aggregate { column: None, .. } => None,
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
fn visible(source: &ReadSource, schema: &Value) -> Result<Predicate, DbError> {
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
        || (plain && crate::sql::compile::column_is_masked(field, &source.schema))
    {
        return Err(invalid("protected field is not valid in this expression"));
    }
    Ok(())
}
fn check_capability(
    path: &FieldPath,
    sources: &[SourceLayout],
    capability: &str,
) -> Result<(), DbError> {
    let source = source_for(path, sources)?;
    if source.schema[path.root().as_str()][capability].as_bool() == Some(false) {
        return Err(invalid(format!(
            "field '{}' is not {capability}",
            path.root().as_str()
        )));
    }
    Ok(())
}
fn validate_predicate(
    predicate: &Predicate,
    sources: &[SourceLayout],
    join: bool,
) -> Result<(), DbError> {
    for path in crate::sql::joins::predicate_paths(predicate).map_err(|e| invalid(e.to_string()))? {
        check_field(path, sources, join)?;
        check_capability(path, sources, "filterable")?;
    }
    let mut stack = vec![predicate];
    while let Some(predicate) = stack.pop() {
        match predicate {
            Predicate::And(children) | Predicate::Or(children) => stack.extend(children),
            Predicate::Not(child) => stack.push(child),
            Predicate::Compare {
                lhs: Operand::Path(a),
                rhs: Operand::Path(b),
                ..
            } => {
                for path in [a, b] {
                    check_field(path, sources, true)?;
                }
                if scalar_kind(a, sources)? != scalar_kind(b, sources)? {
                    return Err(invalid("join columns have incompatible types"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn scalar_kind<'a>(path: &FieldPath, sources: &'a [SourceLayout]) -> Result<&'a str, DbError> {
    let source = source_for(path, sources)?;
    let field = path.root().as_str();
    let kind = source
        .schema
        .get(field)
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("field '{field}' has no declared type")))?;
    match kind {
        "string" | "text" | "ref" | "enum" => Ok("string"),
        "date" | "timestamp" => Ok("timestamp"),
        "integer" | "int" | "bigInt" => Ok("integer"),
        "number" | "float" | "double" => Ok("number"),
        "boolean" | "bool" => Ok("boolean"),
        "decimal" | "bytes" | "calendarDate" | "time" => Ok(kind),
        _ => Err(invalid("join keys must be scalar columns")),
    }
}

fn scalar_definition(expression: &Operand, sources: &[SourceLayout]) -> Result<Value, DbError> {
    use crate::sql::AggregateFunc;
    let path = match expression {
        Operand::Path(path) => path,
        Operand::Aggregate(aggregate) => {
            if aggregate.func() == AggregateFunc::Count {
                return Ok(crate::value!({"type":"bigInt"}));
            }
            let path = aggregate
                .argument()
                .ok_or_else(|| invalid("aggregate requires a column"))?;
            let kind = scalar_kind(path, sources)?;
            if matches!(aggregate.func(), AggregateFunc::Sum | AggregateFunc::Avg) {
                if !matches!(kind, "integer" | "number") {
                    return Err(invalid("sum and avg require an integer or number column"));
                }
                return Ok(
                    crate::value!({"type": if aggregate.func() == AggregateFunc::Avg || kind == "number" { "number" } else { "bigInt" }}),
                );
            }
            if matches!(kind, "boolean" | "bytes" | "decimal") {
                return Err(invalid(
                    "min and max require an ordered portable scalar column",
                ));
            }
            path
        }
        Operand::Lit(_) => return Err(invalid("literal projections are not supported")),
    };
    Ok(source_for(path, sources)?.schema[path.root().as_str()].clone())
}

fn decode_scalars(
    dialect: crate::sql::compile::SqlDialect,
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        let Some(fields) = row.as_object_mut() else {
            continue;
        };
        for (slot, value) in fields {
            if let Value::Decimal(decimal) = value {
                *value =
                    match schema[slot]["type"].as_str() {
                        Some("int" | "integer" | "bigInt") => decimal
                            .parse::<i64>()
                            .map(Value::from)
                            .map_err(|_| invalid("aggregate integer is out of range"))?,
                        Some("number") => {
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
    crate::sql::codecs::decode_rows(dialect, schema, rows)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    fn source(schema: Value) -> SourceLayout {
        let schema = Arc::new(schema);
        let resolved_schema = Value::Object(
            schema
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, definition)| definition.get("type").and_then(Value::as_str).is_some())
                .map(|(field, definition)| (field.clone(), definition.clone()))
                .collect(),
        );
        let registration = crate::sql::registration::SqlRegistration::builtin(
            crate::sql::compile::SqlDialect::Postgres,
        );
        let resolved = crate::crud::resolved::ResolvedTable::aliased(
            &crate::sql::SchemaName::new("read_tests").unwrap(),
            "records",
            "r",
            &resolved_schema,
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

    #[test]
    fn column_types_come_only_from_the_descriptor() {
        let sources = vec![source(value!({
            "id": {"type":"int"}, "created_at": {"type":"string"},
            "version": {"type":"bytes"}, "event_time": {"type":"date"},
            "missing_type": {}
        }))];
        for (field, expected) in [
            ("id", "integer"),
            ("created_at", "string"),
            ("version", "bytes"),
            ("event_time", "timestamp"),
        ] {
            assert_eq!(
                scalar_kind(&sources[0].source.column(field).unwrap(), &sources).unwrap(),
                expected
            );
        }
        for field in ["updated_at", "created_by", "deleted_at", "missing_type"] {
            assert!(scalar_kind(&sources[0].source.column(field).unwrap(), &sources).is_err());
        }
        assert_eq!(
            crate::sql::descriptors::readable_fields(&sources[0].schema),
            ["id", "created_at", "version", "event_time", "missing_type"]
                .map(String::from)
                .into()
        );
    }

    #[test]
    fn familiar_names_do_not_bypass_projection_flags() {
        let sources = vec![source(
            value!({"id":{"type":"string", "readable":false}, "version":{"type":"int", "projectable":false}}),
        )];
        for field in ["id", "version", "created_at"] {
            assert!(
                check_field(&sources[0].source.column(field).unwrap(), &sources, false).is_err()
            );
        }
    }

    #[test]
    fn familiar_names_do_not_bypass_expression_flags() {
        let sources = vec![source(value!({
            "created_at":{"type":"string", "filterable":false},
            "version":{"type":"int", "sortable":false}
        }))];
        let path = sources[0].source.column("created_at").unwrap();
        assert!(
            validate_predicate(&Predicate::is_null(Operand::Path(path)), &sources, false).is_err()
        );
        let path = sources[0].source.column("version").unwrap();
        assert!(check_capability(&path, &sources, "sortable").is_err());
    }
}
