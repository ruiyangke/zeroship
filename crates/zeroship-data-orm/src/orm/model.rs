//! Typed mappings over native records and declared column contracts.
use crate::value::{Record, Value};
use std::marker::PhantomData;
use zeroship_data_orm::error::DbError;

/// Collection metadata generated from native Rust declarations.
pub trait Entity: Sized + 'static {
    const COLLECTION: &'static str;
    fn schema() -> &'static crate::schema::CollectionSchema;
}
pub trait Column: 'static {
    type Entity: Entity;
    type SqlType;
    const NAME: &'static str;
}
pub trait ReadableColumn: Column {}
/// A named forward edge declared by the generated schema.
pub trait Relation: 'static {
    type Source: Entity;
    type Target: Entity;
    const NAME: &'static str;
    const FIELD: &'static str;
    const TARGET_COLUMN: &'static str;
}
pub trait FilterableColumn: Column {}
pub trait WritableColumn: Column {}
pub trait UpdatableColumn: WritableColumn {}
pub trait DefaultableColumn: WritableColumn {}
/// An insert derive implements this for each field it supplies.
pub trait HasColumn<C: Column> {}

pub trait FromRow<E: Entity>: Sized {
    const COLUMNS: &'static [&'static str];
    fn from_row(row: Row) -> Result<Self, DbError>;
}
pub trait Insertable<E: Entity> {
    fn into_record(self) -> Result<Record, DbError>;
}
/// Literal column assignments. The entity API supplies the update operation.
pub trait Changeset<E: Entity> {
    fn into_changes(self) -> Result<Record, DbError>;
}
pub trait EncodeValue<S> {
    fn encode_value(self) -> Result<Value, DbError>;
}
pub trait DecodeValue<S>: Sized {
    fn decode_value(value: Value) -> Result<Self, DbError>;
}

/// An insert field supplies a value or requests its database default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Defaulted<T> {
    #[default]
    Default,
    Value(T),
}
/// Omitting an update field differs from explicitly setting its nullable value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Change<T> {
    #[default]
    Keep,
    Set(T),
}

pub trait DefaultInput<C: Column> {
    fn encode_default(self, record: &mut Record) -> Result<(), DbError>;
}
impl<C: DefaultableColumn, T: EncodeValue<C::SqlType>> DefaultInput<C> for Defaulted<T> {
    fn encode_default(self, record: &mut Record) -> Result<(), DbError> {
        encode_default::<C, T>(record, self)
    }
}
pub trait ChangeInput<C: Column> {
    fn encode_change(self, record: &mut Record) -> Result<(), DbError>;
}
impl<C: UpdatableColumn, T: EncodeValue<C::SqlType>> ChangeInput<C> for Change<T> {
    fn encode_change(self, record: &mut Record) -> Result<(), DbError> {
        encode_change::<C, T>(record, self)
    }
}

