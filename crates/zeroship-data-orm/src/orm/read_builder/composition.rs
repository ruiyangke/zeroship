use super::*;

/// A relational predicate retaining the origin of every referenced expression.
#[derive(Debug, Clone)]
#[must_use]
pub struct ReadPredicate {
    pub(super) expression: Predicate,
    pub(super) origins: Vec<ReadOrigin>,
}

impl Default for ReadPredicate {
    fn default() -> Self {
        Self::all()
    }
}

impl ReadPredicate {
    pub fn all() -> Self {
        Self::new(Predicate::Const(true), Vec::new())
    }

    pub fn and(self, other: Self) -> Self {
        self.combine(other, true)
    }

    pub fn or(self, other: Self) -> Self {
        self.combine(other, false)
    }

    pub fn negate(self) -> Self {
        Self::new(Predicate::Not(Box::new(self.expression)), self.origins)
    }

    pub(super) fn new(expression: Predicate, origins: Vec<ReadOrigin>) -> Self {
        Self {
            expression,
            origins,
        }
    }

    fn combine(mut self, other: Self, conjunction: bool) -> Self {
        let mut children = match self.expression {
            Predicate::And(children) if conjunction => children,
            Predicate::Or(children) if !conjunction => children,
            child => vec![child],
        };
        match other.expression {
            Predicate::And(nested) if conjunction => children.extend(nested),
            Predicate::Or(nested) if !conjunction => children.extend(nested),
            child => children.push(child),
        }
        self.origins.extend(other.origins);
        Self::new(
            if conjunction {
                Predicate::And(children)
            } else {
                Predicate::Or(children)
            },
            self.origins,
        )
    }
}

impl std::ops::Not for ReadPredicate {
    type Output = Self;
    fn not(self) -> Self {
        self.negate()
    }
}

/// An ordering key tied to its aliased source.
#[derive(Debug, Clone)]
#[must_use]
pub struct ReadOrder {
    pub(super) key: OrderKey,
    pub(super) origin: ReadOrigin,
}

impl ReadOrder {
    pub const fn nulls_first(mut self) -> Self {
        self.key.nulls = NullOrder::First;
        self
    }

    pub const fn nulls_last(mut self) -> Self {
        self.key.nulls = NullOrder::Last;
        self
    }
}

/// A native value or compatible expression in an aliased read.
pub trait IntoReadOperand<S, Kind> {
    #[doc(hidden)]
    fn into_read_operand(self) -> Result<ReadOperand, DbError>;
}

#[doc(hidden)]
#[derive(Debug)]
pub struct ReadOperand(ReadOperandValue);
#[derive(Debug)]
enum ReadOperandValue {
    Value(Value),
    Expression(Operand, Vec<ReadOrigin>),
}
impl<S, T: EncodeValue<S>> IntoReadOperand<S, ValueOperand> for T {
    fn into_read_operand(self) -> Result<ReadOperand, DbError> {
        self.encode_value()
            .map(|value| ReadOperand(ReadOperandValue::Value(value)))
    }
}
impl ReadOperand {
    pub(super) fn expression(expression: Operand, origins: Vec<ReadOrigin>) -> Self {
        Self(ReadOperandValue::Expression(expression, origins))
    }
    pub(super) fn compare(
        self,
        lhs: Operand,
        op: CompareOp,
        mut origins: Vec<ReadOrigin>,
        encode: impl FnOnce(Value) -> Result<Option<crate::sql::Literal>, DbError>,
    ) -> Result<ReadPredicate, DbError> {
        let rhs = match self.0 {
            ReadOperandValue::Value(value) => encode(value)?.map(Operand::Lit),
            ReadOperandValue::Expression(expression, other) => {
                origins.extend(other);
                Some(expression)
            }
        };
        let expression = match rhs {
            Some(rhs) => Predicate::compare(lhs, op, rhs),
            None if matches!(op, CompareOp::Eq | CompareOp::Ne) => Predicate::IsNull {
                operand: lhs,
                negated: op == CompareOp::Ne,
            },
            None => return Err(read::invalid("null supports only equality comparisons")),
        };
        Ok(ReadPredicate::new(expression, origins))
    }
}
