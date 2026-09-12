//! Source scope and ordered joins for relational reads.

use crate::sql::{
    CompareOp, FieldPath, Ident, Operand, OrderKey, PlanError, Predicate, Projection,
    ProjectionSource,
};

pub const MAX_READ_SOURCES: usize = 8;
pub const MAX_READ_PREDICATE_NODES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub kind: JoinKind,
    pub collection: Ident,
    pub alias: Ident,
    pub on: Predicate,
}

fn invalid(message: impl Into<String>) -> PlanError {
    PlanError::InvalidSource(message.into())
}

/// Walk without recursion before consumers traverse or canonicalize a predicate.
pub fn predicate_paths(predicate: &Predicate) -> Result<Vec<&FieldPath>, PlanError> {
    let mut stack = vec![(predicate, 1)];
    let mut paths = Vec::new();
    let mut nodes = 0;
    while let Some((node, depth)) = stack.pop() {
        nodes += 1;
        if depth > crate::sql::MAX_PREDICATE_DEPTH || nodes > MAX_READ_PREDICATE_NODES {
            return Err(invalid("read predicate exceeds its complexity budget"));
        }
        let operands: Vec<&Operand> = match node {
            Predicate::And(children) | Predicate::Or(children) => {
                if children.len() > MAX_READ_PREDICATE_NODES {
                    return Err(invalid("read predicate exceeds its complexity budget"));
                }
                stack.extend(children.iter().map(|c| (c, depth + 1)));
                Vec::new()
            }
            Predicate::Not(child) => {
                stack.push((child, depth + 1));
                Vec::new()
            }
            Predicate::Compare { lhs, rhs, .. } => vec![lhs, rhs],
            Predicate::IsNull { operand, .. } => vec![operand],
            Predicate::Membership { lhs, .. } | Predicate::Pattern { lhs, .. } => vec![lhs],
            Predicate::Const(_) => Vec::new(),
        };
        for value in operands {
            match value {
                Operand::Path(path) => paths.push(path),
                Operand::Aggregate(agg) => paths.extend(agg.argument()),
                Operand::Lit(_) => {}
            }
        }
    }
    Ok(paths)
}

fn check_path(path: &FieldPath, sources: &[&Ident], qualified: bool) -> Result<(), PlanError> {
    match path.source() {
        Some(alias) if !sources.contains(&alias) => {
            Err(invalid(format!("unknown read source '{}'", alias.as_str())))
        }
        None if qualified => Err(invalid("joined reads require source-qualified columns")),
        _ => Ok(()),
    }
}

fn connects(predicate: &Predicate, alias: &Ident, prior: &[&Ident]) -> bool {
    match predicate {
        Predicate::And(children) => children.iter().any(|p| connects(p, alias, prior)),
        Predicate::Compare {
            lhs: Operand::Path(a),
            op: CompareOp::Eq,
            rhs: Operand::Path(b),
        } => {
            (a.source() == Some(alias) && b.source().is_some_and(|s| prior.contains(&s)))
                || (b.source() == Some(alias) && a.source().is_some_and(|s| prior.contains(&s)))
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate(
    collection: &Ident,
    alias: Option<&Ident>,
    joins: &[Join],
    projection: &Projection,
    filter: &Predicate,
    having: &Predicate,
    groups: &[FieldPath],
    order: &[OrderKey],
) -> Result<(), PlanError> {
    if joins.len() >= MAX_READ_SOURCES {
        return Err(invalid("read exceeds its source budget"));
    }
    let qualified = !joins.is_empty();
    if qualified && alias.is_none() {
        return Err(invalid("a joined read requires a root alias"));
    }
    let mut sources = vec![alias.unwrap_or(collection)];
    for join in joins {
        if sources.contains(&&join.alias) {
            return Err(invalid("duplicate read source alias"));
        }
        let paths = predicate_paths(&join.on)?;
        if join.on.mentions_aggregate() {
            return Err(PlanError::AggregateOutsideHaving);
        }
        if !connects(&join.on, &join.alias, &sources) {
            return Err(invalid("JOIN ON requires a connecting column equality"));
        }
        sources.push(&join.alias);
        for path in paths {
            check_path(path, &sources, true)?;
        }
    }
    for predicate in [filter, having] {
        for path in predicate_paths(predicate)? {
            check_path(path, &sources, qualified)?;
        }
    }
    for field in projection.fields() {
        match &field.source {
            ProjectionSource::Path(path) => check_path(path, &sources, qualified)?,
            ProjectionSource::Aggregate(agg) => {
                if let Some(path) = agg.argument() {
                    check_path(path, &sources, qualified)?;
                }
            }
            _ if qualified => {
                return Err(invalid(
                    "joined projections require source-qualified columns",
                ))
            }
            _ => {}
        }
    }
    for path in groups.iter().chain(order.iter().map(|key| &key.path)) {
        check_path(path, &sources, qualified)?;
    }
    Ok(())
}
