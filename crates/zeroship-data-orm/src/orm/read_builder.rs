//! Typed Rust aliases and model projections over the shared read operation.
use super::*;

mod composition;
mod predicates;
pub use composition::{IntoReadOperand, ReadOperand, ReadOrder, ReadPredicate};
mod entity_query;
pub use entity_query::{EntityQuery, FieldOrder};
mod related;
pub use related::RelatedQuery;
mod scalars;
use crate::sql::{
    CompareOp, Direction, FieldPath, JoinKind, NullOrder, Operand, OrderKey, Predicate, RowLimit,
};
pub use scalars::*;

#[derive(Debug)]
pub struct EntityAlias<E: Entity> {
    database: Database,
    source: ReadSource,
    schema: std::sync::Arc<FieldMap>,
    entity: PhantomData<fn() -> E>,
}
impl<E: Entity> EntityCollection<E> {
    pub fn alias(&self, alias: &str) -> Result<EntityAlias<E>, DbError> {
        self.validate()?;
        read::ident(alias, crate::sql::IdentRole::Alias)?;
        Ok(EntityAlias {
            database: self.collection.database.clone(),
            source: ReadSource::new(E::COLLECTION, alias),
            schema: self.schema.clone(),
            entity: PhantomData,
        })
    }
}
impl<E: Entity> EntityAlias<E> {
    fn origin(&self) -> ReadOrigin {
        ReadOrigin {
            database: self.database.identity.clone(),
            scope: self.database.scope.clone(),
            source: self.source.clone(),
            schema: self.schema.clone(),
        }
    }
    pub fn column<C: Column<Entity = E>>(&self, _: Field<C>) -> SourceColumn<C> {
        SourceColumn {
            source: self.source.clone(),
            origin: self.origin(),
            entity: PhantomData,
        }
    }
    pub fn row<R: FromRow<E>>(&self) -> EntityProjection<E, R, false> {
        EntityProjection {
            source: self.source.alias.clone(),
            origin: self.origin(),
            entity: PhantomData,
        }
    }
    pub fn optional_row<R: FromRow<E>>(&self) -> EntityProjection<E, R, true> {
        EntityProjection {
            source: self.source.alias.clone(),
            origin: self.origin(),
            entity: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct SourceColumn<C: Column> {
    source: ReadSource,
    origin: ReadOrigin,
    entity: PhantomData<fn() -> C>,
}
impl<C: Column> SourceColumn<C> {
    fn path(&self) -> FieldPath {
        self.source
            .column(C::NAME)
            .expect("generated column and validated alias")
    }
    pub fn asc(&self) -> ReadOrder {
        ReadOrder {
            key: OrderKey {
                path: self.path(),
                direction: Direction::Ascending,
                nulls: NullOrder::Last,
            },
            origin: self.origin.clone(),
        }
    }
    pub fn desc(&self) -> ReadOrder {
        ReadOrder {
            key: OrderKey {
                path: self.path(),
                direction: Direction::Descending,
                nulls: NullOrder::First,
            },
            origin: self.origin.clone(),
        }
    }
}

#[derive(Debug)]
pub struct EntityProjection<E: Entity, R, const OPTIONAL: bool> {
    source: String,
    origin: ReadOrigin,
    entity: PhantomData<fn() -> (E, R)>,
}

pub trait ReadSelection {
    type Output;
    fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError>;
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
    fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError> {
        self.origin.validate(database, sources)
    }
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
    fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError> {
        self.origin.validate(database, sources)
    }
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
            fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError> {
                $(self.$index.validate(database, sources)?;)+
                Ok(())
            }
            fn projections(&self, next: &mut usize, output: &mut Vec<ReadProjection>) { $(self.$index.projections(next, output);)+ }
            fn decode(&self, next: &mut usize, row: &mut Record) -> Result<Self::Output, DbError> { Ok(($(self.$index.decode(next, row)?,)+)) }
        }
    }
}
tuple_selection!(A:0, B:1);
tuple_selection!(A:0, B:1, C:2);
tuple_selection!(A:0, B:1, C:2, D:3);
tuple_selection!(A:0, B:1, C:2, D:3, E:4);
tuple_selection!(A:0, B:1, C:2, D:3, E:4, F:5);
tuple_selection!(A:0, B:1, C:2, D:3, E:4, F:5, G:6);
tuple_selection!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7);

#[derive(Debug)]
pub struct ReadBuilder<P = ()> {
    database: Database,
    query: ReadQuery,
    selection: P,
    error: Option<DbError>,
    schemas: Vec<SchemaExpectation>,
}

#[derive(Debug)]
struct SchemaExpectation {
    collection: String,
    schema: std::sync::Arc<FieldMap>,
    scope: Option<Rc<Cell<bool>>>,
}

impl SchemaExpectation {
    fn validate(&self, database: &Database) -> Result<(), DbError> {
        check_scope(self.scope.as_ref())?;
        validate_bound_schema(database, &self.collection, &self.schema)
    }
}

#[derive(Debug, Clone)]
struct ReadOrigin {
    database: Rc<()>,
    scope: Option<Rc<Cell<bool>>>,
    source: ReadSource,
    schema: std::sync::Arc<FieldMap>,
}

impl ReadOrigin {
    fn validate(&self, database: &Database, sources: &[ReadSource]) -> Result<(), DbError> {
        check_scope(self.scope.as_ref())?;
        if !Rc::ptr_eq(&self.database, &database.identity)
            || !sources.iter().any(|source| {
                source.alias == self.source.alias && source.collection == self.source.collection
            })
        {
            return Err(read::invalid(
                "typed expressions must belong to a registered read source",
            ));
        }
        validate_bound_schema(database, &self.source.collection, &self.schema)
    }
}

