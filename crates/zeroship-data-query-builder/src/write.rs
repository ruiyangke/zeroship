//! The write family: insert, insert-many, update, delete.
//!
//! # Writing a NULL without a null literal
//!
//! [`crate::Literal`] has no `Null` variant and this module does not add one.
//! That is a decision rather than an omission, and the write family is where it
//! has to be argued, because writing a NULL is something creators legitimately
//! do.
//!
//! The resolution is that **the assigned value is a node**, exactly as nullness
//! on the read side is a node ([`crate::Predicate::IsNull`]) rather than an
//! operator over a null operand. [`WriteValue`] has two variants: a bound
//! parameter, and [`WriteValue::Null`], which renders the `NULL` keyword and
//! binds nothing.
//!
//! Three reasons this is the right shape and `Literal::Null` is the wrong one:
//!
//! * **A `Literal` is by definition a bind parameter.** Every one of them is
//!   pushed onto the parameter list and referred to by a `$n`. A NULL is not
//!   bound - `query.rs:3558-3564` inlines the `NULL` keyword and says why in its
//!   own comment: *"Postgres' text-format param protocol (`query_text_params`,
//!   `&[&str]`) cannot represent NULL - an empty string would be encoded as
//!   `""`"*. A `Literal::Null` would therefore be a parameter that is never a
//!   parameter, which is the same category error as the shipped defect where a
//!   JSON null became `String::new()`.
//! * **It would re-open the defect the read family closed.** `Literal` is
//!   reachable from [`crate::Operand::Lit`], so a null literal would immediately
//!   be constructible on both sides of every comparison and inside every
//!   membership set - which is precisely the `{ f: { $in: [null] } }` shape that
//!   compiled to `f IN ($1)` with `$1 = ''`. [`WriteValue`] is not an `Operand`
//!   and cannot appear in a predicate, so the blast radius of the new variant is
//!   the SET/VALUES position and nothing else.
//! * **The conversion boundary keeps working.** A caller translating a nullable
//!   wire value crosses [`WriteValue::from_optional`], whose `Option<Literal>`
//!   argument makes them handle the `None` arm. They cannot get a "value" out of
//!   it that is secretly a null.
//!
//! # `col = col + 1` is NOT a later problem
//!
//! The read family has no function-call node and no arithmetic, and it does not
//! need one. The write family does, immediately: `query.rs:3907` emits
//! `"version" = "version" + 1` on **every** update that arrives through the CRUD
//! dispatch path, and `query.rs:3814-3825` lowers `$inc` / `$dec` / `$mul` to
//! `{col} = {col} <op> ${n}::numeric`. A write family that cannot express those
//! cannot render a single production `UPDATE`, and the first person who needs
//! one adds a string escape hatch.
//!
//! So [`Assignment`] carries [`Arithmetic`] - and it is deliberately **not** a
//! general expression node. The left operand is implicitly the column being
//! assigned, the operator set is three, and the right operand is one numeric
//! literal. `a = b + 1` (cross-column), `a = (a + 1) * 2` (nested) and
//! `a = f(a)` (function call) are all unrepresentable. That closure is the whole
//! point: SC-3 leaves per-family node spelling to each family's port precisely
//! so a shared expression grammar is not fixed by whoever ports first, and a
//! general `Expr` node introduced here would fix it for search, unmask and
//! effects as well.
//!
//! The `::numeric` cast in the shipped lowering is **deleted**, not relocated,
//! for the same reason [`crate::render::postgres::PostgresValueFormat`] deletes
//! `decode($N, 'base64')::bytea`: the cast is there because the parameter is a
//! `String`. A [`crate::Literal::Int`] binds as an integer, so there is nothing
//! left for the cast to repair.
//!
//! # What is deliberately absent, and what it costs
//!
//! `$push`, `$pull` and `$addToSet` (`query.rs:3826-3845`) lower to `jsonb`
//! expressions - `||`, `jsonb_agg` over `jsonb_array_elements`, `@>` inside a
//! `CASE`. They are not here. Writing them would mean settling the same
//! `PostgreSQL` JSON operator-resolution question [`crate::path`] refuses to
//! settle without a server, and settling it inside three ad-hoc templates rather
//! than as a grammar. The cost is stated rather than hidden: those three update
//! operators cannot be ported until a JSON node exists, and a ledger row for
//! `build_set_clauses_with_system_fields` must stay `unported` on their account.

use crate::ident::Ident;
use crate::literal::Literal;
use crate::plan::RowLimit;
use crate::predicate::{Predicate, MAX_PREDICATE_DEPTH};
use crate::projection::{Projection, ProjectionKind};
use core::fmt;

