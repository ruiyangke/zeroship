//! Descriptor-aware rendering shared by ordinary and joined SELECT plans.

use super::*;
use crate::sql::{AggregateRef, FieldPath, Ident, Operand, ProjectionSource, Select};

#[derive(Debug, Clone, Copy)]
pub struct ReadSource<'a> {
    pub alias: Option<&'a Ident>,
    pub schema: &'a Value,
}

pub(super) fn resolve<'a, 'p>(
    path: &'p FieldPath,
    sources: &[ReadSource<'a>],
) -> Result<(&'p str, &'a Value), QueryError> {
    if path.is_nested() {
        return Err(invalid("nested read paths are not supported"));
    }
    let source = sources
        .iter()
        .find(|s| s.alias == path.source())
        .ok_or_else(|| invalid("unknown read source"))?;
    let field = path.root().as_str();
    Ok((field, source.schema))
}

pub(super) fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

pub(super) fn column_sql(
    path: &FieldPath,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
    value_operation: bool,
) -> Result<String, QueryError> {
    let (field, schema) = resolve(path, sources)?;
    if value_operation {
        validate_value_operation(field, schema)?;
    }
    let physical = value_column_for_field(field, schema);
    let qualified = qualified_read_field(path.source().map(Ident::as_str), &physical);
    if value_operation
        && dialect == SqlDialect::Sqlite
        && schema
            .get(field)
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            == Some("decimal")
    {
        return Ok(format!("CAST({qualified} AS NUMERIC)"));
    }
    Ok(qualified)
}

pub(super) fn aggregate_sql(
    agg: &AggregateRef,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    let argument = match agg.argument() {
        Some(path) => {
            let (field, schema) = resolve(path, sources)?;
            if column_is_masked(field, schema) {
                return Err(invalid("protected fields cannot be aggregated"));
            }
            column_sql(path, sources, dialect, true)?
        }
        None => "*".into(),
    };
    Ok(format!(
        "{}({}{argument})",
        agg.func().as_sql(),
        if agg.is_distinct() { "DISTINCT " } else { "" }
    ))
}

pub(super) fn operand_sql(
    operand: &Operand,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    match operand {
        Operand::Path(path) => column_sql(path, sources, dialect, true),
        Operand::Aggregate(agg) => aggregate_sql(agg, sources, dialect),
        Operand::Lit(_) => Err(invalid("expected a column or aggregate operand")),
    }
}

pub(super) fn bind_operand(
    params: &mut Vec<Value>,
    value: &crate::sql::Literal,
    operand: &Operand,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    match operand {
        Operand::Path(path) => {
            let (field, schema) = resolve(path, sources)?;
            push_field_value_bind(params, &filter_literal(value)?, field, schema, dialect)
        }
        Operand::Aggregate(_) => {
            params.push(filter_literal(value)?);
            Ok(format!("${}", params.len()))
        }
        Operand::Lit(_) => Err(invalid("comparison requires a column or aggregate")),
    }
}

/// Render an already validated relational plan using every source's descriptor.
pub fn build_select(
    plan: &Select,
    namespace: &SchemaName,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    if sources.len() != plan.joins().len() + 1
        || sources.first().map(|s| s.alias) != Some(plan.alias())
    {
        return Err(invalid("SELECT descriptors do not match its sources"));
    }
    for (join, source) in plan.joins().iter().zip(&sources[1..]) {
        if source.alias != Some(&join.alias) {
            return Err(invalid("JOIN descriptor does not match its source"));
        }
    }
    let projected = plan
        .projection()
        .fields()
        .iter()
        .map(|field| {
            let expression = match &field.source {
                ProjectionSource::Column(column) => {
                    column_sql(&FieldPath::column(column.clone()), sources, dialect, false)?
                }
                ProjectionSource::Path(path) => column_sql(path, sources, dialect, false)?,
                ProjectionSource::Aggregate(agg) => aggregate_sql(agg, sources, dialect)?,
                _ => return Err(invalid("unsupported SELECT projection")),
            };
            Ok(format!(
                "{expression} AS {}",
                quote_ident(field.alias.as_str())
            ))
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    let mut params = Vec::new();
    let mut sql = format!(
        "SELECT {}{} FROM {}.{}",
        if plan.is_distinct() { "DISTINCT " } else { "" },
        projected.join(", "),
        crate::sql::compile::quote_ident(namespace.as_str()),
        quote_ident(plan.collection().as_str())
    );
    if let Some(alias) = plan.alias() {
        sql.push_str(&format!(" AS {}", quote_ident(alias.as_str())));
    }
    for join in plan.joins() {
        sql.push_str(&format!(
            " {} JOIN {}.{} AS {} ON {}",
            match join.kind {
                crate::sql::JoinKind::Inner => "INNER",
                crate::sql::JoinKind::Left => "LEFT",
            },
            crate::sql::compile::quote_ident(namespace.as_str()),
            quote_ident(join.collection.as_str()),
            quote_ident(join.alias.as_str()),
            render_filter(&join.on, &mut params, sources, dialect, false)?
        ));
    }
    if plan.filter() != &crate::sql::Predicate::always() {
        sql.push_str(" WHERE ");
        sql.push_str(&render_filter(
            plan.filter(),
            &mut params,
            sources,
            dialect,
            true,
        )?);
    }
    if !plan.group_by().is_empty() {
        sql.push_str(" GROUP BY ");
        sql.push_str(
            &plan
                .group_by()
                .iter()
                .map(|p| column_sql(p, sources, dialect, true))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "),
        );
    }
    if plan.having() != &crate::sql::Predicate::always() {
        sql.push_str(" HAVING ");
        sql.push_str(&render_filter(
            plan.having(),
            &mut params,
            sources,
            dialect,
            true,
        )?);
    }
    if !plan.order_by().is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(
            &plan
                .order_by()
                .iter()
                .map(|key| {
                    Ok(format!(
                        "{} {} NULLS {}",
                        column_sql(&key.path, sources, dialect, true)?,
                        match key.direction {
                            crate::sql::Direction::Ascending => "ASC",
                            crate::sql::Direction::Descending => "DESC",
                        },
                        match key.nulls {
                            crate::sql::NullOrder::First => "FIRST",
                            crate::sql::NullOrder::Last => "LAST",
                        }
                    ))
                })
                .collect::<Result<Vec<_>, QueryError>>()?
                .join(", "),
        );
    }
    params.push(Value::from(plan.limit().get()));
    sql.push_str(&format!(" LIMIT ${}", params.len()));
    params.push(Value::from(plan.offset().get()));
    sql.push_str(&format!(" OFFSET ${}", params.len()));
    if params.len() > 32766 {
        return Err(invalid("SELECT exceeds its parameter budget"));
    }
    Ok(BuiltQuery { sql, params })
}
