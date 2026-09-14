//! Resolved physical statements. Application policy is applied before this boundary.

use super::{
    Ident, IdentRole, SchemaName,
    compiler::CompileError,
    predicate::{CompareOp, MembershipOp, PatternOp},
};
use crate::value::Value;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageType {
    Boolean,
    Integer,
    Real,
    Text,
    Bytes,
    Timestamp,
    ExactDecimal(DecimalStorage),
    Json,
    Vector,
    GeoPoint,
}

impl StorageType {
    pub fn exact_decimal(precision: u64, scale: u64) -> Result<Self, CompileError> {
        DecimalStorage::new(precision, scale).map(Self::ExactDecimal)
    }

    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Boolean => matches!(value, Value::Bool(_)),
            Self::Integer => matches!(value, Value::Number(value) if value.as_i64().is_some()),
            Self::Real => match value {
                Value::Number(value) => value.as_i64().is_some() || value.is_f64(),
                Value::Decimal(value) => valid_decimal(value),
                _ => false,
            },
            Self::ExactDecimal(_) => {
                matches!(value, Value::Decimal(value) if valid_decimal(value))
            }
            Self::Text => matches!(value, Value::String(_)),
            Self::Bytes => matches!(value, Value::Bytes(_)),
            Self::Timestamp => {
                matches!(value, Value::Timestamp(_))
                    || matches!(value, Value::String(_))
                        && crate::sql::temporal::timestamp_millis(value).is_some()
            }
            Self::Json => matches!(value, Value::Json(_) | Value::Array(_) | Value::Object(_)),
            Self::Vector => matches!(value, Value::Array(_) | Value::Bytes(_)),
            Self::GeoPoint => matches!(value, Value::Object(_) | Value::Bytes(_)),
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Real | Self::ExactDecimal(_))
    }

    fn accepts_arithmetic(self, value: &Value) -> bool {
        self.numeric() && self.accepts(value)
    }

    fn supports_equality(self) -> bool {
        !matches!(self, Self::Vector | Self::GeoPoint)
    }

    fn supports_ordering(self) -> bool {
        matches!(
            self,
            Self::Integer | Self::Real | Self::Text | Self::Timestamp
        )
    }

    fn supports_portable_equality(self) -> bool {
        matches!(
            self,
            Self::Boolean | Self::Integer | Self::Real | Self::Text | Self::Bytes | Self::Timestamp
        )
    }

    pub fn decimal(self) -> Option<DecimalStorage> {
        match self {
            Self::ExactDecimal(storage) => Some(storage),
            _ => None,
        }
    }

    fn compatible_with(self, other: Self) -> bool {
        self == other
            || matches!(
                (self, other),
                (Self::ExactDecimal(_), Self::ExactDecimal(_))
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecimalStorage {
    precision: u16,
    scale: u16,
}

impl DecimalStorage {
    pub fn new(precision: u64, scale: u64) -> Result<Self, CompileError> {
        let precision = u16::try_from(precision)
            .ok()
            .filter(|precision| (1..=crate::sql::decimal::MAX_PRECISION).contains(precision))
            .ok_or_else(|| invalid("decimal precision is outside the portable range"))?;
        let scale = u16::try_from(scale)
            .ok()
            .filter(|scale| *scale <= precision)
            .ok_or_else(|| invalid("decimal scale exceeds its precision"))?;
        Ok(Self { precision, scale })
    }

    pub fn precision(self) -> u16 {
        self.precision
    }

    pub fn scale(self) -> u16 {
        self.scale
    }
}

fn valid_decimal(value: &str) -> bool {
    crate::sql::decimal::valid(value)
}

#[derive(Debug)]
struct Source {
    namespace: SchemaName,
    table: Ident,
    alias: Option<Ident>,
    columns: Vec<(Ident, StorageType)>,
    indices: BTreeMap<String, usize>,
}

#[derive(Clone, Debug)]
pub struct Table(Arc<Source>);

impl Table {
    pub fn new(
        namespace: SchemaName,
        table: Ident,
        columns: impl IntoIterator<Item = (Ident, StorageType)>,
    ) -> Result<Self, CompileError> {
        Ident::parse_as(table.as_str(), IdentRole::Collection)?;
        let mut indices = BTreeMap::new();
        let mut resolved = Vec::new();
        for (name, storage) in columns {
            Ident::parse_as(name.as_str(), IdentRole::StoredColumn)?;
            if indices
                .insert(name.as_str().to_owned(), resolved.len())
                .is_some()
            {
                return Err(invalid("duplicate physical column"));
            }
            resolved.push((name, storage));
        }
        Ok(Self(Arc::new(Source {
            namespace,
            table,
            alias: None,
            columns: resolved,
            indices,
        })))
    }

    pub fn namespace(&self) -> &SchemaName {
        &self.0.namespace
    }
    pub fn name(&self) -> &Ident {
        &self.0.table
    }

    pub fn aliased(
        namespace: SchemaName,
        table: Ident,
        alias: Ident,
        columns: impl IntoIterator<Item = (Ident, StorageType)>,
    ) -> Result<Self, CompileError> {
        Ident::parse_as(alias.as_str(), IdentRole::Alias)?;
        let mut resolved = Self::new(namespace, table, columns)?;
        Arc::get_mut(&mut resolved.0)
            .expect("new table source is uniquely owned")
            .alias = Some(alias);
        Ok(resolved)
    }

    pub fn alias(&self) -> Option<&Ident> {
        self.0.alias.as_ref()
    }

    pub fn column(&self, name: &str) -> Result<Column, CompileError> {
        let index = self
            .0
            .indices
            .get(name)
            .copied()
            .ok_or_else(|| invalid("column does not belong to the resolved table"))?;
        Ok(Column {
            source: self.clone(),
            index,
        })
    }

    fn check_column(&self, column: &Column) -> Result<(), CompileError> {
        if !Arc::ptr_eq(&self.0, &column.source.0) {
            return Err(invalid("column belongs to a different source"));
        }
        Ok(())
    }

    pub fn owns(&self, column: &Column) -> bool {
        Arc::ptr_eq(&self.0, &column.source.0)
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    source: Table,
    index: usize,
}

impl Column {
    pub fn table(&self) -> &Table {
        &self.source
    }
    pub fn name(&self) -> &Ident {
        &self.source.0.columns[self.index].0
    }
    pub fn storage(&self) -> StorageType {
        self.source.0.columns[self.index].1
    }
}

pub enum Expression {
    Bind(Value),
    Null,
    Default,
    Current(Column),
    Incoming(Column),
    Increment {
        column: Column,
        step: i64,
    },
    Arithmetic {
        column: Column,
        operator: ArithmeticOperator,
        operand: Value,
    },
    ArrayMutation {
        column: Column,
        operator: ArrayOperator,
        operand: Value,
    },
    CurrentTimestamp,
    DatabaseTimestamp {
        offset_millis: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticOperator {
    Add,
    Subtract,
    Multiply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayOperator {
    Push,
    Pull,
    AddToSet,
}

#[derive(Debug)]
pub struct Assignment {
    pub column: Column,
    pub value: Expression,
}

#[derive(Clone)]
pub struct Comparison {
    pub column: Column,
    pub op: CompareOp,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub enum ResolvedOperand {
    /// A field-free marker for a matching row.
    RowPresence,
    Column(Column),
    Comparison(Comparison),
    Aggregate {
        function: super::AggregateFunc,
        column: Option<Column>,
        distinct: bool,
    },
}

impl ResolvedOperand {
    pub fn storage(&self) -> Result<StorageType, CompileError> {
        Ok(match self {
            Self::Column(column) => column.storage(),
            Self::Comparison(_) => StorageType::Boolean,
            Self::Aggregate {
                function: super::AggregateFunc::Count,
                column: None,
                distinct: true,
            } => return Err(invalid("COUNT(DISTINCT *) is not supported")),
            Self::RowPresence
            | Self::Aggregate {
                function: super::AggregateFunc::Count,
                ..
            } => StorageType::Integer,
            Self::Aggregate {
                function: super::AggregateFunc::Avg,
                column: Some(column),
                ..
            } if matches!(column.storage(), StorageType::Integer | StorageType::Real) => {
                StorageType::Real
            }
            Self::Aggregate {
                function: super::AggregateFunc::Sum,
                column: Some(column),
                ..
            } if matches!(column.storage(), StorageType::Integer | StorageType::Real) => {
                column.storage()
            }
            Self::Aggregate {
                function: super::AggregateFunc::Min | super::AggregateFunc::Max,
                column: Some(column),
                ..
            } if matches!(
                column.storage(),
                StorageType::Integer
                    | StorageType::Real
                    | StorageType::Text
                    | StorageType::Timestamp
            ) =>
            {
                column.storage()
            }
            Self::Aggregate {
                function: super::AggregateFunc::Sum | super::AggregateFunc::Avg,
                column: Some(_),
                ..
            } => return Err(invalid("sum and avg require numeric storage")),
            Self::Aggregate {
                function: super::AggregateFunc::Min | super::AggregateFunc::Max,
                column: Some(_),
                ..
            } => return Err(invalid("min and max require ordered portable storage")),
            Self::Aggregate { column: None, .. } => {
                return Err(invalid("aggregate requires a column"));
            }
        })
    }
}

#[derive(Debug)]
pub enum ResolvedPredicateValue {
    Operand(ResolvedOperand),
    Bind { storage: StorageType, value: Value },
}

#[derive(Debug)]
pub enum ResolvedPredicate {
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    Compare {
        lhs: ResolvedOperand,
        op: CompareOp,
        rhs: ResolvedPredicateValue,
    },
    Membership {
        lhs: ResolvedOperand,
        op: MembershipOp,
        values: Vec<Value>,
    },
    Pattern {
        lhs: ResolvedOperand,
        op: PatternOp,
        value: String,
        escape: Option<char>,
    },
    IsNull {
        operand: ResolvedOperand,
        negated: bool,
    },
    Const(bool),
}

impl ResolvedPredicate {
    pub fn and(mut children: Vec<Self>) -> Self {
        let mut flattened = Vec::new();
        for child in children.drain(..) {
            match child {
                Self::And(nested) => flattened.extend(nested),
                Self::Const(true) => {}
                Self::Const(false) => return Self::Const(false),
                child => flattened.push(child),
            }
        }
        match flattened.len() {
            0 => Self::Const(true),
            1 => flattened.pop().expect("one predicate"),
            _ => Self::And(flattened),
        }
    }

    pub fn or(mut children: Vec<Self>) -> Self {
        let mut flattened = Vec::new();
        for child in children.drain(..) {
            match child {
                Self::Or(nested) => flattened.extend(nested),
                Self::Const(false) => {}
                Self::Const(true) => return Self::Const(true),
                child => flattened.push(child),
            }
        }
        match flattened.len() {
            0 => Self::Const(false),
            1 => flattened.pop().expect("one predicate"),
            _ => Self::Or(flattened),
        }
    }
}

#[derive(Debug)]
pub struct ReturnedColumn {
    pub column: Column,
    pub alias: Option<Ident>,
}

#[derive(Debug)]
pub struct SelectedExpression {
    pub expression: ResolvedOperand,
    pub alias: Ident,
}

#[derive(Debug)]
pub struct ResolvedJoin {
    pub kind: super::JoinKind,
    pub table: Table,
    pub on: ResolvedPredicate,
}

#[derive(Debug)]
pub struct ResolvedOrder {
    pub expression: ResolvedOperand,
    pub direction: super::Direction,
    pub nulls: super::NullOrder,
}

#[derive(Debug)]
pub struct SelectParts {
    pub table: Table,
    pub joins: Vec<ResolvedJoin>,
    pub projection: Vec<SelectedExpression>,
    pub predicate: ResolvedPredicate,
    pub group_by: Vec<ResolvedOperand>,
    pub having: ResolvedPredicate,
    pub order_by: Vec<ResolvedOrder>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub distinct: bool,
    pub lock: RowLock,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowLock {
    #[default]
    None,
    Update,
}

#[derive(Debug)]
pub struct SelectStatement(SelectParts, Option<SelectSummary>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectSummary {
    Count,
    Exists,
}

impl SelectStatement {
    pub(crate) fn summarize(mut self, summary: SelectSummary) -> Self {
        self.0.order_by.clear();
        self.0.offset = None;
        self.0.limit = (summary == SelectSummary::Exists).then_some(1);
        self.1 = Some(summary);
        self
    }
    pub(crate) fn summary(&self) -> Option<SelectSummary> {
        self.1
    }

    pub fn new(parts: SelectParts) -> Result<Self, CompileError> {
        validate_select(&parts)?;
        Ok(Self(parts, None))
    }

    pub fn parts(&self) -> &SelectParts {
        &self.0
    }

    pub fn into_parts(self) -> SelectParts {
        self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_select(&self.0)
    }
}

#[derive(Debug)]
pub struct VectorSearchParts {
    pub table: Table,
    pub projection: Vec<ReturnedColumn>,
    pub identity: Column,
    pub vector: Column,
    pub query: Value,
    pub metric: super::descriptors::VectorMetric,
    pub predicate: ResolvedPredicate,
    pub limit: i64,
}

#[derive(Debug)]
pub struct VectorSearchStatement(VectorSearchParts);

impl VectorSearchStatement {
    pub fn new(parts: VectorSearchParts) -> Result<Self, CompileError> {
        validate_vector_search(&parts)?;
        Ok(Self(parts))
    }

    pub fn parts(&self) -> &VectorSearchParts {
        &self.0
    }

    pub fn into_parts(self) -> VectorSearchParts {
        self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_vector_search(&self.0)
    }
}

#[derive(Debug)]
pub struct SpatialNearParts {
    pub table: Table,
    pub projection: Vec<ReturnedColumn>,
    pub identity: Column,
    pub spatial: Column,
    pub point: Value,
    pub radius_m: f64,
    pub predicate: ResolvedPredicate,
    pub limit: i64,
}

#[derive(Debug)]
pub struct SpatialNearStatement(SpatialNearParts);

impl SpatialNearStatement {
    pub fn new(parts: SpatialNearParts) -> Result<Self, CompileError> {
        validate_spatial_near(&parts)?;
        Ok(Self(parts))
    }

    pub fn parts(&self) -> &SpatialNearParts {
        &self.0
    }

    pub fn into_parts(self) -> SpatialNearParts {
        self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_spatial_near(&self.0)
    }
}

#[derive(Debug)]
pub struct IdentityRequest {
    table: Table,
    column: Column,
    count: usize,
}

impl IdentityRequest {
    pub fn new(table: Table, column: Column, count: usize) -> Result<Self, CompileError> {
        table.check_column(&column)?;
        if column.storage() != StorageType::Integer {
            return Err(invalid("generated identity requires integer storage"));
        }
        if count == 0 {
            return Err(invalid("generated identity allocation cannot be empty"));
        }
        Ok(Self {
            table,
            column,
            count,
        })
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    pub fn column(&self) -> &Column {
        &self.column
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

#[derive(Debug)]
pub struct InsertParts {
    pub table: Table,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Expression>>,
    pub returning: Vec<ReturnedColumn>,
    pub insert_generated_identity: bool,
}

#[derive(Debug)]
pub enum MutationScope {
    First { target: Column },
    Matching,
}

#[derive(Debug)]
pub struct UpdateParts {
    pub table: Table,
    pub assignments: Vec<Assignment>,
    pub predicate: ResolvedPredicate,
    pub scope: MutationScope,
    pub returning: Vec<ReturnedColumn>,
}

#[derive(Debug)]
pub struct Update(UpdateParts);

impl Update {
    pub fn new(mut parts: UpdateParts) -> Result<Self, CompileError> {
        validate_update(&parts)?;
        parts
            .assignments
            .sort_by(|left, right| left.column.name().cmp(right.column.name()));
        Ok(Self(parts))
    }

    pub fn into_parts(self) -> UpdateParts {
        self.0
    }

    pub fn parts(&self) -> &UpdateParts {
        &self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_update(&self.0)
    }
}

#[derive(Debug)]
pub struct DeleteParts {
    pub table: Table,
    pub predicate: ResolvedPredicate,
    pub scope: MutationScope,
    pub returning: Vec<ReturnedColumn>,
}

#[derive(Debug)]
pub struct Delete(DeleteParts);

impl Delete {
    pub fn new(parts: DeleteParts) -> Result<Self, CompileError> {
        validate_delete(&parts)?;
        Ok(Self(parts))
    }

    pub fn into_parts(self) -> DeleteParts {
        self.0
    }

    pub fn parts(&self) -> &DeleteParts {
        &self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_delete(&self.0)
    }
}

#[derive(Debug)]
pub struct Insert(InsertParts);

impl Insert {
    pub fn new(mut parts: InsertParts) -> Result<Self, CompileError> {
        validate_insert(&parts)?;
        let mut order: Vec<_> = (0..parts.columns.len()).collect();
        order.sort_by(|left, right| {
            parts.columns[*left]
                .name()
                .cmp(parts.columns[*right].name())
        });
        parts.columns = order
            .iter()
            .map(|index| parts.columns[*index].clone())
            .collect();
        parts.rows = parts
            .rows
            .into_iter()
            .map(|row| {
                let mut row: Vec<_> = row.into_iter().map(Some).collect();
                order
                    .iter()
                    .map(|index| row[*index].take().expect("validated insert row"))
                    .collect()
            })
            .collect();
        Ok(Self(parts))
    }

    pub fn parts(&self) -> &InsertParts {
        &self.0
    }

    pub fn into_parts(self) -> InsertParts {
        self.0
    }

    pub fn validate(&self) -> Result<(), CompileError> {
        validate_insert(&self.0)
    }
}

#[derive(Debug)]
pub struct UpsertParts {
    pub table: Table,
    pub insert: Vec<Assignment>,
    pub conflict: Vec<Column>,
    pub update: Vec<Assignment>,
    pub condition: Option<Comparison>,
    pub returning: Vec<ReturnedColumn>,
    pub insert_generated_identity: bool,
}

#[derive(Debug)]
pub struct Upsert(UpsertParts);

impl Upsert {
    pub fn new(mut parts: UpsertParts) -> Result<Self, CompileError> {
        validate_upsert(&parts)?;
        // Input records are unordered; projection and conflict order are retained.
        parts
            .insert
            .sort_by(|a, b| a.column.name().cmp(b.column.name()));
        parts
            .update
            .sort_by(|a, b| a.column.name().cmp(b.column.name()));
        Ok(Self(parts))
    }

    pub fn parts(&self) -> &UpsertParts {
        &self.0
    }
    pub fn into_parts(self) -> UpsertParts {
        self.0
    }
    pub fn validate(&self) -> Result<(), CompileError> {
        validate_upsert(&self.0)
    }
}

#[derive(Debug)]
pub enum Statement {
    Select(SelectStatement),
    VectorSearch(VectorSearchStatement),
    SpatialNear(SpatialNearStatement),
    Insert(Insert),
    Upsert(Upsert),
    Update(Update),
    Delete(Delete),
}

fn validate_vector_search(parts: &VectorSearchParts) -> Result<(), CompileError> {
    parts.table.check_column(&parts.identity)?;
    parts.table.check_column(&parts.vector)?;
    if parts.identity.name().as_str() != "id" {
        return Err(invalid("vector search requires the collection identity"));
    }
    if parts.vector.storage() != StorageType::Vector || !StorageType::Vector.accepts(&parts.query) {
        return Err(invalid("vector search requires a vector column and query"));
    }
    if parts.limit <= 0 {
        return Err(invalid("vector search limit must be positive"));
    }
    if parts.projection.is_empty() {
        return Err(invalid("vector search requires a projection"));
    }
    validate_search_projection(&parts.table, &parts.projection, "_distance")?;
    validate_predicate_for_tables(&[&parts.table], &parts.predicate, false)
}

fn validate_spatial_near(parts: &SpatialNearParts) -> Result<(), CompileError> {
    parts.table.check_column(&parts.identity)?;
    parts.table.check_column(&parts.spatial)?;
    if parts.identity.name().as_str() != "id" {
        return Err(invalid("spatial search requires the collection identity"));
    }
    if parts.spatial.storage() != StorageType::GeoPoint
        || !StorageType::GeoPoint.accepts(&parts.point)
    {
        return Err(invalid(
            "spatial search requires a geographic column and point",
        ));
    }
    if !parts.radius_m.is_finite() || parts.radius_m <= 0.0 || parts.limit <= 0 {
        return Err(invalid("spatial search bounds are invalid"));
    }
    if parts.projection.is_empty() {
        return Err(invalid("spatial search requires a projection"));
    }
    validate_search_projection(&parts.table, &parts.projection, "_distance_m")?;
    validate_predicate_for_tables(&[&parts.table], &parts.predicate, false)
}

fn validate_search_projection(
    table: &Table,
    projection: &[ReturnedColumn],
    synthetic: &str,
) -> Result<(), CompileError> {
    validate_returning(table, projection)?;
    if projection.iter().any(|field| {
        field
            .alias
            .as_ref()
            .map_or_else(|| field.column.name().as_str(), Ident::as_str)
            == synthetic
    }) {
        return Err(invalid("search projection shadows its distance output"));
    }
    Ok(())
}

fn validate_select(parts: &SelectParts) -> Result<(), CompileError> {
    if parts.projection.is_empty() {
        return Err(invalid("select requires a projection"));
    }
    if parts.joins.len() >= super::MAX_READ_SOURCES {
        return Err(invalid("select exceeds its source budget"));
    }
    let mut tables = vec![&parts.table];
    tables.extend(parts.joins.iter().map(|join| &join.table));
    let mut aliases = HashSet::new();
    for table in &tables {
        let alias = table
            .alias()
            .ok_or_else(|| invalid("select sources require aliases"))?;
        if !aliases.insert(alias.as_str()) {
            return Err(invalid("duplicate select source alias"));
        }
    }
    let mut outputs = HashSet::new();
    for selected in &parts.projection {
        validate_operand(&tables, &selected.expression, true)?;
        Ident::parse_as(selected.alias.as_str(), IdentRole::Alias)?;
        if !outputs.insert(selected.alias.as_str()) {
            return Err(invalid("duplicate select output alias"));
        }
    }
    let mut introduced = vec![&parts.table];
    for join in &parts.joins {
        introduced.push(&join.table);
        validate_predicate_for_tables(&introduced, &join.on, false)?;
    }
    validate_predicate_for_tables(&tables, &parts.predicate, false)?;
    validate_predicate_for_tables(&tables, &parts.having, true)?;
    for expression in &parts.group_by {
        validate_operand(&tables, expression, false)?;
        if !expression.storage()?.supports_portable_equality() {
            return Err(invalid("group by requires portable equality storage"));
        }
    }
    for order in &parts.order_by {
        validate_operand(&tables, &order.expression, true)?;
        if !order.expression.storage()?.supports_ordering() {
            return Err(invalid("order by requires portable ordered storage"));
        }
    }
    if parts.distinct {
        for selected in &parts.projection {
            if !selected.expression.storage()?.supports_portable_equality() {
                return Err(invalid("distinct requires portable equality storage"));
            }
        }
    }
    let grouped = !parts.group_by.is_empty()
        || parts
            .projection
            .iter()
            .any(|selected| matches!(selected.expression, ResolvedOperand::Aggregate { .. }))
        || predicate_mentions_aggregate(&parts.having)
        || parts
            .order_by
            .iter()
            .any(|order| matches!(order.expression, ResolvedOperand::Aggregate { .. }));
    if !grouped && !matches!(&parts.having, ResolvedPredicate::Const(true)) {
        return Err(invalid("having requires a grouped or aggregate select"));
    }
    if grouped {
        for selected in &parts.projection {
            validate_grouped_operand(&selected.expression, &parts.group_by)?;
        }
        validate_grouped_predicate(&parts.having, &parts.group_by)?;
        for order in &parts.order_by {
            validate_grouped_operand(&order.expression, &parts.group_by)?;
        }
    }
    if parts.lock == RowLock::Update && (grouped || parts.distinct) {
        return Err(invalid("row locking requires an ungrouped select"));
    }
    if parts.limit.is_some_and(|value| value < 0) || parts.offset.is_some_and(|value| value < 0) {
        return Err(invalid("select pagination cannot be negative"));
    }
    Ok(())
}

fn predicate_mentions_aggregate(predicate: &ResolvedPredicate) -> bool {
    match predicate {
        ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
            children.iter().any(predicate_mentions_aggregate)
        }
        ResolvedPredicate::Not(child) => predicate_mentions_aggregate(child),
        ResolvedPredicate::Compare { lhs, rhs, .. } => {
            matches!(lhs, ResolvedOperand::Aggregate { .. })
                || matches!(
                    rhs,
                    ResolvedPredicateValue::Operand(ResolvedOperand::Aggregate { .. })
                )
        }
        ResolvedPredicate::Membership { lhs, .. }
        | ResolvedPredicate::Pattern { lhs, .. }
        | ResolvedPredicate::IsNull { operand: lhs, .. } => {
            matches!(lhs, ResolvedOperand::Aggregate { .. })
        }
        ResolvedPredicate::Const(_) => false,
    }
}

fn validate_grouped_predicate(
    predicate: &ResolvedPredicate,
    group_by: &[ResolvedOperand],
) -> Result<(), CompileError> {
    match predicate {
        ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
            for child in children {
                validate_grouped_predicate(child, group_by)?;
            }
        }
        ResolvedPredicate::Not(child) => validate_grouped_predicate(child, group_by)?,
        ResolvedPredicate::Compare { lhs, rhs, .. } => {
            validate_grouped_operand(lhs, group_by)?;
            if let ResolvedPredicateValue::Operand(rhs) = rhs {
                validate_grouped_operand(rhs, group_by)?;
            }
        }
        ResolvedPredicate::Membership { lhs, .. }
        | ResolvedPredicate::Pattern { lhs, .. }
        | ResolvedPredicate::IsNull { operand: lhs, .. } => {
            validate_grouped_operand(lhs, group_by)?;
        }
        ResolvedPredicate::Const(_) => {}
    }
    Ok(())
}

fn validate_grouped_operand(
    operand: &ResolvedOperand,
    group_by: &[ResolvedOperand],
) -> Result<(), CompileError> {
    let column = match operand {
        ResolvedOperand::Column(column)
        | ResolvedOperand::Comparison(Comparison { column, .. }) => column,
        ResolvedOperand::RowPresence | ResolvedOperand::Aggregate { .. } => return Ok(()),
    };
    let grouped = group_by.iter().any(|group| {
        matches!(group, ResolvedOperand::Column(candidate) if Arc::ptr_eq(&column.source.0, &candidate.source.0) && column.index == candidate.index)
    });
    if grouped {
        Ok(())
    } else {
        Err(invalid("aggregate select contains an ungrouped column"))
    }
}

fn invalid(message: &'static str) -> CompileError {
    CompileError::InvalidStatement(message.into())
}

fn validate_insert(parts: &InsertParts) -> Result<(), CompileError> {
    if parts.columns.is_empty() || parts.rows.is_empty() {
        return Err(invalid("insert requires columns and rows"));
    }
    let mut columns = HashSet::new();
    for column in &parts.columns {
        parts.table.check_column(column)?;
        if !columns.insert(column.index) {
            return Err(invalid("duplicate insert column"));
        }
    }
    for row in &parts.rows {
        if row.len() != parts.columns.len() {
            return Err(invalid("insert row does not match its column list"));
        }
        for (column, expression) in parts.columns.iter().zip(row) {
            validate_insert_expression(column.storage(), expression)?;
        }
    }
    validate_returning(&parts.table, &parts.returning)
}

fn validate_insert_expression(
    storage: StorageType,
    expression: &Expression,
) -> Result<(), CompileError> {
    match expression {
        Expression::Bind(value) if value.is_null() => {
            Err(invalid("SQL null must use the explicit null expression"))
        }
        Expression::Bind(value) if !storage.accepts(value) => Err(invalid(
            "bound value does not match its physical storage type",
        )),
        Expression::Bind(_) | Expression::Null | Expression::Default => Ok(()),
        _ => Err(invalid("insert values cannot reference an existing row")),
    }
}

fn validate_upsert(parts: &UpsertParts) -> Result<(), CompileError> {
    if parts.insert.is_empty() || parts.conflict.is_empty() || parts.update.is_empty() {
        return Err(invalid(
            "upsert requires insert values, a conflict target, and an update",
        ));
    }
    for (assignments, inserting) in [(&parts.insert, true), (&parts.update, false)] {
        let mut columns = HashSet::new();
        for assignment in assignments {
            parts.table.check_column(&assignment.column)?;
            if !columns.insert(assignment.column.index) {
                return Err(invalid("column is assigned more than once"));
            }
            let storage = assignment.column.storage();
            match &assignment.value {
                Expression::Bind(value) if value.is_null() => {
                    return Err(invalid("SQL null must use the explicit null expression"));
                }
                Expression::Bind(value) if !storage.accepts(value) => {
                    return Err(invalid(
                        "bound value does not match its physical storage type",
                    ));
                }
                Expression::Bind(_) | Expression::Null | Expression::Default => {}
                _ if inserting => {
                    return Err(invalid("insert values cannot reference a conflict row"));
                }
                Expression::Current(column) | Expression::Incoming(column) => {
                    parts.table.check_column(column)?;
                    if column.storage() != storage {
                        return Err(invalid("assignment column types differ"));
                    }
                }
                Expression::Increment { column, .. } => {
                    parts.table.check_column(column)?;
                    if column.index != assignment.column.index || storage != StorageType::Integer {
                        return Err(invalid("increment requires its assigned integer column"));
                    }
                }
                Expression::Arithmetic { .. } | Expression::ArrayMutation { .. } => {
                    return Err(invalid("upsert assignments cannot use update operators"));
                }
                Expression::CurrentTimestamp | Expression::DatabaseTimestamp { .. } => {
                    if storage != StorageType::Timestamp {
                        return Err(invalid("current timestamp requires timestamp storage"));
                    }
                    validate_timestamp_expression(&assignment.value)?;
                }
            }
        }
    }
    let mut conflict = HashSet::new();
    for column in &parts.conflict {
        parts.table.check_column(column)?;
        if !conflict.insert(column.index) {
            return Err(invalid("duplicate conflict target column"));
        }
    }
    if let Some(condition) = &parts.condition {
        parts.table.check_column(&condition.column)?;
        validate_comparison_operator(condition.column.storage(), condition.op)?;
        if condition.value.is_null() || !condition.column.storage().accepts(&condition.value) {
            return Err(invalid(
                "comparison requires a non-null value with matching storage type",
            ));
        }
    }
    validate_returning(&parts.table, &parts.returning)
}

fn validate_update(parts: &UpdateParts) -> Result<(), CompileError> {
    if parts.assignments.is_empty() {
        return Err(invalid("update requires assignments"));
    }
    let mut columns = HashSet::new();
    for assignment in &parts.assignments {
        parts.table.check_column(&assignment.column)?;
        if !columns.insert(assignment.column.index) {
            return Err(invalid("column is assigned more than once"));
        }
        validate_update_expression(&parts.table, &assignment.column, &assignment.value)?;
    }
    validate_predicate(&parts.table, &parts.predicate)?;
    validate_scope(&parts.table, &parts.scope)?;
    validate_returning(&parts.table, &parts.returning)
}

fn validate_delete(parts: &DeleteParts) -> Result<(), CompileError> {
    validate_predicate(&parts.table, &parts.predicate)?;
    validate_scope(&parts.table, &parts.scope)?;
    validate_returning(&parts.table, &parts.returning)
}

fn validate_scope(table: &Table, scope: &MutationScope) -> Result<(), CompileError> {
    if let MutationScope::First { target } = scope {
        table.check_column(target)?;
        if target.name().as_str() != "id" {
            return Err(invalid("first-row mutation requires the id column"));
        }
    }
    Ok(())
}

fn validate_update_expression(
    table: &Table,
    assigned: &Column,
    expression: &Expression,
) -> Result<(), CompileError> {
    let storage = assigned.storage();
    match expression {
        Expression::Bind(value) if value.is_null() => {
            Err(invalid("SQL null must use the explicit null expression"))
        }
        Expression::Bind(value) if !storage.accepts(value) => Err(invalid(
            "bound value does not match its physical storage type",
        )),
        Expression::Bind(_) | Expression::Null => Ok(()),
        Expression::CurrentTimestamp | Expression::DatabaseTimestamp { .. }
            if storage == StorageType::Timestamp =>
        {
            validate_timestamp_expression(expression)
        }
        Expression::Increment { column, .. } => {
            table.check_column(column)?;
            if column.index != assigned.index || storage != StorageType::Integer {
                return Err(invalid("increment requires its assigned integer column"));
            }
            Ok(())
        }
        Expression::Arithmetic {
            column, operand, ..
        } => {
            table.check_column(column)?;
            if column.index != assigned.index || !storage.accepts_arithmetic(operand) {
                return Err(invalid(
                    "arithmetic requires its assigned numeric column and operand",
                ));
            }
            Ok(())
        }
        Expression::ArrayMutation {
            column, operand, ..
        } => {
            table.check_column(column)?;
            if column.index != assigned.index
                || storage != StorageType::Json
                || !StorageType::Json.accepts(operand)
            {
                return Err(invalid(
                    "array mutation requires its assigned JSON column and encoded operand",
                ));
            }
            Ok(())
        }
        Expression::Default | Expression::Current(_) | Expression::Incoming(_) => {
            Err(invalid("expression is not valid in an ordinary update"))
        }
        Expression::CurrentTimestamp | Expression::DatabaseTimestamp { .. } => {
            Err(invalid("current timestamp requires timestamp storage"))
        }
    }
}

fn validate_timestamp_expression(expression: &Expression) -> Result<(), CompileError> {
    if let Expression::DatabaseTimestamp { offset_millis } = expression {
        if !super::temporal::is_timestamp_offset_millis(*offset_millis) {
            return Err(invalid("timestamp offset is outside the portable calendar"));
        }
    }
    Ok(())
}

fn validate_predicate(table: &Table, predicate: &ResolvedPredicate) -> Result<(), CompileError> {
    validate_predicate_for_tables(&[table], predicate, false)
}

fn validate_comparison_operator(storage: StorageType, op: CompareOp) -> Result<(), CompileError> {
    let supported = if matches!(op, CompareOp::Eq | CompareOp::Ne) {
        storage.supports_equality()
    } else {
        storage.supports_ordering()
    };
    if supported {
        Ok(())
    } else {
        Err(invalid(
            "comparison operator is not portable for this storage type",
        ))
    }
}

fn validate_predicate_for_tables(
    tables: &[&Table],
    predicate: &ResolvedPredicate,
    allow_aggregate: bool,
) -> Result<(), CompileError> {
    let mut pending = vec![(predicate, 1)];
    let mut nodes = 0;
    while let Some((predicate, depth)) = pending.pop() {
        nodes += 1;
        if depth > super::MAX_PREDICATE_DEPTH || nodes > super::joins::MAX_READ_PREDICATE_NODES {
            return Err(invalid("predicate exceeds its complexity budget"));
        }
        match predicate {
            ResolvedPredicate::And(children) | ResolvedPredicate::Or(children)
                if children.is_empty() =>
            {
                return Err(invalid("predicate connective cannot be empty"));
            }
            ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
                pending.extend(children.iter().map(|child| (child, depth + 1)));
            }
            ResolvedPredicate::Not(child) => pending.push((child, depth + 1)),
            ResolvedPredicate::Compare { lhs, op, rhs } => {
                validate_operand(tables, lhs, allow_aggregate)?;
                let lhs_storage = lhs.storage()?;
                validate_comparison_operator(lhs_storage, *op)?;
                match rhs {
                    ResolvedPredicateValue::Operand(rhs) => {
                        validate_operand(tables, rhs, allow_aggregate)?;
                        if !rhs.storage()?.compatible_with(lhs_storage) {
                            return Err(invalid(
                                "comparison operands have different storage types",
                            ));
                        }
                    }
                    ResolvedPredicateValue::Bind { storage, value }
                        if *storage == lhs_storage
                            && !value.is_null()
                            && storage.accepts(value) => {}
                    ResolvedPredicateValue::Bind { .. } => {
                        return Err(invalid(
                            "comparison requires a non-null value with matching storage type",
                        ));
                    }
                }
            }
            ResolvedPredicate::Membership { lhs, values, .. } => {
                validate_operand(tables, lhs, allow_aggregate)?;
                let storage = lhs.storage()?;
                if !storage.supports_equality()
                    || values.is_empty()
                    || values
                        .iter()
                        .any(|value| value.is_null() || !storage.accepts(value))
                {
                    return Err(invalid(
                        "membership requires non-null values with matching storage types",
                    ));
                }
            }
            ResolvedPredicate::Pattern {
                lhs, value, escape, ..
            } => {
                validate_operand(tables, lhs, allow_aggregate)?;
                if lhs.storage()? != StorageType::Text
                    || value.contains('\0')
                    || escape.is_some_and(|value| !value.is_ascii_graphic())
                {
                    return Err(invalid("pattern requires text storage without a NUL byte"));
                }
            }
            ResolvedPredicate::IsNull { operand, .. } => {
                validate_operand(tables, operand, allow_aggregate)?
            }
            ResolvedPredicate::Const(_) => {}
        }
    }
    Ok(())
}

fn validate_operand(
    tables: &[&Table],
    operand: &ResolvedOperand,
    allow_aggregate: bool,
) -> Result<(), CompileError> {
    if let ResolvedOperand::Aggregate {
        column: Some(column),
        distinct: true,
        ..
    } = operand
    {
        if !column.storage().supports_portable_equality() {
            return Err(invalid(
                "distinct aggregate requires portable equality storage",
            ));
        }
    }
    match operand {
        ResolvedOperand::RowPresence => {}
        ResolvedOperand::Comparison(comparison) => {
            validate_operand(
                tables,
                &ResolvedOperand::Column(comparison.column.clone()),
                false,
            )?;
            let storage = comparison.column.storage();
            validate_comparison_operator(storage, comparison.op)?;
            if comparison.value.is_null() || !storage.accepts(&comparison.value) {
                return Err(invalid(
                    "comparison projection requires a non-null value with matching storage type",
                ));
            }
        }
        ResolvedOperand::Column(column) => {
            if !tables
                .iter()
                .any(|table| table.check_column(column).is_ok())
            {
                return Err(invalid("column belongs to a different source"));
            }
        }
        ResolvedOperand::Aggregate { column, .. } if allow_aggregate => {
            if let Some(column) = column {
                if !tables
                    .iter()
                    .any(|table| table.check_column(column).is_ok())
                {
                    return Err(invalid("aggregate column belongs to a different source"));
                }
            }
        }
        ResolvedOperand::Aggregate { .. } => {
            return Err(invalid("aggregate is not valid in a mutation predicate"));
        }
    }
    operand.storage()?;
    Ok(())
}

fn validate_returning(table: &Table, returning: &[ReturnedColumn]) -> Result<(), CompileError> {
    let mut columns = HashSet::new();
    let mut outputs = HashSet::new();
    for field in returning {
        table.check_column(&field.column)?;
        if !columns.insert(field.column.index) {
            return Err(invalid("duplicate returning column"));
        }
        let output = match &field.alias {
            Some(alias) => {
                Ident::parse_as(alias.as_str(), IdentRole::Alias)?;
                alias.as_str()
            }
            None => field.column.name().as_str(),
        };
        if !outputs.insert(output) {
            return Err(invalid("duplicate returning output name"));
        }
    }
    Ok(())
}

impl std::fmt::Debug for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(value) => f
                .debug_tuple("Bind")
                .field(&super::compiler::ParameterType::of(value))
                .finish(),
            Self::Null => f.write_str("Null"),
            Self::Default => f.write_str("Default"),
            Self::Current(column) => f.debug_tuple("Current").field(column).finish(),
            Self::Incoming(column) => f.debug_tuple("Incoming").field(column).finish(),
            Self::Increment { column, .. } => f
                .debug_struct("Increment")
                .field("column", column)
                .finish_non_exhaustive(),
            Self::Arithmetic {
                column, operator, ..
            } => f
                .debug_struct("Arithmetic")
                .field("column", column)
                .field("operator", operator)
                .finish_non_exhaustive(),
            Self::ArrayMutation {
                column, operator, ..
            } => f
                .debug_struct("ArrayMutation")
                .field("column", column)
                .field("operator", operator)
                .finish_non_exhaustive(),
            Self::CurrentTimestamp => f.write_str("CurrentTimestamp"),
            Self::DatabaseTimestamp { .. } => f.write_str("DatabaseTimestamp"),
        }
    }
}

impl std::fmt::Debug for Comparison {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Comparison")
            .field("column", &self.column)
            .field("op", &self.op)
            .field(
                "parameter_type",
                &super::compiler::ParameterType::of(&self.value),
            )
            .finish()
    }
}