/// The largest number of rows one insert may carry.
///
/// Mirrors `MAX_INSERT_MANY_BATCH` (`query.rs:601`), and it is a **separate**
/// bound from [`BindBudget`] rather than a proxy for it. `query.rs:598-600`
/// says why in its own words: the document count "bounds the multi-row SQL
/// string and row bookkeeping materialized in the worker" and "says nothing
/// about row width". A batch of 1,000 one-column rows and a batch of 20
/// five-thousand-column rows stress different resources, and a single cap can
/// only see one of them.
pub const MAX_INSERT_ROWS: usize = 1_000;

/// The bind-parameter ceiling of one backend's wire protocol.
///
/// # Why this is an argument and not a constant
///
/// The bound is **per dialect**: `POSTGRES_MAX_BIND_PARAMETERS` and
/// `SQLITE_MAX_BIND_PARAMETERS` are two different numbers at `query.rs:602-603`.
/// This crate has no dialect at plan-construction time, because a plan is
/// backend-neutral by design and the backend is chosen at lowering.
///
/// Three ways to resolve that, two of them wrong:
///
/// * **Hard-code `PostgreSQL`.** Then the dev tier silently accepts a batch it
///   cannot execute, and the failure arrives from `SQLite` as a driver error
///   naming neither the batch nor the bound.
/// * **Take the tighter of the two.** Then production refuses a batch it could
///   serve because the dev tier could not, which is the dev tier constraining
///   production - the inversion of what a dev tier is for.
/// * **Make the caller name it.** The caller *does* know which backend the plan
///   is bound for; it is the same decision that picks the renderer. So the
///   budget is a required constructor argument with no default.
///
/// The type is a closed set of two associated constants and has **no public
/// constructor**, so a caller cannot invent a budget of `usize::MAX` and argue
/// their way past the bound. Adding a dialect means adding a constant here,
/// beside the two it must be compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindBudget {
    dialect: &'static str,
    max: usize,
}

impl BindBudget {
    /// `PostgreSQL`: 65,535.
    ///
    /// Verified against the encoder that writes the field rather than taken
    /// from the mirrored constant alone: `frontend::bind` writes the Bind
    /// message's parameter count through `write_counted`, which narrows with
    /// `u16::from_usize(count)?` and `BigEndian::write_u16`
    /// (postgres-protocol 0.6.12, `src/message/frontend.rs:48-101`; that
    /// version is the one `Cargo.lock:4366-4368` resolves). One parameter more
    /// than this is an encoder error, not a truncated message.
    pub const POSTGRES: Self = Self {
        dialect: "postgres",
        max: u16::MAX as usize,
    };

    /// `SQLite`: 32,766. Mirrors `SQLITE_MAX_BIND_PARAMETERS` (`query.rs:603`).
    pub const SQLITE: Self = Self {
        dialect: "sqlite",
        max: 32_766,
    };

    /// The ceiling.
    #[must_use]
    pub const fn max(self) -> usize {
        self.max
    }

    /// The dialect's name, for refusal messages.
    #[must_use]
    pub const fn dialect_name(self) -> &'static str {
        self.dialect
    }
}

/// A value a write puts into a column.
///
/// Two variants, and the second is this module's argument: nullness is a node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WriteValue {
    /// A bound parameter. Renders `$n`.
    Bind(Literal),
    /// SQL `NULL`. Renders the keyword and binds nothing.
    ///
    /// This is **not** a [`Literal`] variant; see the module note for the three
    /// reasons it must not become one.
    Null,
}

impl WriteValue {
    /// The conversion boundary a caller crossing from a nullable source must
    /// use.
    ///
    /// The signature is the mechanism, exactly as it is for
    /// [`Literal::from_optional`]: a caller holding "a value that might be null"
    /// cannot reach a `Bind` without the `None` arm being handled, and the
    /// `None` arm produces a node.
    // Not `const`: `Option<Literal>` owns a `String`/`Vec`, whose destructor
    // cannot run in a const context (E0493). The signature is what matters
    // here, not the const-ness.
    #[must_use]
    pub fn from_optional(value: Option<Literal>) -> Self {
        value.map_or(Self::Null, Self::Bind)
    }

    /// Whether this writes SQL `NULL`.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// How many bind parameters this value consumes: one, or none for a
    /// [`WriteValue::Null`].
    #[must_use]
    pub const fn bind_count(&self) -> usize {
        match self {
            Self::Bind(_) => 1,
            Self::Null => 0,
        }
    }
}

/// The three operators an [`Arithmetic`] assignment may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArithmeticOp {
    Add,
    Subtract,
    Multiply,
}

