//! Typed mappings over native records and migration-derived column contracts.
use crate::value::{Record, Value};
use std::marker::PhantomData;
use zeroship_data_orm::error::DbError;

/// Collection metadata generated from the deployment's runtime descriptor.
pub trait Entity: Sized + 'static {
    const COLLECTION: &'static str;
    fn schema() -> &'static Value;
}
pub trait Column: 'static {
    type Entity: Entity;
    type SqlType;
    const NAME: &'static str;
}
pub trait ReadableColumn: Column {}
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
        let value = value
            .encode_value()
            .map_err(|error| field_error::<C>("filter", error))?;
        Ok(Filter {
            predicate: ModelPredicate::Compare {
                field: C::NAME,
                op: crate::sql::CompareOp::Eq,
                value,
            },
            entity: PhantomData,
        })
    }
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
    Compare {
        field: &'static str,
        op: crate::sql::CompareOp,
        value: Value,
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
    fn combine(self, conjunction: bool, other: Self) -> Self {
        Self {
            predicate: if conjunction {
                ModelPredicate::And(vec![self.predicate, other.predicate])
            } else {
                ModelPredicate::Or(vec![self.predicate, other.predicate])
            },
            entity: PhantomData,
        }
    }
    pub(crate) fn into_predicate(self) -> ModelPredicate {
        self.predicate
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
