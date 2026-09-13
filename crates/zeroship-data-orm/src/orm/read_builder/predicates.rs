use super::*;

impl<C: FilterableColumn> SourceColumn<C> {
    fn predicate(&self, expression: Predicate) -> ReadPredicate {
        ReadPredicate::new(expression, vec![self.origin.clone()])
    }
    pub fn eq<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Eq, value)
    }
    pub fn ne<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Ne, value)
    }
    fn compare<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        op: CompareOp,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        value.into_read_operand()?.compare(
            Operand::Path(self.path()),
            op,
            vec![self.origin.clone()],
            literal::<C>,
        )
    }

    pub fn in_values<T: EncodeValue<C::SqlType>>(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<ReadPredicate, DbError> {
        self.membership(crate::sql::MembershipOp::In, values)
    }
    pub fn not_in_values<T: EncodeValue<C::SqlType>>(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<ReadPredicate, DbError> {
        self.membership(crate::sql::MembershipOp::NotIn, values)
    }
    fn membership<T: EncodeValue<C::SqlType>>(
        &self,
        op: crate::sql::MembershipOp,
        values: impl IntoIterator<Item = T>,
    ) -> Result<ReadPredicate, DbError> {
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
                self.is_null().and(ReadPredicate::all().negate())
            } else {
                self.is_null().or(ReadPredicate::all())
            });
        }
        let members = values
            .into_iter()
            .map(literal::<C>)
            .collect::<Result<_, _>>()?;
        let expression = Predicate::membership(Operand::Path(self.path()), op, members)
            .map_err(|error| read::invalid(error.to_string()))?;
        Ok(self.predicate(expression))
    }
    pub fn is_null(&self) -> ReadPredicate {
        self.predicate(Predicate::IsNull {
            operand: Operand::Path(self.path()),
            negated: false,
        })
    }
    pub fn is_not_null(&self) -> ReadPredicate {
        self.predicate(Predicate::IsNull {
            operand: Operand::Path(self.path()),
            negated: true,
        })
    }
}

impl<S, C> IntoReadOperand<S, ExpressionOperand> for SourceColumn<C>
where
    S: ComparableSqlType,
    C: FilterableColumn,
    C::SqlType: ComparableSqlType<Base = S::Base>,
{
    fn into_read_operand(self) -> Result<ReadOperand, DbError> {
        Ok(ReadOperand::expression(
            Operand::Path(self.path()),
            vec![self.origin],
        ))
    }
}
impl<S, C> IntoReadOperand<S, ExpressionOperand> for &SourceColumn<C>
where
    S: ComparableSqlType,
    C: FilterableColumn,
    C::SqlType: ComparableSqlType<Base = S::Base>,
{
    fn into_read_operand(self) -> Result<ReadOperand, DbError> {
        Ok(ReadOperand::expression(
            Operand::Path(self.path()),
            vec![self.origin.clone()],
        ))
    }
}

impl<C: FilterableColumn> SourceColumn<C>
where
    C::SqlType: OrderedSqlType,
{
    pub fn lt<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Lt, value)
    }
    pub fn lte<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Lte, value)
    }
    pub fn gt<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Gt, value)
    }
    pub fn gte<T: IntoReadOperand<C::SqlType, K>, K>(
        &self,
        value: T,
    ) -> Result<ReadPredicate, DbError> {
        self.compare(CompareOp::Gte, value)
    }
}

fn literal<C: Column>(mut value: Value) -> Result<Option<crate::sql::Literal>, DbError> {
    if !value.is_null()
        && !matches!(value, Value::Json(_))
        && C::Entity::schema()[C::NAME].logical_type.is_json()
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
        fn schema() -> &'static crate::schema::CollectionSchema {
            static SCHEMA: std::sync::LazyLock<crate::schema::CollectionSchema> =
                std::sync::LazyLock::new(|| {
                    let mut embedding =
                        crate::schema::ColumnSchema::new(crate::schema::LogicalType::Vector);
                    embedding.vector_dims = Some(2);
                    crate::schema::CollectionSchema::new([("embedding".into(), embedding)])
                });
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
                schema: std::sync::Arc::new(Vectors::schema().fields().clone()),
            },
            entity: PhantomData,
        };
        for values in [vec![], vec![None], vec![Some(vec![1.0_f32, 2.0])]] {
            assert!(column.in_values(values.clone()).is_err());
            assert!(column.not_in_values(values).is_err());
        }
    }
}