impl ArithmeticOp {
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
        }
    }
}

/// `col = col <op> $n`, where the left operand is the column being assigned.
///
/// The fields are private and the only constructor validates, for the same
/// reason [`crate::LiteralSet`]'s are: the invariant has to travel with the
/// value rather than be re-checked by whoever renders it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Arithmetic {
    op: ArithmeticOp,
    operand: Literal,
}

impl Arithmetic {
    /// # Errors
    ///
    /// [`WriteError::ArithmeticOperandNotNumeric`] for anything but
    /// [`Literal::Int`] or [`Literal::Float`]. `col = col + $1` with a text
    /// parameter is an operator-resolution failure at the server, reported
    /// against a statement the creator never wrote; the shipped lowering
    /// (`query.rs:3814-3825`) pushes whatever `value_to_param` returns and finds
    /// out at execution.
    pub fn new(op: ArithmeticOp, operand: Literal) -> Result<Self, WriteError> {
        if !matches!(operand, Literal::Int(_) | Literal::Float(_)) {
            return Err(WriteError::ArithmeticOperandNotNumeric {
                found: operand.type_name(),
            });
        }
        Ok(Self { op, operand })
    }

    #[must_use]
    pub const fn op(&self) -> ArithmeticOp {
        self.op
    }

    #[must_use]
    pub const fn operand(&self) -> &Literal {
        &self.operand
    }
}

/// The right-hand side of one `UPDATE ... SET` clause.
///
/// Deliberately a different type from [`WriteValue`], which is what an `INSERT`
/// writes. `INSERT INTO t (a) VALUES (a + 1)` references a column that does not
/// exist yet and `PostgreSQL` refuses it; making the two positions share one
/// type would make that shape representable for no gain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Assignment {
    /// `col = $n` or `col = NULL`.
    Set(WriteValue),
    /// `col = col <op> $n`.
    Arithmetic(Arithmetic),
    /// `col = NOW()` - **the server's** clock, not the worker's.
    ///
    /// A named node rather than a general function call, and it earns its place
    /// twice over. `query.rs:3910-3917` stamps `updated_at` on every `UPDATE`
    /// through `renderer(dialect).current_timestamp_expr()`, so the shape is
    /// mandatory; and the value must come from the database, because a worker
    /// with a skewed clock would otherwise write a timestamp that disagrees with
    /// every other worker's.
    ///
    /// The spelling belongs to the backend ([`crate::render::ValueFormat`]) -
    /// `NOW()` on `PostgreSQL`, `CURRENT_TIMESTAMP` on `SQLite`.
    CurrentTimestamp,
}

impl Assignment {
    /// `col = $n`.
    #[must_use]
    pub const fn bind(value: Literal) -> Self {
        Self::Set(WriteValue::Bind(value))
    }

    /// `col = NULL`.
    #[must_use]
    pub const fn null() -> Self {
        Self::Set(WriteValue::Null)
    }

    /// `col = col <op> $n`.
    ///
    /// # Errors
    ///
    /// [`WriteError::ArithmeticOperandNotNumeric`].
    pub fn arithmetic(op: ArithmeticOp, operand: Literal) -> Result<Self, WriteError> {
        Arithmetic::new(op, operand).map(Self::Arithmetic)
    }

    /// How many bind parameters this assignment consumes.
    #[must_use]
    pub const fn bind_count(&self) -> usize {
        match self {
            Self::Set(value) => value.bind_count(),
            Self::Arithmetic(_) => 1,
            Self::CurrentTimestamp => 0,
        }
    }
}

/// One column of one inserted row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnValue {
    pub column: Ident,
    pub value: WriteValue,
}

impl ColumnValue {
    #[must_use]
    pub const fn new(column: Ident, value: WriteValue) -> Self {
        Self { column, value }
    }
}

/// One `SET` clause of an update.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnAssignment {
    pub column: Ident,
    pub value: Assignment,
}

impl ColumnAssignment {
    #[must_use]
    pub const fn new(column: Ident, value: Assignment) -> Self {
        Self { column, value }
    }
}