#[derive(Debug)]
pub struct Row(Record);
impl Row {
    pub(crate) fn new(fields: Record) -> Self {
        Self(fields)
    }
    /// Move a field into the model using its generated column contract.
    pub fn take<C, T>(&mut self) -> Result<T, DbError>
    where
        C: ReadableColumn,
        T: DecodeValue<C::SqlType>,
    {
        let value = self.0.swap_remove(C::NAME).ok_or_else(|| {
            DbError::validation(
                "model_decode_failed",
                format!("{}.{}: missing field", C::Entity::COLLECTION, C::NAME),
            )
        })?;
        T::decode_value(value).map_err(|error| field_error::<C>("decode", error))
    }
}
fn field_error<C: Column>(operation: &str, error: DbError) -> DbError {
    DbError::validation(
        "model_value_failed",
        format!(
            "{}.{}: {operation}: {error}",
            C::Entity::COLLECTION,
            C::NAME
        ),
    )
}
pub fn encode_field<C, T>(record: &mut Record, value: T) -> Result<(), DbError>
where
    C: WritableColumn,
    T: EncodeValue<C::SqlType>,
{
    let value = value
        .encode_value()
        .map_err(|error| field_error::<C>("encode", error))?;
    record.insert(C::NAME.into(), value);
    Ok(())
}
/// Used by insert fields carrying `#[orm(default)]`.
pub fn encode_default<C, T>(record: &mut Record, value: Defaulted<T>) -> Result<(), DbError>
where
    C: DefaultableColumn,
    T: EncodeValue<C::SqlType>,
{
    if let Defaulted::Value(value) = value {
        encode_field::<C, _>(record, value)?;
    }
    Ok(())
}
pub fn encode_change<C, T>(record: &mut Record, value: Change<T>) -> Result<(), DbError>
where
    C: UpdatableColumn,
    T: EncodeValue<C::SqlType>,
{
    if let Change::Set(value) = value {
        encode_field::<C, _>(record, value)?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct Field<C>(PhantomData<fn() -> C>);
impl<C> Copy for Field<C> {}
impl<C> Clone for Field<C> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<C: Column> Default for Field<C> {
    fn default() -> Self {
        Self::new()
    }
}
impl<C: Column> Field<C> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}
impl<C: FilterableColumn> Field<C> {
    pub fn eq<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Eq, value)
    }
    pub fn ne<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Ne, value)
    }
    fn compare<T: EncodeValue<C::SqlType>>(
        self,
        op: crate::sql::CompareOp,
        value: T,
    ) -> Result<Filter<C::Entity>, DbError> {
        let value = value
            .encode_value()
            .map_err(|error| field_error::<C>("filter", error))?;
        Ok(Filter {
            predicate: ModelPredicate::Compare {
                field: C::NAME,
                op,
                value,
            },
            entity: PhantomData,
        })
    }
    pub fn in_values<T: EncodeValue<C::SqlType>>(
        self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Filter<C::Entity>, DbError> {
        self.membership(crate::sql::MembershipOp::In, values)
    }
    pub fn not_in_values<T: EncodeValue<C::SqlType>>(
        self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Filter<C::Entity>, DbError> {
        self.membership(crate::sql::MembershipOp::NotIn, values)
    }
    fn membership<T: EncodeValue<C::SqlType>>(
        self,
        op: crate::sql::MembershipOp,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Filter<C::Entity>, DbError> {
        Ok(Filter {
            predicate: ModelPredicate::Membership {
                field: C::NAME,
                op,
                values: encode_members::<C, T>(values)?,
            },
            entity: PhantomData,
        })
    }
    pub fn is_null(self) -> Filter<C::Entity> {
        self.null_check(false)
    }
    pub fn is_not_null(self) -> Filter<C::Entity> {
        self.null_check(true)
    }
    fn null_check(self, negated: bool) -> Filter<C::Entity> {
        Filter {
            predicate: ModelPredicate::IsNull {
                field: C::NAME,
                negated,
            },
            entity: PhantomData,
        }
    }
}

/// Logical types supporting portable ordered comparisons.
pub trait OrderedSqlType {}
macro_rules! ordered_types {
    ($($sql:ident),* $(,)?) => { $(impl OrderedSqlType for super::sql_types::$sql {})* };
}
ordered_types!(Text, Integer, BigInt, Number, Timestamp, CalendarDate, Time);
impl<T: OrderedSqlType> OrderedSqlType for super::sql_types::Nullable<T> {}

impl<C: FilterableColumn> Field<C>
where
    C::SqlType: OrderedSqlType,
{
    pub fn lt<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Lt, value)
    }
    pub fn lte<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Lte, value)
    }
    pub fn gt<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Gt, value)
    }
    pub fn gte<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Filter<C::Entity>, DbError> {
        self.compare(crate::sql::CompareOp::Gte, value)
    }
}

