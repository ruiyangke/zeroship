use crate::sql::{FieldPath, Operand, Predicate, ReadError};

pub const MAX_READ_SOURCES: usize = 8;
pub const MAX_READ_PREDICATE_NODES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

pub(crate) fn predicate_paths(predicate: &Predicate) -> Result<Vec<&FieldPath>, ReadError> {
    let mut stack = vec![(predicate, 1)];
    let mut paths = Vec::new();
    let mut nodes = 0;
    while let Some((node, depth)) = stack.pop() {
        nodes += 1;
        if depth > crate::sql::MAX_PREDICATE_DEPTH || nodes > MAX_READ_PREDICATE_NODES {
            return Err(ReadError::PredicateTooComplex);
        }
        let operands: Vec<&Operand> = match node {
            Predicate::And(children) | Predicate::Or(children) => {
                if children.len() > MAX_READ_PREDICATE_NODES {
                    return Err(ReadError::PredicateTooComplex);
                }
                stack.extend(children.iter().map(|child| (child, depth + 1)));
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
        for operand in operands {
            match operand {
                Operand::Path(path) => paths.push(path),
                Operand::Aggregate(aggregate) => paths.extend(aggregate.argument()),
                Operand::Lit(_) => {}
            }
        }
    }
    Ok(paths)
}