/// What a write returns.
///
/// # `RETURNING *` has no representation, and that is what this type is for
///
/// **The asymmetry this paragraph described is now closed on both sides.** It
/// used to say that twelve production sites in `query.rs` wrote `RETURNING *`,
/// and listed their line numbers; the star is gone from all twelve (they
/// project from the runtime descriptor, `query::build_returning_expr`) and the
/// line-number census was already stale when the sites moved. A census of
/// somebody else's line numbers cannot be kept true from here, so this one is
/// not replaced.
///
/// The reason the type has no wildcard is unchanged and is not about that
/// census. A star returns every **physical** column of the row, which is not
/// the set the creator may see: a protected field occupies two columns, and only
/// one of them is on the read surface. Expressing "everything" would let a
/// rendered plan reach the other one with no node to point at, which is the
/// property this type exists to make unstateable. It reuses [`Projection`], the
/// same node the read family projects through, including
/// [`crate::ProjectionSource::Stored`].
///
/// `Projection` fits without alteration and is therefore reused rather than
/// re-invented: it is non-empty, it has no wildcard variant, it unions the
/// platform fields in its constructor, and it carries the mask substitution as a
/// node. The one thing a `RETURNING` list must refuse that a `SELECT` list may
/// carry is an aggregate, and [`Returning::rows`] refuses it.
///
/// The field is private and there is no "everything" constructor. The two
/// spellings are an explicit column list, and nothing at all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Returning(Option<Projection>);

impl Returning {
    /// No `RETURNING` clause at all. The statement reports an affected-row count
    /// and no rows.
    ///
    /// This is the *narrow* end of the type, not a convenience for the wide end:
    /// it returns fewer columns than any list, never more. It is right for a
    /// bulk purge, whose only result today is a row count that
    /// `dispatch_purge_many` obtains by counting the rows its `RETURNING`
    /// clause hands back - a projected column list since 2026-08-28, and a
    /// star before that. Either way it materialises rows to count them.
    ///
    /// **It has a consequence the caller owns.** The change-event publication
    /// correlates on `row["id"]`, so a write that returns nothing has no rows to
    /// publish and no keys to publish them under. Choosing `nothing()` is
    /// choosing not to emit events for that write.
    #[must_use]
    pub const fn nothing() -> Self {
        Self(None)
    }

    /// An explicit returned-column list.
    ///
    /// # Errors
    ///
    /// [`WriteError::AggregateReturning`] for an aggregate projection. A write
    /// returns the rows it touched; `RETURNING COUNT(*)` is not valid
    /// `PostgreSQL`, and the affected-row count is already in the driver's
    /// command tag.
    pub fn rows(projection: Projection) -> Result<Self, WriteError> {
        if projection.kind() == ProjectionKind::Aggregate {
            return Err(WriteError::AggregateReturning);
        }
        Ok(Self(Some(projection)))
    }

    /// The returned columns, or `None` for [`Returning::nothing`].
    #[must_use]
    pub const fn projection(&self) -> Option<&Projection> {
        self.0.as_ref()
    }
}

/// An insert of one or more rows.
///
/// `insert` and `insertMany` are **one node**, because they are one statement:
/// `INSERT INTO t (a) VALUES ($1)` is what both produce for a single row. Two
/// nodes would give one logical plan two spellings, which is the property
/// [`crate::render`] rests on and the reason [`crate::Predicate::range`] is a
/// constructor rather than a variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insert {
    namespace: Option<Ident>,
    collection: Ident,
    /// Canonical: sorted, unique, non-empty.
    columns: Vec<Ident>,
    /// One entry per row, each aligned positionally with `columns`.
    rows: Vec<Vec<WriteValue>>,
    returning: Returning,
    budget: BindBudget,
}

impl Insert {
    /// Start building an insert.
    ///
    /// `budget` has no default; see [`BindBudget`].
    #[must_use]
    pub const fn builder(
        collection: Ident,
        returning: Returning,
        budget: BindBudget,
    ) -> InsertBuilder {
        InsertBuilder {
            namespace: None,
            collection,
            rows: Vec::new(),
            returning,
            budget,
        }
    }

    #[must_use]
    pub const fn namespace(&self) -> Option<&Ident> {
        self.namespace.as_ref()
    }

    #[must_use]
    pub const fn collection(&self) -> &Ident {
        &self.collection
    }

    /// The shared column list, in canonical order.
    #[must_use]
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// The rows, each aligned positionally with [`Insert::columns`].
    #[must_use]
    pub fn rows(&self) -> &[Vec<WriteValue>] {
        &self.rows
    }

    #[must_use]
    pub const fn returning(&self) -> &Returning {
        &self.returning
    }

    #[must_use]
    pub const fn budget(&self) -> BindBudget {
        self.budget
    }

    /// How many bind parameters this insert will emit. Never above
    /// [`BindBudget::max`], which is checked at construction.
    #[must_use]
    pub fn bind_count(&self) -> usize {
        self.rows
            .iter()
            .flat_map(|row| row.iter())
            .map(WriteValue::bind_count)
            .sum()
    }
}

