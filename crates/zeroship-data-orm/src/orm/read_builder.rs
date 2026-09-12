//! Typed Rust aliases and model projections over the shared read operation.
use super::*;
use crate::sql::{
    CompareOp, Direction, FieldPath, JoinKind, NullOrder, Operand, OrderKey, Predicate, RowLimit,
};

#[derive(Debug)]
pub struct EntityAlias<E: Entity> {
    database: Database,
    source: ReadSource,
    entity: PhantomData<fn() -> E>,
}
impl<E: Entity> EntityCollection<E> {
    pub fn alias(&self, alias: &str) -> Result<EntityAlias<E>, DbError> {
        read::ident(alias, crate::sql::IdentRole::Alias)?;
        Ok(EntityAlias {
            database: self.collection.database.clone(),
            source: ReadSource::new(E::COLLECTION, alias),
            entity: PhantomData,
        })
    }
}
impl<E: Entity> EntityAlias<E> {
    pub fn column<C: Column<Entity = E>>(&self, _: Field<C>) -> SourceColumn<C> {
        SourceColumn {
            source: self.source.clone(),
            entity: PhantomData,
        }
    }
    pub fn row<R: FromRow<E>>(&self) -> EntityProjection<E, R, false> {
        EntityProjection {
            source: self.source.alias.clone(),
            entity: PhantomData,
        }
    }
    pub fn optional_row<R: FromRow<E>>(&self) -> EntityProjection<E, R, true> {
        EntityProjection {
            source: self.source.alias.clone(),
            entity: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct SourceColumn<C: Column> {
    source: ReadSource,
    entity: PhantomData<fn() -> C>,
}
impl<C: Column> SourceColumn<C> {
    fn path(&self) -> FieldPath {
        self.source
            .column(C::NAME)
            .expect("generated column and validated alias")
    }
    pub fn asc(&self) -> OrderKey {
        OrderKey {
            path: self.path(),
            direction: Direction::Ascending,
            nulls: NullOrder::Last,
        }
    }
    pub fn desc(&self) -> OrderKey {
        OrderKey {
            path: self.path(),
            direction: Direction::Descending,
            nulls: NullOrder::First,
        }
    }
}
impl<C: FilterableColumn> SourceColumn<C> {
    pub fn eq<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.comparison("$eq", value)
    }
    pub fn gt<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.comparison("$gt", value)
    }
    pub fn gte<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.comparison("$gte", value)
    }
    pub fn lte<T: EncodeValue<C::SqlType>>(&self, value: T) -> Result<Predicate, DbError> {
        self.comparison("$lte", value)
    }
    fn comparison<T: EncodeValue<C::SqlType>>(
        &self,
        operator: &str,
        value: T,
    ) -> Result<Predicate, DbError> {
        let filter = crate::sql::filter::decode(&Value::Object(
            [(
                C::NAME.into(),
                Value::Object([(operator.into(), value.encode_value()?)].into()),
            )]
            .into(),
        ))?;
        let Predicate::And(mut children) = filter else {
            return Err(read::invalid("invalid comparison"));
        };
        let predicate = children
            .pop()
            .ok_or_else(|| read::invalid("missing comparison"))?;
        Ok(match predicate {
            Predicate::Compare { op, rhs, .. } => Predicate::Compare {
                lhs: Operand::Path(self.path()),
                op,
                rhs,
            },
            Predicate::IsNull { negated, .. } => Predicate::IsNull {
                operand: Operand::Path(self.path()),
                negated,
            },
            _ => return Err(read::invalid("invalid comparison")),
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

#[derive(Debug)]
pub struct EntityProjection<E: Entity, R, const OPTIONAL: bool> {
    source: String,
    entity: PhantomData<fn() -> (E, R)>,
}

pub trait ReadSelection {
    type Output;
    fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>);
    fn decode(&self, next: &mut usize, row: &mut Record) -> Result<Self::Output, DbError>;
}
fn take(next: &mut usize, row: &mut Record) -> Result<Value, DbError> {
    let name = format!("p{next}");
    *next += 1;
    row.swap_remove(&name)
        .ok_or_else(|| DbError::internal("missing model projection"))
}
fn project<E: Entity, R: FromRow<E>>(
    source: &str,
    optional: bool,
    next: &mut usize,
    output: &mut Vec<ReadProjection>,
) {
    output.push(ReadProjection::Row {
        output: format!("p{next}"),
        source: source.into(),
        fields: Some(R::COLUMNS.iter().map(|s| (*s).into()).collect()),
        optional,
    });
    *next += 1;
}
impl<E: Entity, R: FromRow<E>> ReadSelection for EntityProjection<E, R, false> {
    type Output = R;
    fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>) {
        project::<E, R>(&self.source, false, next, output);
    }
    fn decode(&self, next: &mut usize, row: &mut Record) -> Result<R, DbError> {
        let Value::Object(fields) = take(next, row)? else {
            return Err(read::invalid("required model projection was null"));
        };
        R::from_row(Row::new(fields))
    }
}
impl<E: Entity, R: FromRow<E>> ReadSelection for EntityProjection<E, R, true> {
    type Output = Option<R>;
    fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>) {
        project::<E, R>(&self.source, true, next, output);
    }
    fn decode(&self, next: &mut usize, row: &mut Record) -> Result<Option<R>, DbError> {
        match take(next, row)? {
            Value::Null => Ok(None),
            Value::Object(fields) => Ok(Some(R::from_row(Row::new(fields))?)),
            _ => Err(read::invalid("invalid model projection")),
        }
    }
}
macro_rules! tuple_selection {
    ($($t:ident:$index:tt),+) => {
        impl<$($t: ReadSelection),+> ReadSelection for ($($t,)+) {
            type Output = ($($t::Output,)+);
            fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>) { $(self.$index.projections(next, output);)+ }
            fn decode(&self, next: &mut usize, row: &mut Record) -> Result<Self::Output, DbError> { Ok(($(self.$index.decode(next, row)?,)+)) }
        }
    }
}
tuple_selection!(A:0, B:1);
tuple_selection!(A:0, B:1, C:2);
tuple_selection!(A:0, B:1, C:2, D:3);