pub(super) fn validate_bound_schema(
    database: &Database,
    collection: &str,
    expected: &std::sync::Arc<FieldMap>,
) -> Result<(), DbError> {
    database.check_scope()?;
    database.context.with(|| {
        let current = crate::descriptor::collection_schema(&database.binding, collection)?;
        if std::sync::Arc::ptr_eq(&current, expected) || current.as_ref() == expected.as_ref() {
            Ok(())
        } else {
            Err(schema_mismatch_for(collection))
        }
    })
}
impl Database {
    pub fn from<E: Entity>(&self, source: &EntityAlias<E>) -> ReadBuilder {
        ReadBuilder {
            database: self.clone(),
            query: ReadQuery::new(source.source.clone()),
            selection: (),
            error: (!Rc::ptr_eq(&self.identity, &source.database.identity))
                .then(|| read::invalid("read sources must belong to the same database handle")),
            schemas: vec![SchemaExpectation {
                collection: E::COLLECTION.into(),
                schema: source.schema.clone(),
                scope: source.database.scope.clone(),
            }],
        }
    }
}
impl<P> ReadBuilder<P> {
    fn register_origins(
        &mut self,
        origins: Vec<ReadOrigin>,
        sources: &[ReadSource],
    ) -> Result<(), DbError> {
        for origin in origins {
            origin.validate(&self.database, sources)?;
            self.schemas.push(SchemaExpectation {
                collection: origin.source.collection,
                schema: origin.schema,
                scope: origin.scope,
            });
        }
        Ok(())
    }
    fn sources(&self) -> Vec<ReadSource> {
        std::iter::once(self.query.source.clone())
            .chain(self.query.joins.iter().map(|join| join.source.clone()))
            .collect()
    }
    fn validate_schemas(&self) -> Result<(), DbError> {
        for expected in &self.schemas {
            expected.validate(&self.database)?;
        }
        Ok(())
    }
    fn join<E: Entity>(
        mut self,
        source: &EntityAlias<E>,
        on: ReadPredicate,
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
        source.database.check_scope()?;
        validate_bound_schema(&self.database, E::COLLECTION, &source.schema)?;
        self.schemas.push(SchemaExpectation {
            collection: E::COLLECTION.into(),
            schema: source.schema.clone(),
            scope: source.database.scope.clone(),
        });
        let mut sources = self.sources();
        sources.push(source.source.clone());
        self.register_origins(on.origins, &sources)?;
        self.query.joins.push(ReadJoin {
            kind,
            source: source.source.clone(),
            on: on.expression,
        });
        Ok(self)
    }
    pub fn left_join<E: Entity>(
        self,
        source: &EntityAlias<E>,
        on: ReadPredicate,
    ) -> Result<Self, DbError> {
        self.join(source, on, JoinKind::Left)
    }
    pub fn inner_join<E: Entity>(
        self,
        source: &EntityAlias<E>,
        on: ReadPredicate,
    ) -> Result<Self, DbError> {
        self.join(source, on, JoinKind::Inner)
    }
    pub fn filter(mut self, predicate: ReadPredicate) -> Self {
        if let Err(error) = self.register_origins(predicate.origins, &self.sources()) {
            self.error.get_or_insert(error);
        }
        self.query.filter = predicate.expression;
        self
    }
    pub fn group_by<C: ReadableColumn>(mut self, column: SourceColumn<C>) -> Self {
        if let Err(error) = column.origin.validate(&self.database, &self.sources()) {
            self.error.get_or_insert(error);
            return self;
        }
        self.schemas.push(SchemaExpectation {
            collection: C::Entity::COLLECTION.into(),
            schema: column.origin.schema.clone(),
            scope: column.origin.scope.clone(),
        });
        self.query.group_by.push(column.path());
        self
    }
    pub fn having(mut self, predicate: ReadPredicate) -> Self {
        if let Err(error) = self.register_origins(predicate.origins, &self.sources()) {
            self.error.get_or_insert(error);
        }
        self.query.having = predicate.expression;
        self
    }
    pub fn order_by(mut self, key: ReadOrder) -> Self {
        if let Err(error) = self.register_origins(vec![key.origin], &self.sources()) {
            self.error.get_or_insert(error);
        }
        self.query.order_by.push(key.key);
        self
    }
    pub fn limit(mut self, limit: i64) -> Result<Self, DbError> {
        self.query.limit = RowLimit::new(limit).map_err(|e| read::invalid(e.to_string()))?;
        Ok(self)
    }
    pub fn offset(mut self, offset: i64) -> Result<Self, DbError> {
        self.query.offset =
            crate::sql::RowOffset::new(offset).map_err(|e| read::invalid(e.to_string()))?;
        Ok(self)
    }
    pub fn select<S: ReadSelection>(mut self, selection: S) -> Result<ReadBuilder<S>, DbError> {
        self.validate_schemas()?;
        selection.validate(&self.database, &self.sources())?;
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
            schemas: self.schemas,
        })
    }
}
impl<P: ReadSelection> ReadBuilder<P> {
    pub fn all(self) -> impl Future<Output = Result<Vec<P::Output>, DbError>> {
        let sources = self.sources();
        let work = self
            .validate_schemas()
            .and_then(|()| self.selection.validate(&self.database, &sources))
            .map(|()| self.database.read(self.query));
        // Keep execution and projection state out of the caller's async state.
        Box::pin(async move {
            if let Some(error) = self.error {
                return Err(error);
            }
            for expected in &self.schemas {
                expected.validate(&self.database)?;
            }
            self.selection.validate(&self.database, &sources)?;
            let Output::Rows { rows, .. } = work?.await? else {
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
        })
    }
}