/// Builder for [`Insert`].
#[derive(Debug, Clone)]
pub struct InsertBuilder {
    namespace: Option<Ident>,
    collection: Ident,
    rows: Vec<Vec<ColumnValue>>,
    returning: Returning,
    budget: BindBudget,
}

impl InsertBuilder {
    #[must_use]
    pub fn namespace(mut self, namespace: Ident) -> Self {
        self.namespace = Some(namespace);
        self
    }

    /// Append one row. Called once for `insert`, many times for `insertMany`.
    #[must_use]
    pub fn row(mut self, row: Vec<ColumnValue>) -> Self {
        self.rows.push(row);
        self
    }

    /// Check the invariants and canonicalise.
    ///
    /// # Errors
    ///
    /// [`WriteError`], naming the invariant that failed. In particular
    /// [`WriteError::ColumnSetMismatch`], which is where a schema fact is handed
    /// back to the caller rather than guessed at - see its own documentation.
    pub fn build(self) -> Result<Insert, WriteError> {
        if self.rows.is_empty() {
            return Err(WriteError::EmptyBatch);
        }
        // The batch cap is checked BEFORE the rows are walked, so a hostile
        // batch bounds the work this function does rather than only the
        // statement it would have produced. `query.rs:4047-4056` makes the same
        // ordering argument for the same reason.
        if self.rows.len() > MAX_INSERT_ROWS {
            return Err(WriteError::BatchTooLarge {
                rows: self.rows.len(),
            });
        }

        let mut columns: Vec<Ident> = self.rows[0].iter().map(|c| c.column.clone()).collect();
        if columns.is_empty() {
            return Err(WriteError::EmptyColumnList);
        }
        columns.sort();
        if let Some(pair) = columns.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(WriteError::DuplicateColumn {
                column: pair[0].as_str().to_string(),
            });
        }

        let mut rows: Vec<Vec<WriteValue>> = Vec::with_capacity(self.rows.len());
        let mut binds = 0_usize;
        for (index, row) in self.rows.iter().enumerate() {
            if row.len() != columns.len() {
                return Err(WriteError::ColumnSetMismatch { row: index });
            }
            let mut aligned: Vec<WriteValue> = Vec::with_capacity(columns.len());
            for column in &columns {
                let Some(cell) = row.iter().find(|c| &c.column == column) else {
                    return Err(WriteError::ColumnSetMismatch { row: index });
                };
                // Saturating rather than checked: a saturated total is above
                // any budget, so the refusal below is still the outcome, and an
                // error variant for an addition that cannot overflow a `usize`
                // would be a branch no test could reach.
                binds = binds.saturating_add(cell.value.bind_count());
                aligned.push(cell.value.clone());
            }
            rows.push(aligned);
        }

        if binds > self.budget.max() {
            return Err(WriteError::BindBudgetExceeded {
                binds,
                budget: self.budget,
            });
        }

        Ok(Insert {
            namespace: self.namespace,
            collection: self.collection,
            columns,
            rows,
            returning: self.returning,
            budget: self.budget,
        })
    }
}

/// A bounded update.
///
/// # There is no unbounded update
///
/// [`RowLimit`] is not optional and has no "all rows" value, exactly as it is
/// not optional on [`crate::Select`]. That closes a hole that is open today in
/// two independent places:
///
/// * `build_update_many_with_system_fields` emits the `WHERE` clause only when
///   the filter is non-empty and never emits a bound, so `updateMany({}, ...)`
///   renders `UPDATE "app"."t" SET ... RETURNING <cols>` - a whole-table
///   rewrite that also materialises every row. (The clause names its columns
///   since 2026-08-28 and was `RETURNING *` before; the hole is the missing
///   bound, which neither spelling closes.)
/// * the `MAX_QUERY_LIMIT` cap that does exist is enforced by the **caller**,
///   and only on one of two branches. `dispatch_update_many` probes the target
///   ids and refuses above the cap at `crud/mod.rs:1249`, but that check sits
///   inside `if per_row_encrypted_update` (`:1233`). An update that touches no
///   randomised-encrypted column falls through to `:1353-1385`, which builds the
///   statement straight from the caller's filter with no probe and no cap.
///
/// A bound that lives in a caller is a bound one branch can miss. Here it is a
/// field with no absent value.
///
/// # The lowering is one statement, and it holds a lock
///
/// See [`crate::render::postgres::render_update`]: the bound is expressed as a
/// subquery over the backend's row-identity column, so the read that chooses the
/// rows and the write that changes them are one statement against one snapshot,
/// rather than the probe-then-write pair the encrypted branch performs across
/// two round trips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    namespace: Option<Ident>,
    collection: Ident,
    /// Canonical: sorted by column, unique, non-empty.
    assignments: Vec<ColumnAssignment>,
    filter: Predicate,
    limit: RowLimit,
    returning: Returning,
}

