use super::*;
use crate::sql::{AggregateFunc, AggregateRef};

/// A scalar projection with a native result decoder and typed comparison input.
#[derive(Debug)]
pub struct ScalarSelection<S, R> {
    expression: Operand,
    decode: fn(Value) -> Result<R, DbError>,
    origin: Option<ReadOrigin>,
    sql_type: PhantomData<fn() -> S>,
}

impl<S, R> ScalarSelection<S, R> {
    fn new(
        expression: Operand,
        decode: fn(Value) -> Result<R, DbError>,
        origin: Option<ReadOrigin>,
    ) -> Self {
        Self {
            expression,
            decode,
            origin,
            sql_type: PhantomData,
        }
    }

    fn compare<T: EncodeValue<S>>(&self, op: CompareOp, value: T) -> Result<Predicate, DbError> {
        let literal = crate::sql::Literal::try_from_value(value.encode_value()?)
            .map_err(|error| read::invalid(error.to_string()))?;
        match literal {
            Some(literal) => Ok(Predicate::compare(
                self.expression.clone(),
                op,
                Operand::Lit(literal),
            )),
            None if matches!(op, CompareOp::Eq | CompareOp::Ne) => Ok(Predicate::IsNull {
                operand: self.expression.clone(),
                negated: op == CompareOp::Ne,
            }),
            None => Err(read::invalid("null supports only equality comparisons")),
        }
    }

    pub fn eq<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Eq, value)
    }

    pub fn ne<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Ne, value)
    }

    pub fn is_null(&self) -> Predicate {
        Predicate::IsNull {
            operand: self.expression.clone(),
            negated: false,
        }
    }

    pub fn is_not_null(&self) -> Predicate {
        Predicate::IsNull {
            operand: self.expression.clone(),
            negated: true,
        }
    }
}

impl<S: OrderedSqlType, R> ScalarSelection<S, R> {
    pub fn lt<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Lt, value)
    }

    pub fn lte<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Lte, value)
    }

    pub fn gt<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Gt, value)
    }

    pub fn gte<T: EncodeValue<S>>(&self, value: T) -> Result<Predicate, DbError> {
        self.compare(CompareOp::Gte, value)
    }
}

impl<S, R> ReadSelection for ScalarSelection<S, R> {
    type Output = R;

    fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError> {
        match &self.origin {
            Some(origin) => origin.validate(database, sources),
            None => Ok(()),
        }
    }

    fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>) {
        output.push(ReadProjection::Scalar {
            output: format!("p{next}"),
            expression: self.expression.clone(),
        });
        *next += 1;
    }

    fn decode(&self, next: &mut usize, row: &mut Record) -> Result<R, DbError> {
        (self.decode)(take(next, row)?)
    }
}

pub fn count_rows() -> ScalarSelection<sql_types::BigInt, i64> {
    ScalarSelection::new(
        Operand::Aggregate(AggregateRef::count_rows()),
        <i64 as DecodeValue<sql_types::BigInt>>::decode_value,
        None,
    )
}

impl<C: ReadableColumn> SourceColumn<C> {
    pub fn select<R: DecodeValue<C::SqlType>>(&self) -> ScalarSelection<C::SqlType, R> {
        ScalarSelection::new(
            Operand::Path(self.path()),
            R::decode_value,
            Some(self.origin.clone()),
        )
    }

    /// Decode a possibly absent joined column as an optional scalar.
    pub fn select_optional<R: DecodeValue<C::SqlType>>(
        &self,
    ) -> ScalarSelection<C::SqlType, Option<R>> {
        ScalarSelection::new(
            Operand::Path(self.path()),
            <Option<R> as DecodeValue<sql_types::Nullable<C::SqlType>>>::decode_value,
            Some(self.origin.clone()),
        )
    }

    fn aggregate(&self, function: AggregateFunc, distinct: bool) -> Operand {
        Operand::Aggregate(
            AggregateRef::over_path(function, self.path(), distinct)
                .expect("typed aggregate constructors only request supported distinct functions"),
        )
    }

    pub fn count(&self) -> ScalarSelection<sql_types::BigInt, i64> {
        ScalarSelection::new(
            self.aggregate(AggregateFunc::Count, false),
            <i64 as DecodeValue<sql_types::BigInt>>::decode_value,
            Some(self.origin.clone()),
        )
    }

    pub fn count_distinct(&self) -> ScalarSelection<sql_types::BigInt, i64> {
        ScalarSelection::new(
            self.aggregate(AggregateFunc::Count, true),
            <i64 as DecodeValue<sql_types::BigInt>>::decode_value,
            Some(self.origin.clone()),
        )
    }
}

/// Portable sum result types for generated numeric columns.
pub trait NumericAggregate {
    type SumSql;
    type SumValue: DecodeValue<Self::SumSql>;
}

impl NumericAggregate for sql_types::Integer {
    type SumSql = sql_types::BigInt;
    type SumValue = i64;
}
impl NumericAggregate for sql_types::BigInt {
    type SumSql = sql_types::BigInt;
    type SumValue = i64;
}
impl NumericAggregate for sql_types::Number {
    type SumSql = sql_types::Number;
    type SumValue = f64;
}
impl<S: NumericAggregate> NumericAggregate for sql_types::Nullable<S> {
    type SumSql = S::SumSql;
    type SumValue = S::SumValue;
}

impl<C: ReadableColumn> SourceColumn<C>
where
    C::SqlType: NumericAggregate,
{
    pub fn sum(
        &self,
    ) -> ScalarSelection<
        <C::SqlType as NumericAggregate>::SumSql,
        Option<<C::SqlType as NumericAggregate>::SumValue>,
    > {
        ScalarSelection::new(
            self.aggregate(AggregateFunc::Sum, false),
            <Option<<C::SqlType as NumericAggregate>::SumValue> as DecodeValue<
                sql_types::Nullable<<C::SqlType as NumericAggregate>::SumSql>,
            >>::decode_value,
            Some(self.origin.clone()),
        )
    }

    pub fn avg(&self) -> ScalarSelection<sql_types::Number, Option<f64>> {
        ScalarSelection::new(
            self.aggregate(AggregateFunc::Avg, false),
            <Option<f64> as DecodeValue<sql_types::Nullable<sql_types::Number>>>::decode_value,
            Some(self.origin.clone()),
        )
    }
}

impl<C: ReadableColumn> SourceColumn<C>
where
    C::SqlType: OrderedSqlType + JoinType,
{
    pub fn min<R: DecodeValue<<C::SqlType as JoinType>::Base>>(
        &self,
    ) -> ScalarSelection<<C::SqlType as JoinType>::Base, Option<R>> {
        ScalarSelection::new(self.aggregate(AggregateFunc::Min, false),
            <Option<R> as DecodeValue<sql_types::Nullable<<C::SqlType as JoinType>::Base>>>::decode_value,
            Some(self.origin.clone()))
    }

    pub fn max<R: DecodeValue<<C::SqlType as JoinType>::Base>>(
        &self,
    ) -> ScalarSelection<<C::SqlType as JoinType>::Base, Option<R>> {
        ScalarSelection::new(self.aggregate(AggregateFunc::Max, false),
            <Option<R> as DecodeValue<sql_types::Nullable<<C::SqlType as JoinType>::Base>>>::decode_value,
            Some(self.origin.clone()))
    }
}
