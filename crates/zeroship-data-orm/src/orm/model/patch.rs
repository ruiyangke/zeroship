use super::{
    Changeset, Column, DbError, EncodeValue, Entity, Field, PhantomData, Record, UpdatableColumn,
    Value, field_error,
};
use crate::sql::update::Operator;
use indexmap::IndexMap;

/// An array column with a declared element codec.
pub trait ArrayColumn: Column {
    type ItemSqlType;
}

/// Portable arithmetic operand types, independent of column nullability.
pub trait NumericSqlType {
    type Operand;
}
macro_rules! numeric_types {
    ($($t:ident),* $(,)?) => { $(impl NumericSqlType for super::super::sql_types::$t { type Operand = super::super::sql_types::$t; })* };
}
numeric_types!(Integer, BigInt, Number);
impl<S: NumericSqlType> NumericSqlType for super::super::sql_types::Nullable<S> {
    type Operand = S::Operand;
}

#[derive(Debug)]
pub struct Patch<E> {
    assignments: IndexMap<String, PatchAssignment>,
    entity: PhantomData<fn() -> E>,
}

#[derive(Debug)]
enum PatchAssignment {
    Value(Operator, Value),
    Timestamp(super::super::TimestampExpr),
}
impl<E> Patch<E> {
    #[doc(hidden)]
    #[must_use]
    pub fn from_assignments(fields: Record) -> Self {
        Self {
            assignments: fields
                .into_iter()
                .map(|(field, value)| (field, PatchAssignment::Value(Operator::Set, value)))
                .collect(),
            entity: PhantomData,
        }
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }

    pub(super) fn operation<C: Column<Entity = E>>(operator: Operator, value: Value) -> Self {
        Self {
            assignments: [(C::NAME.into(), PatchAssignment::Value(operator, value))].into(),
            entity: PhantomData,
        }
    }

    pub(in crate::orm) fn into_update(self) -> crate::crud::update::Input {
        let mut sets = Record::new();
        let mut update = Record::new();
        let mut expressions = IndexMap::new();
        for (field, assignment) in self.assignments {
            let (operator, operand) = match assignment {
                PatchAssignment::Timestamp(expression) => {
                    expressions.insert(field, expression);
                    continue;
                }
                PatchAssignment::Value(operator, operand) => (operator, operand),
            };
            if operator == Operator::Set {
                sets.insert(field, operand);
            } else {
                update.insert(
                    field,
                    Value::Object([(operator.name().into(), operand)].into()),
                );
            }
        }
        if !sets.is_empty() {
            update.insert("$set".into(), Value::Object(sets));
        }
        crate::crud::update::Input {
            values: Value::Object(update),
            expressions,
        }
    }
}

/// Timestamp columns, including nullable timestamps, accept database expressions.
pub trait TimestampSqlType {}
impl TimestampSqlType for super::super::sql_types::Timestamp {}
impl TimestampSqlType for super::super::sql_types::Nullable<super::super::sql_types::Timestamp> {}

impl<C: UpdatableColumn> Field<C>
where
    C::SqlType: TimestampSqlType,
{
    pub fn set_expression(
        self,
        expression: super::super::TimestampExpr,
    ) -> Result<Patch<C::Entity>, DbError> {
        Ok(Patch {
            assignments: [(C::NAME.into(), PatchAssignment::Timestamp(expression))].into(),
            entity: PhantomData,
        })
    }
}
impl<E: Entity> Patch<E> {
    /// Combine changes to distinct columns, refusing duplicate assignments.
    ///
    /// # Errors
    /// Refuses invalid operands and duplicate column assignments.
    pub fn and<C: Changeset<E>>(mut self, other: C) -> Result<Self, DbError> {
        let other = other.into_changes()?;
        if other
            .assignments
            .keys()
            .any(|field| self.assignments.contains_key(field))
        {
            return Err(DbError::validation(
                "invalid_update",
                "a field may be assigned only once per update",
            ));
        }
        self.assignments.extend(other.assignments);
        Ok(self)
    }
}
impl<E: Entity> Changeset<E> for Patch<E> {
    fn into_changes(self) -> Result<Self, DbError> {
        Ok(self)
    }
}

impl<C: UpdatableColumn> Field<C> {
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn set<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Patch<C::Entity>, DbError> {
        let value = value
            .encode_value()
            .map_err(|error| field_error::<C>("encode", error))?;
        Ok(Patch::operation::<C>(Operator::Set, value))
    }
}
impl<C: UpdatableColumn> Field<C>
where
    C::SqlType: NumericSqlType,
{
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn increment<T: EncodeValue<<C::SqlType as NumericSqlType>::Operand>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::arithmetic(Operator::Increment, value)
    }
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn decrement<T: EncodeValue<<C::SqlType as NumericSqlType>::Operand>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::arithmetic(Operator::Decrement, value)
    }
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn multiply<T: EncodeValue<<C::SqlType as NumericSqlType>::Operand>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::arithmetic(Operator::Multiply, value)
    }
    fn arithmetic<T: EncodeValue<<C::SqlType as NumericSqlType>::Operand>>(
        op: Operator,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        let value = value
            .encode_value()
            .map_err(|error| field_error::<C>("encode", error))?;
        Ok(Patch::operation::<C>(op, value))
    }
}
impl<C: UpdatableColumn + ArrayColumn> Field<C> {
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn push<T: EncodeValue<C::ItemSqlType>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::array(Operator::Push, value)
    }
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn pull<T: EncodeValue<C::ItemSqlType>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::array(Operator::Pull, value)
    }
    /// # Errors
    /// Refuses values that cannot be encoded for this operand.
    pub fn add_to_set<T: EncodeValue<C::ItemSqlType>>(
        self,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        Self::array(Operator::AddToSet, value)
    }
    fn array<T: EncodeValue<C::ItemSqlType>>(
        op: Operator,
        value: T,
    ) -> Result<Patch<C::Entity>, DbError> {
        let value = value
            .encode_value()
            .map_err(|error| field_error::<C>("encode", error))?;
        Ok(Patch::operation::<C>(op, value))
    }
}