impl Update {
    /// Start building an update. The limit is required; there is no default and
    /// no absent value.
    #[must_use]
    pub const fn builder(collection: Ident, limit: RowLimit, returning: Returning) -> UpdateBuilder {
        UpdateBuilder {
            namespace: None,
            collection,
            assignments: Vec::new(),
            filter: Predicate::always(),
            limit,
            returning,
        }
    }

    #[must_use]
    pub const fn namespace(&self) -> Option<&Ident> {
        self.namespace.as_ref()
    }

    #[must_use]
    pub const fn collection(&self) -> &Ident {
        &self.collection
    }

    /// The `SET` clauses, in canonical order.
    #[must_use]
    pub fn assignments(&self) -> &[ColumnAssignment] {
        &self.assignments
    }

    #[must_use]
    pub const fn filter(&self) -> &Predicate {
        &self.filter
    }

    /// The maximum number of rows this update may touch.
    ///
    /// A caller that must distinguish "exactly this many matched" from "more
    /// matched and the rest were left alone" compares the returned row count
    /// against this. The plan cannot make that call for them: it does not know
    /// the match count, and refusing versus truncating over the cap is a policy
    /// decision (`crud/mod.rs:1249-1262` refuses; `LIMIT 1` on `updateOne`
    /// truncates). What the plan guarantees is that the write is bounded and
    /// that the bound is visible.
    #[must_use]
    pub const fn limit(&self) -> RowLimit {
        self.limit
    }

    #[must_use]
    pub const fn returning(&self) -> &Returning {
        &self.returning
    }
}

/// Builder for [`Update`].
#[derive(Debug, Clone)]
pub struct UpdateBuilder {
    namespace: Option<Ident>,
    collection: Ident,
    assignments: Vec<ColumnAssignment>,
    filter: Predicate,
    limit: RowLimit,
    returning: Returning,
}

impl UpdateBuilder {
    #[must_use]
    pub fn namespace(mut self, namespace: Ident) -> Self {
        self.namespace = Some(namespace);
        self
    }

    /// Append one `SET` clause.
    #[must_use]
    pub fn set(mut self, assignment: ColumnAssignment) -> Self {
        self.assignments.push(assignment);
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: Predicate) -> Self {
        self.filter = filter;
        self
    }

    /// Check the invariants and canonicalise.
    ///
    /// # Errors
    ///
    /// [`WriteError`], naming the invariant that failed.
    pub fn build(self) -> Result<Update, WriteError> {
        let assignments = canonical_assignments(self.assignments)?;
        check_filter_depth(&self.filter)?;
        let filter = self.filter.canonical();
        if filter.mentions_aggregate() {
            return Err(WriteError::AggregateInFilter);
        }
        Ok(Update {
            namespace: self.namespace,
            collection: self.collection,
            assignments,
            filter,
            limit: self.limit,
            returning: self.returning,
        })
    }
}

/// A bounded delete.
///
/// The same bound as [`Update`], for the same reason and with a second site to
/// point at: `dispatch_purge_many` (`crud/mod.rs:1586-1613`) hands
/// `build_delete_many` a creator filter and no bound at all, and
/// `build_delete_many` omits the `WHERE` clause entirely when that filter is
/// empty (`query.rs:4249-4254`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delete {
    namespace: Option<Ident>,
    collection: Ident,
    filter: Predicate,
    limit: RowLimit,
    returning: Returning,
}

impl Delete {
    /// Start building a delete. The limit is required.
    #[must_use]
    pub const fn builder(collection: Ident, limit: RowLimit, returning: Returning) -> DeleteBuilder {
        DeleteBuilder {
            namespace: None,
            collection,
            filter: Predicate::always(),
            limit,
            returning,
        }
    }

    #[must_use]
    pub const fn namespace(&self) -> Option<&Ident> {
        self.namespace.as_ref()
    }

    #[must_use]
    pub const fn collection(&self) -> &Ident {
        &self.collection
    }

    #[must_use]
    pub const fn filter(&self) -> &Predicate {
        &self.filter
    }

    #[must_use]
    pub const fn limit(&self) -> RowLimit {
        self.limit
    }

    #[must_use]
    pub const fn returning(&self) -> &Returning {
        &self.returning
    }
}

/// Builder for [`Delete`].
#[derive(Debug, Clone)]
pub struct DeleteBuilder {
    namespace: Option<Ident>,
    collection: Ident,
    filter: Predicate,
    limit: RowLimit,
    returning: Returning,
}

