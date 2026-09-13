use super::*;

impl<C: FilterableColumn> SourceColumn<C> {
    pub fn eq<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Eq, value)
    }
    pub fn ne<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Ne, value)
    }
    fn compare<T: EncodeValue<C::SqlType>>(
        &self,
        op: CompareOp,
        value: T,
    ) -> Result<Predicate, DbError> {
        Ok(match literal::<C>(value.encode_value()?)? {
            Some(literal) => Predicate::Compare {
                lhs: Operand::Path(self.path()),
                op,
                rhs: Operand::Lit(literal),
            },
            None if matches!(op, CompareOp::Eq | CompareOp::Ne) => Predicate::IsNull {
                operand: Operand::Path(self.path()),
                negated: op == CompareOp::Ne,
            },
            None => return Err(read::invalid("null supports only equality comparisons")),
        })
    }
    pub fn in_values<T: EncodeValue<C::SqlType>>(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Predicate, DbError> {
        self.membership(crate::sql::MembershipOp::In, values)
    }
    pub fn not_in_values<T: EncodeValue<C::SqlType>>(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Predicate, DbError> {
        self.membership(crate::sql::MembershipOp::NotIn, values)
    }
    fn membership<T: EncodeValue<C::SqlType>>(
        &self,
        op: crate::sql::MembershipOp,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Predicate, DbError> {
        use crate::sql::descriptors::{supports_predicate_operator, PredicateOperator};
        if !supports_predicate_operator(&C::Entity::schema()[C::NAME], PredicateOperator::Equality)
        {
            return Err(read::invalid(
                "ordinary equality is not supported for this field type",
            ));
        }
        let values = encode_members::<C, T>(values)?;
        if values.is_empty() {
            // Keep the field available to the read's capability checks.
            return Ok(if op == crate::sql::MembershipOp::In {
                Predicate::And(vec![self.is_null(), Predicate::Const(false)])
            } else {
                Predicate::Or(vec![self.is_null(), Predicate::Const(true)])
            });
        }
        let members = values
            .into_iter()
            .map(literal::<C>)
            .collect::<Result<_, _>>()?;
        Predicate::membership(Operand::Path(self.path()), op, members)
            .map_err(|error| read::invalid(error.to_string()))
    }
    pub fn is_null(&self) -> Predicate {
        Predicate::IsNull {
            operand: Operand::Path(self.path()),
            negated: false,
        }
    }
    pub fn is_not_null(&self) -> Predicate {
        Predicate::IsNull {
            operand: Operand::Path(self.path()),
            negated: true,
        }
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

impl<C: FilterableColumn> SourceColumn<C>
where
    C::SqlType: OrderedSqlType,
{
    pub fn lt<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Lt, value)
    }
    pub fn lte<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Lte, value)
    }
    pub fn gt<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Gt, value)
    }
    pub fn gte<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Gte, value)
    }
}

fn literal<C: Column>(mut value: Value) -> Result<Option<crate::sql::Literal>, DbError> {
    if !value.is_null()
        && !matches!(value, Value::Json(_))
        && matches!(
            C::Entity::schema()[C::NAME]["type"].as_str(),
            Some("json" | "object" | "array" | "union")
        )
    {
        value = Value::Json(
            serde_json::to_string(&value).map_err(|error| read::invalid(error.to_string()))?,
        );
    }
    crate::sql::Literal::try_from_value(value).map_err(|error| read::invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Vectors;
    impl Entity for Vectors {
        const COLLECTION: &'static str = "vectors";
        fn schema() -> &'static Value {
            static SCHEMA: std::sync::LazyLock<Value> = std::sync::LazyLock::new(
                || crate::value!({"embedding":{"type":"vector", "vectorDims":2}}),
            );
            &SCHEMA
        }
    }
    struct Embedding;
    impl Column for Embedding {
        type Entity = Vectors;
        type SqlType = sql_types::Nullable<sql_types::Vector>;
        const NAME: &'static str = "embedding";
    }
    impl FilterableColumn for Embedding {}

    #[test]
    fn membership_checks_equality_before_empty_and_null_simplification() {
        let column = SourceColumn::<Embedding> {
            source: ReadSource::new("vectors", "v"),
            origin: ReadOrigin {
                database: Rc::new(()),
                scope: None,
                source: ReadSource::new("vectors", "v"),
                schema: std::sync::Arc::new(Vectors::schema().clone()),
            },
            entity: PhantomData,
        };
        for values in [vec![], vec![None], vec![Some(vec![1.0_f32, 2.0])]] {
            assert!(column.in_values(values.clone()).is_err());
            assert!(column.not_in_values(values).is_err());
        }
    }
}
