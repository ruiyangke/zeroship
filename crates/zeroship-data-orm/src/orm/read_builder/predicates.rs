use super::*;

impl<C: FilterableColumn> SourceColumn<C> {
    pub fn eq<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        let value = value.encode_value()?;
        let literal = crate::sql::Literal::try_from_value(value)
            .map_err(|error| read::invalid(error.to_string()))?;
        Ok(match literal {
            Some(literal) => Predicate::Compare {
                lhs: Operand::Path(self.path()),
                op: CompareOp::Eq,
                rhs: Operand::Lit(literal),
            },
            None => Predicate::IsNull {
                operand: Operand::Path(self.path()),
                negated: false,
            },
        })
    }
    pub fn eq_column<D: FilterableColumn>(
        &self,
        other: SourceColumn<D>,
    ) -> Result<Predicate, DbError>
    where
        C::SqlType: JoinType,
        D::SqlType: JoinType<Base = <C::SqlType as JoinType>::Base>,
    {
        Ok(Predicate::compare(
            Operand::Path(self.path()),
            CompareOp::Eq,
            Operand::Path(other.path()),
        ))
    }
}

/// Comparable scalar types, independent of column nullability.
pub trait JoinType {
    type Base;
}
macro_rules! join_types { ($($t:ident),* $(,)?) => { $(impl JoinType for sql_types::$t { type Base = sql_types::$t; })* }; }
join_types!(
    Text,
    Integer,
    BigInt,
    Number,
    Boolean,
    Bytes,
    Timestamp,
    CalendarDate,
    Time
);
impl<T: JoinType> JoinType for sql_types::Nullable<T> {
    type Base = T::Base;
}