impl DeleteBuilder {
    #[must_use]
    pub fn namespace(mut self, namespace: Ident) -> Self {
        self.namespace = Some(namespace);
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: Predicate) -> Self {
        self.filter = filter;
        self
    }

    /// Check the invariants and canonicalise.
    ///
    /// # Errors
    ///
    /// [`WriteError::AggregateInFilter`].
    pub fn build(self) -> Result<Delete, WriteError> {
        check_filter_depth(&self.filter)?;
        let filter = self.filter.canonical();
        if filter.mentions_aggregate() {
            return Err(WriteError::AggregateInFilter);
        }
        Ok(Delete {
            namespace: self.namespace,
            collection: self.collection,
            filter,
            limit: self.limit,
            returning: self.returning,
        })
    }
}

/// Refuse a filter nested past [`MAX_PREDICATE_DEPTH`], **before** anything
/// recursive touches it.
///
/// `Predicate`'s own smart constructors already refuse an over-deep tree, but
/// its variants are public, so a caller can assemble one by hand. A write
/// builder is one of the three boundaries a predicate must cross to become
/// executable - `crate::plan::SelectBuilder::build` is the other - and the check
/// runs ahead of `canonical()`, `mentions_aggregate()` and the renderer, all
/// three of which recurse.
///
/// The write family adds **no** recursive shape of its own: `Insert` carries no
/// predicate, `Assignment` cannot contain another `Assignment`, `Returning`
/// wraps a flat `Projection`, and a `FieldPath` is already bounded by
/// `MAX_PATH_SEGMENTS`. The filter is the whole exposure.
fn check_filter_depth(filter: &Predicate) -> Result<(), WriteError> {
    let depth = filter.depth();
    if depth > MAX_PREDICATE_DEPTH {
        return Err(WriteError::FilterTooDeep { depth });
    }
    Ok(())
}