pub(crate) fn encode_members<C: Column, T: EncodeValue<C::SqlType>>(
    values: impl IntoIterator<Item = T>,
) -> Result<Vec<Value>, DbError> {
    let mut encoded = Vec::new();
    for value in values {
        if encoded.len() == crate::sql::MAX_MEMBERSHIP_LIST_LEN {
            return Err(DbError::validation(
                "invalid_filter",
                "membership exceeds its value budget",
            ));
        }
        encoded.push(
            value
                .encode_value()
                .map_err(|error| field_error::<C>("filter", error))?,
        );
    }
    Ok(encoded)
}
impl<C: UpdatableColumn> Field<C> {
    pub fn set<T: EncodeValue<C::SqlType>>(self, value: T) -> Result<Patch<C::Entity>, DbError> {
        let mut fields = Record::new();
        encode_field::<C, _>(&mut fields, value)?;
        Ok(Patch {
            fields,
            entity: PhantomData,
        })
    }
}

#[derive(Debug)]
pub struct Filter<E> {
    predicate: ModelPredicate,
    entity: PhantomData<fn() -> E>,
}

#[derive(Debug, Clone)]
pub(crate) enum ModelPredicate {
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    Compare {
        field: &'static str,
        op: crate::sql::CompareOp,
        value: Value,
    },
    Membership {
        field: &'static str,
        op: crate::sql::MembershipOp,
        values: Vec<Value>,
    },
    IsNull {
        field: &'static str,
        negated: bool,
    },
    Const(bool),
}
impl<E> Default for Filter<E> {
    fn default() -> Self {
        Self::all()
    }
}
impl<E> Filter<E> {
    pub fn all() -> Self {
        Self {
            predicate: ModelPredicate::Const(true),
            entity: PhantomData,
        }
    }
    pub fn and(self, other: Self) -> Self {
        self.combine(true, other)
    }
    pub fn or(self, other: Self) -> Self {
        self.combine(false, other)
    }
    pub fn negate(self) -> Self {
        Self {
            predicate: ModelPredicate::Not(Box::new(self.predicate)),
            entity: PhantomData,
        }
    }
    fn combine(self, conjunction: bool, other: Self) -> Self {
        let mut children = match self.predicate {
            ModelPredicate::And(children) if conjunction => children,
            ModelPredicate::Or(children) if !conjunction => children,
            child => vec![child],
        };
        match other.predicate {
            ModelPredicate::And(nested) if conjunction => children.extend(nested),
            ModelPredicate::Or(nested) if !conjunction => children.extend(nested),
            child => children.push(child),
        }
        Self {
            predicate: if conjunction {
                ModelPredicate::And(children)
            } else {
                ModelPredicate::Or(children)
            },
            entity: PhantomData,
        }
    }
    pub(crate) fn into_predicate(self) -> ModelPredicate {
        self.predicate
    }
}
impl<E> std::ops::Not for Filter<E> {
    type Output = Self;
    fn not(self) -> Self {
        self.negate()
    }
}
#[derive(Debug)]
pub struct Patch<E> {
    fields: Record,
    entity: PhantomData<fn() -> E>,
}
impl<E> Patch<E> {
    /// Combine assignments to distinct columns.
    ///
    /// # Errors
    /// Refuses duplicate columns before either patch can be executed.
    pub fn and(mut self, other: Self) -> Result<Self, DbError> {
        if other
            .fields
            .keys()
            .any(|field| self.fields.contains_key(field))
        {
            return Err(DbError::validation(
                "invalid_update",
                "a field may be assigned only once per update",
            ));
        }
        self.fields.extend(other.fields);
        Ok(self)
    }
}
impl<E: Entity> Changeset<E> for Patch<E> {
    fn into_changes(self) -> Result<Record, DbError> {
        Ok(self.fields)
    }
}
#[derive(Default, Debug)]
pub struct FindOptions {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub include_deleted: bool,
}