#[derive(Debug)]
pub struct ReadBuilder<P = ()> {
    database: Database,
    query: ReadQuery,
    selection: P,
    error: Option<DbError>,
}
impl Database {
    pub fn from<E: Entity>(&self, source: &EntityAlias<E>) -> ReadBuilder {
        ReadBuilder {
            database: self.clone(),
            query: ReadQuery::new(source.source.clone()),
            selection: (),
            error: (!Rc::ptr_eq(&self.identity, &source.database.identity))
                .then(|| read::invalid("read sources must belong to the same database handle")),
        }
    }
}
impl<P> ReadBuilder<P> {
    fn join<E: Entity>(
        mut self,
        source: &EntityAlias<E>,
        on: Predicate,
        kind: JoinKind,
    ) -> Result<Self, DbError> {
        if !Rc::ptr_eq(&self.database.identity, &source.database.identity) {
            return Err(read::invalid(
                "read sources must belong to the same database handle",
            ));
        }
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        self.query.joins.push(ReadJoin {
            kind,
            source: source.source.clone(),
            on,
        });
        Ok(self)
    }
    pub fn left_join<E: Entity>(
        self,
        source: &EntityAlias<E>,
        on: Predicate,
    ) -> Result<Self, DbError> {
        self.join(source, on, JoinKind::Left)
    }
    pub fn inner_join<E: Entity>(
        self,
        source: &EntityAlias<E>,
        on: Predicate,
    ) -> Result<Self, DbError> {
        self.join(source, on, JoinKind::Inner)
    }
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.query.filter = predicate;
        self
    }
    pub fn order_by(mut self, key: OrderKey) -> Self {
        self.query.order_by.push(key);
        self
    }
    pub fn limit(mut self, limit: i64) -> Result<Self, DbError> {
        self.query.limit = RowLimit::new(limit).map_err(|e| read::invalid(e.to_string()))?;
        Ok(self)
    }
    pub fn select<S: ReadSelection>(mut self, selection: S) -> Result<ReadBuilder<S>, DbError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        self.query.projection.clear();
        selection.projections(&mut 0, &mut self.query.projection);
        Ok(ReadBuilder {
            database: self.database,
            query: self.query,
            selection,
            error: None,
        })
    }
}
impl<P: ReadSelection> ReadBuilder<P> {
    pub fn all(self) -> impl Future<Output = Result<Vec<P::Output>, DbError>> {
        let work = self.database.read(self.query);
        async move {
            if let Some(error) = self.error {
                return Err(error);
            }
            let Output::Rows { rows, .. } = work.await? else {
                return Err(DbError::internal("read returned a count"));
            };
            rows.into_iter()
                .map(|row| {
                    let Value::Object(mut record) = row else {
                        return Err(DbError::internal("read returned a non-object"));
                    };
                    self.selection.decode(&mut 0, &mut record)
                })
                .collect()
        }
    }
}