/// Sort the `SET` clauses by column and refuse a repeated one.
///
/// **Sorting is meaning-preserving here, and it is worth saying why rather than
/// assuming it.** Every assignment in one `UPDATE` reads the row's values as
/// they were *before* the statement, so the clauses do not chain:
/// `SET a = b, b = a` swaps the two columns, it does not set both to the old
/// `b`. Order is therefore not observable, and two callers who wrote the same
/// assignments in different orders must produce one statement or the
/// prepared-statement cache holds two entries for one operation.
///
/// A repeated column is a different matter and is refused: `PostgreSQL` rejects
/// `SET a = 1, a = 2` outright ("multiple assignments to same column"), and
/// silently keeping one of them would pick a winner by sort stability.
fn canonical_assignments(
    assignments: Vec<ColumnAssignment>,
) -> Result<Vec<ColumnAssignment>, WriteError> {
    if assignments.is_empty() {
        return Err(WriteError::EmptyAssignments);
    }
    let mut assignments = assignments;
    assignments.sort_by(|a, b| a.column.cmp(&b.column));
    if let Some(pair) = assignments
        .windows(2)
        .find(|pair| pair[0].column == pair[1].column)
    {
        return Err(WriteError::DuplicateColumn {
            column: pair[0].column.as_str().to_string(),
        });
    }
    Ok(assignments)
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// An insert with no rows. `INSERT ... VALUES` with an empty list is a
    /// syntax error, not an empty write.
    EmptyBatch,
    /// An insert whose rows write no columns.
    EmptyColumnList,
    /// An update with no `SET` clauses. `UPDATE t SET` is a syntax error.
    EmptyAssignments,
    /// The same column written twice in one row or one `SET` list.
    DuplicateColumn { column: String },
    /// More rows than [`MAX_INSERT_ROWS`].
    BatchTooLarge { rows: usize },
    /// More bind parameters than the named backend's protocol carries.
    BindBudgetExceeded { binds: usize, budget: BindBudget },
    /// Two rows of one insert write different column sets.
    ///
    /// **This is where a schema fact is handed back to the caller.** The shipped
    /// builder unions the columns across documents and fills every gap with a
    /// SQL `NULL` literal (`query.rs:4119-4135`), which is a decision it is not
    /// equipped to make: a column omitted from a document might want its DDL
    /// default - `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`
    /// (`query.rs:212`) is one the platform declares itself - and writing an
    /// explicit `NULL` overrides that default and violates the `NOT NULL`. Which
    /// of the two a gap means depends on the declared schema, which this crate
    /// does not have and must not guess at. So the batch is refused and the
    /// caller, who does have the schema, says which it meant: an explicit
    /// [`WriteValue::Null`] for the first, or a separate insert for the second.
    ColumnSetMismatch { row: usize },
    /// An aggregate projection was offered as a `RETURNING` list.
    AggregateReturning,
    /// An aggregate operand appeared in a write's filter.
    AggregateInFilter,
    /// The filter was nested past [`MAX_PREDICATE_DEPTH`].
    FilterTooDeep { depth: usize },
    /// `col = col <op> $n` with a non-numeric operand.
    ArithmeticOperandNotNumeric { found: &'static str },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBatch => f.write_str("an insert must carry at least one row"),
            Self::EmptyColumnList => f.write_str("an insert must write at least one column"),
            Self::EmptyAssignments => f.write_str("an update must carry at least one SET clause"),
            Self::DuplicateColumn { column } => write!(
                f,
                "the column '{column}' is written twice in one statement; SQL refuses \
                 multiple assignments to one column"
            ),
            Self::BatchTooLarge { rows } => write!(
                f,
                "an insert of {rows} rows exceeds the maximum of {MAX_INSERT_ROWS}"
            ),
            Self::BindBudgetExceeded { binds, budget } => write!(
                f,
                "this write binds {binds} parameters; one {} statement carries at most \
                 {}. Split the rows into smaller batches",
                budget.dialect_name(),
                budget.max()
            ),
            Self::ColumnSetMismatch { row } => write!(
                f,
                "row {row} writes a different column set from the first row. Whether an \
                 omitted column means SQL NULL or the column's declared default is a \
                 schema question this plan cannot answer; write the NULL explicitly or \
                 insert the rows separately"
            ),
            Self::AggregateReturning => f.write_str(
                "a RETURNING list projects the rows the write touched, so it cannot \
                 aggregate; the affected-row count comes from the command tag",
            ),
            Self::AggregateInFilter => f.write_str(
                "an aggregate operand is legal only in a HAVING position, not in a \
                 write's filter",
            ),
            Self::FilterTooDeep { depth } => write!(
                f,
                "the filter is nested {depth} deep, over the maximum of \
                 {MAX_PREDICATE_DEPTH}"
            ),
            Self::ArithmeticOperandNotNumeric { found } => write!(
                f,
                "'col = col <op> $n' needs a numeric operand, not {found}"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::IdentRole;

    fn column(name: &str) -> Ident {
        Ident::parse_as(name, IdentRole::Column).expect("valid column")
    }

    /// The canonical sort must be able to change something, or every
    /// determinism arm that rests on it passes vacuously. This is the in-crate
    /// half of the mutation proof for the `SET` list; the render-level half
    /// lives in `crate::render::postgres`.
    #[test]
    fn the_assignment_sort_can_actually_reorder() {
        let unsorted = vec![
            ColumnAssignment::new(column("zeta"), Assignment::bind(Literal::Int(1))),
            ColumnAssignment::new(column("alpha"), Assignment::bind(Literal::Int(2))),
        ];
        let before: Vec<String> = unsorted
            .iter()
            .map(|a| a.column.as_str().to_string())
            .collect();
        let after: Vec<String> = canonical_assignments(unsorted)
            .expect("canonical")
            .iter()
            .map(|a| a.column.as_str().to_string())
            .collect();
        assert_ne!(before, after, "the sort left an unsorted input untouched");
        assert_eq!(after[0], "alpha");
    }

    /// The two budgets must be different numbers and ordered the way
    /// [`BindBudget`]'s argument assumes, or "take the tighter of the two" and
    /// "hard-code Postgres" would be the same choice.
    #[test]
    fn the_two_bind_budgets_are_different_and_sqlite_is_the_tighter() {
        assert_ne!(BindBudget::POSTGRES.max(), BindBudget::SQLITE.max());
        assert!(BindBudget::SQLITE.max() < BindBudget::POSTGRES.max());
        assert_eq!(BindBudget::POSTGRES.max(), 65_535);
        assert_eq!(BindBudget::SQLITE.max(), 32_766);
    }

    /// A null consumes no bind parameter, which is what makes the budget a count
    /// of parameters rather than of cells.
    #[test]
    fn a_null_costs_no_bind_parameter() {
        assert_eq!(WriteValue::Null.bind_count(), 0);
        assert_eq!(WriteValue::Bind(Literal::Int(1)).bind_count(), 1);
        assert_eq!(Assignment::CurrentTimestamp.bind_count(), 0);
        assert_eq!(Assignment::null().bind_count(), 0);
        assert_eq!(
            Assignment::arithmetic(ArithmeticOp::Add, Literal::Int(1))
                .expect("numeric")
                .bind_count(),
            1
        );
    }
}
