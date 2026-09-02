//! The `PostgreSQL` lowering.
//!
//! # A backend that cannot serve a node refuses; it does not emulate
//!
//! SC-3's decision 2, and the refusals live here rather than above the backend
//! boundary: [`RenderError`] is this module's type, so no capability is
//! discovered by a dialect match in the frontend.
//!
//! The alternative is real - jOOQ emulates missing dialect features - and it is
//! rejected because its failure mode is silent. A provider that quietly
//! evaluates an untranslatable predicate outside the database returns
//! correct-looking rows having filtered them *after* they left the tenant's
//! boundary: a correctness and performance problem in an ordinary ORM, a
//! security failure here.
//!
//! # This is the only backend
//!
//! There is no `SQLite` lowering in this crate. Writing one would mean deciding,
//! among other things, whether `ILIKE` is emulated by `LIKE ... COLLATE NOCASE`,
//! which is what `query.rs` does today and what decision 2 says a backend must
//! not do. That is a divergence to settle in
//! `docs/reference/sqlite-divergences.md` with an arm comparing the documented
//! refusal set against the real one, not something to decide in passing while
//! building a grammar.

use crate::ident::Ident;
use crate::literal::Literal;
use crate::path::FieldPath;
use crate::plan::{DbPlan, Direction, NullOrder, OrderKey, RowLimit, Select};
use crate::predicate::{
    AggregateRef, CompareOp, MembershipOp, Operand, PatternOp, Predicate, TextPattern,
};
use crate::projection::{ProjectedField, ProjectionSource, SearchScalarKind};
use crate::render::{RenderedSql, ValueFormat};
use crate::search::{Search, SearchCriterion, VectorMetric};
use crate::write::{
    Assignment, ColumnAssignment, Delete, Insert, Returning, Update, WriteValue,
};
use core::fmt;

/// Why this backend refused a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The node is well-formed but this backend has no lowering for it.
    Unsupported {
        node: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { node, reason } => {
                write!(f, "the postgres backend cannot serve {node}: {reason}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Lower a plan.
///
/// # Errors
///
/// [`RenderError`] if the plan carries a node this backend does not serve.
pub fn render(plan: &DbPlan) -> Result<RenderedSql, RenderError> {
    match plan {
        DbPlan::Select(select) => render_select(select),
        DbPlan::Insert(insert) => render_insert(insert),
        DbPlan::Update(update) => render_update(update),
        DbPlan::Delete(delete) => render_delete(delete),
        DbPlan::Search(search) => render_search(search),
    }
}

/// Lower a read.
///
/// # Errors
///
/// [`RenderError`] if the plan carries a node this backend does not serve.
pub fn render_select(plan: &Select) -> Result<RenderedSql, RenderError> {
    let mut out = Writer::default();
    out.sql.push_str("SELECT ");
    if plan.is_distinct() {
        out.sql.push_str("DISTINCT ");
    }

    let mut first = true;
    for field in plan.projection().fields() {
        if !first {
            out.sql.push_str(", ");
        }
        first = false;
        // `None`: a read has no criterion, so a ranking scalar has no operands
        // to read. `Projection::rows` refuses one, so this is unreachable by
        // construction - and it is still a typed refusal rather than an
        // `unreachable!`, because the day that constructor gains a bypass is
        // the day this must say so rather than abort a worker thread.
        write_projected_field(&mut out, field, None)?;
    }

    out.sql.push_str(" FROM ");
    write_qualified_table(&mut out, plan.namespace(), plan.collection());

    // `Const(true)` is the canonical form of "no filter", so the clause is
    // omitted rather than emitted as `WHERE TRUE`. Two plans that differ only
    // in how the absent filter was spelled must produce one statement.
    if plan.filter() != &Predicate::always() {
        out.sql.push_str(" WHERE ");
        write_predicate(&mut out, plan.filter())?;
    }

    if !plan.group_by().is_empty() {
        out.sql.push_str(" GROUP BY ");
        let mut first = true;
        for key in plan.group_by() {
            if !first {
                out.sql.push_str(", ");
            }
            first = false;
            write_path(&mut out, key)?;
        }
    }

    if plan.having() != &Predicate::always() {
        out.sql.push_str(" HAVING ");
        write_predicate(&mut out, plan.having())?;
    }

    if !plan.order_by().is_empty() {
        out.sql.push_str(" ORDER BY ");
        let mut first = true;
        for key in plan.order_by() {
            if !first {
                out.sql.push_str(", ");
            }
            first = false;
            write_order_key(&mut out, key)?;
        }
    }

    // LIMIT and OFFSET are bound rather than interpolated. They are values, and
    // binding them means one prepared statement serves every page of a
    // paginated read instead of one per offset.
    out.sql.push_str(" LIMIT ");
    out.write_param(Literal::Int(plan.limit().get()));
    out.sql.push_str(" OFFSET ");
    out.write_param(Literal::Int(plan.offset().get()));

    Ok(RenderedSql::new(out.sql, out.params))
}

/// Lower an insert of one or more rows.
///
/// ```text
/// INSERT INTO "ns"."t" ("a", "b") VALUES ($1, NULL), ($2, $3) RETURNING ...
/// ```
///
/// The column list is canonical (sorted at construction) and every row emits its
/// values in that order, so two callers who wrote the same document with the
/// keys in different orders produce one statement and one binding order.
///
/// A [`WriteValue::Null`] emits the `NULL` keyword and binds nothing, which is
/// why the placeholder numbers are not a function of position. See
/// [`crate::write`] for why a null is a node rather than a parameter.
///
/// # Errors
///
/// [`RenderError`] if the `RETURNING` list carries a node this backend does not
/// serve.
pub fn render_insert(plan: &Insert) -> Result<RenderedSql, RenderError> {
    let mut out = Writer::default();
    out.sql.push_str("INSERT INTO ");
    write_qualified_table(&mut out, plan.namespace(), plan.collection());
    out.sql.push_str(" (");
    write_column_list(&mut out, plan.columns());
    out.sql.push_str(") VALUES ");

    let mut first_row = true;
    for row in plan.rows() {
        if !first_row {
            out.sql.push_str(", ");
        }
        first_row = false;
        out.sql.push('(');
        let mut first = true;
        for value in row {
            if !first {
                out.sql.push_str(", ");
            }
            first = false;
            write_write_value(&mut out, value);
        }
        out.sql.push(')');
    }

    write_returning(&mut out, plan.returning())?;
    Ok(RenderedSql::new(out.sql, out.params))
}

/// Lower a bounded update.
///
/// ```text
/// UPDATE "ns"."t" SET "a" = $1, "v" = "v" + $2
///   WHERE "id" IN (SELECT "id" FROM "ns"."t" WHERE .. LIMIT $3 FOR UPDATE)
///   RETURNING ...
/// ```
///
/// # Why the bound is a subquery over `id` and not a `LIMIT` on the `UPDATE`
///
/// `PostgreSQL` has no `LIMIT` on `UPDATE`, so the bound has to be expressed as
/// a set of rows chosen by a subquery. Every creator collection carries an
/// immutable `id TEXT PRIMARY KEY`, so this generalises the single-row shape
/// from one logical row to `n` and makes the bound unconditional: there is no
/// arm where the clause is absent.
///
/// Doing it in **one statement** rather than as a probe followed by a write is
/// the other half. The encrypted branch of `dispatch_update_many` reads the
/// target ids in one query and then issues per-row updates
/// (`write_pipeline.rs:329-360`, `crud/mod.rs:1278-1321`); the unencrypted
/// branch does neither. Here the row choice and the write are one statement
/// against one snapshot.
///
/// # `FOR UPDATE` is load-bearing, not decoration
///
/// A logical key removes `ctid`'s line-pointer reuse hazard, but it does not
/// make the lock optional. Without a lock, a concurrent update can change a
/// selected row so it no longer satisfies the original filter before the outer
/// write reaches it; the outer predicate names only `id` and would still match.
/// `FOR UPDATE` keeps selection and mutation tied to the same row. The shipped
/// encrypted-write probe uses the same lock for this reason.
///
/// The clause goes after `LIMIT`, which is where `PostgreSQL`'s `SELECT` grammar
/// puts a locking clause. The shipped probe likewise appends it to a statement
/// that already ends in `LIMIT $n OFFSET $n`.
///
/// One property is worth stating rather than leaving to be discovered: the
/// subquery has no `ORDER BY`, so the lock order is the scan order. Two
/// concurrent bounded updates over overlapping filters can therefore take their
/// locks in different orders. Pinning an order would cost a sort on every write
/// and is a trade to make with a measurement, not in passing.
///
/// # Errors
///
/// [`RenderError`] if the filter or the `RETURNING` list carries a node this
/// backend does not serve.
pub fn render_update(plan: &Update) -> Result<RenderedSql, RenderError> {
    let mut out = Writer::default();
    out.sql.push_str("UPDATE ");
    write_qualified_table(&mut out, plan.namespace(), plan.collection());
    out.sql.push_str(" SET ");
    write_assignments(&mut out, plan.assignments());
    write_bounded_target(
        &mut out,
        plan.namespace(),
        plan.collection(),
        plan.filter(),
        plan.limit(),
    )?;
    write_returning(&mut out, plan.returning())?;
    Ok(RenderedSql::new(out.sql, out.params))
}

/// Lower a bounded delete. Same bound, same lock, same argument as
/// [`render_update`].
///
/// # Errors
///
/// [`RenderError`] if the filter or the `RETURNING` list carries a node this
/// backend does not serve.
pub fn render_delete(plan: &Delete) -> Result<RenderedSql, RenderError> {
    let mut out = Writer::default();
    out.sql.push_str("DELETE FROM ");
    write_qualified_table(&mut out, plan.namespace(), plan.collection());
    write_bounded_target(
        &mut out,
        plan.namespace(),
        plan.collection(),
        plan.filter(),
        plan.limit(),
    )?;
    write_returning(&mut out, plan.returning())?;
    Ok(RenderedSql::new(out.sql, out.params))
}

/// Lower a ranked search.
///
/// ```text
/// SELECT .., "emb" <=> $1::vector AS "_distance" FROM "ns"."t"
///   WHERE ..  ORDER BY "emb" <=> $1::vector ASC  LIMIT $2
///
/// SELECT .., ST_Distance("loc", ST_MakePoint($1, $2)::geography) AS "_distance_m"
///   FROM "ns"."t"
///   WHERE ST_DWithin("loc", ST_MakePoint($1, $2)::geography, $3) AND ..
///   ORDER BY ST_Distance("loc", ST_MakePoint($1, $2)::geography) ASC  LIMIT $4
/// ```
///
/// # The `ORDER BY` re-emits the expression, and that is not redundancy
///
/// `PostgreSQL` would accept `ORDER BY "_distance"` - it resolves an output
/// column name in an `ORDER BY` - and the shipped geo builder writes exactly
/// that (`crates/zeroship-schema/src/query.rs:5082`) while the shipped vector
/// builder re-emits the expression (`:5013`). One of the two spellings has to
/// win here, and it is the expression, for a reason that is specific to this
/// family: an `hnsw` or `ivfflat` index answers a `k`-nearest query only when
/// the sort key is the indexed distance *expression*, and `PostGIS`'s `GiST` index
/// works the same way. Sorting by an alias leaves the planner to prove the two
/// are the same thing.
///
/// The operand is bound **once** and referenced twice; see [`Writer::bind`] for
/// what that saves.
///
/// # The radius is a criterion, not a filter
///
/// `ST_DWithin` is emitted before the caller's filter and unconditionally, so a
/// geo search cannot be built without its bound - the same argument that makes
/// a bounded write's `LIMIT` subquery unconditional. `ST_DWithin` rather than
/// `ST_Distance(..) <= r` is what lets the `GiST` index serve the predicate; the
/// two are equivalent on `geography` and only one is indexable.
///
/// # Errors
///
/// [`RenderError`] if the filter, the projection or a tiebreak carries a node
/// this backend does not serve. **No metric is refused here**: `pgvector`
/// serves all three, and the one a backend cannot serve is refused by *that*
/// backend - see [`crate::search`].
pub fn render_search(plan: &Search) -> Result<RenderedSql, RenderError> {
    let mut out = Writer::default();

    // The criterion's operands are bound FIRST, before the projection is
    // written, because the scalar in the select list is the first thing that
    // needs them and a slot must exist before it can be referenced. Binding
    // ahead of the walk also fixes the parameter order as a function of the
    // plan rather than of where in the projection the scalar happened to sort.
    let slots = bind_criterion(&mut out, plan.criterion());

    out.sql.push_str("SELECT ");
    let mut first = true;
    for field in plan.projection().fields() {
        if !first {
            out.sql.push_str(", ");
        }
        first = false;
        write_projected_field(&mut out, field, Some(&slots))?;
    }

    out.sql.push_str(" FROM ");
    write_qualified_table(&mut out, plan.namespace(), plan.collection());

    // The geo criterion contributes a WHERE term of its own; the vector one
    // does not, because a k-nearest search has no radius to bound it.
    let has_filter = plan.filter() != &Predicate::always();
    match &slots {
        CriterionSlots::Vector { .. } => {
            if has_filter {
                out.sql.push_str(" WHERE ");
                write_predicate(&mut out, plan.filter())?;
            }
        }
        CriterionSlots::Geo { .. } => {
            out.sql.push_str(" WHERE ");
            write_within_radius(&mut out, &slots);
            if has_filter {
                out.sql.push_str(" AND ");
                write_predicate(&mut out, plan.filter())?;
            }
        }
    }

    out.sql.push_str(" ORDER BY ");
    write_distance_expr(&mut out, &slots, plan.criterion().scalar())?;
    // Always explicit, and always ascending: every metric this family carries
    // is "smaller is nearer", including the negated inner product. The keyword
    // is written rather than left to the default so the statement says what it
    // means, for the same reason `write_order_key` spells NULLS FIRST/LAST.
    out.sql.push_str(" ASC");
    for key in plan.tiebreak() {
        out.sql.push_str(", ");
        write_order_key(&mut out, key)?;
    }

    // No OFFSET. A k-nearest search has no paginated form here and did not have
    // one before: `build_vector_search` and `build_spatial_near` emit LIMIT and
    // nothing else. See the note in `crate::search` on what that costs.
    out.sql.push_str(" LIMIT ");
    out.write_param(Literal::Int(plan.limit().get()));

    Ok(RenderedSql::new(out.sql, out.params))
}

/// The parameter slots a criterion's operands occupy.
///
/// Carrying the *slots* rather than the operands is what lets the distance
/// expression be written twice from one binding, and it also means the two
/// occurrences cannot disagree: there is one slot number, so there is one
/// value.
#[derive(Debug)]
enum CriterionSlots {
    Vector {
        column: Ident,
        metric: VectorMetric,
        query: usize,
    },
    Geo {
        column: Ident,
        longitude: usize,
        latitude: usize,
        radius: usize,
    },
}

/// Bind a criterion's operands, writing nothing.
fn bind_criterion(out: &mut Writer, criterion: &SearchCriterion) -> CriterionSlots {
    match criterion {
        SearchCriterion::Vector {
            column,
            query,
            metric,
        } => CriterionSlots::Vector {
            column: column.clone(),
            metric: *metric,
            query: out.bind(Literal::Vector(query.clone())),
        },
        SearchCriterion::Geo {
            column,
            point,
            radius,
        } => CriterionSlots::Geo {
            column: column.clone(),
            // Longitude first. `ST_MakePoint` is `(x, y)`, which is
            // `(longitude, latitude)` - the inverse of the `{lat, lng}` order
            // every layer above uses. The accessors are spelled in full at both
            // ends so the transposition has to be written to happen.
            longitude: out.bind(Literal::Float(point.longitude())),
            latitude: out.bind(Literal::Float(point.latitude())),
            radius: out.bind(Literal::Float(radius.get())),
        },
    }
}

/// `ST_MakePoint($lng, $lat)::geography`.
fn write_geography_point(out: &mut Writer, longitude: usize, latitude: usize) {
    out.sql.push_str("ST_MakePoint(");
    out.write_bound(longitude);
    out.sql.push_str(", ");
    out.write_bound(latitude);
    out.sql.push_str(")::geography");
}

/// The scalar a search ranks by, as an expression.
///
/// Every `PostgreSQL`-specific spelling in this family is in this function and
/// its two helpers: the three `pgvector` operators and the two `PostGIS`
/// functions. None of them appears in [`crate::search`], which is the property
/// that lets a second backend compute the same scalar by a completely different
/// mechanism - `SQLite` ranks a vector search by joining a `vec0` virtual table
/// and reading `v.distance`, which is not this expression with a different
/// operator.
fn write_distance_expr(
    out: &mut Writer,
    slots: &CriterionSlots,
    kind: SearchScalarKind,
) -> Result<(), RenderError> {
    match (slots, kind) {
        (
            CriterionSlots::Vector {
                column,
                metric,
                query,
            },
            SearchScalarKind::VectorDistance,
        ) => {
            out.sql.push_str(&quote(column));
            // The operator/opclass pairing is `pgvector`'s:
            // `<=>`/`vector_cosine_ops`, `<->`/`vector_l2_ops`,
            // `<#>`/`vector_ip_ops`. `<#>` returns the NEGATIVE inner product,
            // which is why one ascending sort ranks all three.
            out.sql.push_str(match metric {
                VectorMetric::Cosine => " <=> ",
                VectorMetric::L2 => " <-> ",
                VectorMetric::InnerProduct => " <#> ",
            });
            out.write_bound(*query);
            Ok(())
        }
        (
            CriterionSlots::Geo {
                column,
                longitude,
                latitude,
                ..
            },
            SearchScalarKind::GeoDistanceMetres,
        ) => {
            out.sql.push_str("ST_Distance(");
            out.sql.push_str(&quote(column));
            out.sql.push_str(", ");
            write_geography_point(out, *longitude, *latitude);
            out.sql.push(')');
            Ok(())
        }
        // Unreachable: `SearchBuilder::build` installs the scalar from
        // `criterion.scalar()`, so the pair always matches. Typed rather than
        // `unreachable!` for the reason the whole crate prefers that - a
        // panicking arm in the worker takes every co-tenanted app with it.
        _ => Err(RenderError::Unsupported {
            node: "a search scalar of a different kind from its criterion",
            reason: "the scalar and the criterion disagree about what is being ranked",
        }),
    }
}

/// `ST_DWithin("col", ST_MakePoint($lng, $lat)::geography, $radius)`.
fn write_within_radius(out: &mut Writer, slots: &CriterionSlots) {
    let CriterionSlots::Geo {
        column,
        longitude,
        latitude,
        radius,
    } = slots
    else {
        // The caller matched on the variant to get here; a vector criterion has
        // no radius term and never reaches this.
        return;
    };
    out.sql.push_str("ST_DWithin(");
    out.sql.push_str(&quote(column));
    out.sql.push_str(", ");
    write_geography_point(out, *longitude, *latitude);
    out.sql.push_str(", ");
    out.write_bound(*radius);
    out.sql.push(')');
}

/// Statement text under construction, plus the parameters bound so far.
///
/// One structure for both so a placeholder number can never be written without
/// the value it refers to having been pushed - the two are updated by the same
/// call.
#[derive(Debug, Default)]
struct Writer {
    sql: String,
    params: Vec<Literal>,
}

impl Writer {
    /// Bind a value and write its placeholder.
    ///
    /// The spelling comes from [`ValueFormat`] via the single exhaustive
    /// dispatch in the parent module, so the *type* of the value decides it and
    /// no call site gets to spell one itself.
    fn write_param(&mut self, value: Literal) {
        let slot = self.bind(value);
        self.write_bound(slot);
    }

    /// Bind a value **without** writing anything, returning its slot.
    ///
    /// Paired with [`Writer::write_bound`] for the one shape that needs a
    /// parameter in two places: a search's distance expression appears in the
    /// select list and again in the `ORDER BY`, and the operand is the same
    /// value both times. Binding it twice would be correct and would put a
    /// second copy of the query vector on the wire - for a 1536-dimension
    /// embedding, roughly 12 KB of duplicated text per search. Today's builder
    /// re-uses `$1` for exactly this reason
    /// (`crates/zeroship-schema/src/query.rs:5006-5013`).
    fn bind(&mut self, value: Literal) -> usize {
        self.params.push(value);
        self.params.len()
    }

    /// Write the placeholder for a slot [`Writer::bind`] already took.
    ///
    /// The spelling is read back off the bound value, so it goes through the
    /// same exhaustive dispatch as a first occurrence and a re-use cannot spell
    /// a cast the first occurrence did not.
    ///
    /// # Panics
    ///
    /// Only if `slot` was never bound, which no caller here can arrange: every
    /// slot comes from a [`Writer::bind`] on the same `Writer`, and the vector
    /// only grows.
    fn write_bound(&mut self, slot: usize) {
        let value = self
            .params
            .get(slot - 1)
            .expect("a slot is only ever written after bind() returned it");
        let placeholder = crate::render::placeholder_for(&PostgresValueFormat, slot, value);
        self.sql.push_str(&placeholder);
    }
}

/// `PostgreSQL`'s parameter spellings.
///
/// Every one of them is a bare `$n`, and the `bytes` arm is the interesting
/// one: today's builder wraps it as `decode($N, 'base64')::bytea`
/// (`query.rs:106-122`) because the parameter is a `String` carrying base64.
/// A [`Literal::Bytes`] is a `Vec<u8>`, so the driver binds it as binary
/// directly and the wrapper has nothing left to do. The fragment is deleted,
/// not moved - which is the test to apply to any "typed parameter" change:
/// if the SQL wrapper survives, the type did not replace the tag, it joined it.
#[derive(Debug, Clone, Copy)]
pub struct PostgresValueFormat;

impl ValueFormat for PostgresValueFormat {
    fn dialect_name(&self) -> &'static str {
        "postgres"
    }

    fn bool_placeholder(&self, slot: usize) -> String {
        format!("${slot}")
    }

    fn int_placeholder(&self, slot: usize) -> String {
        format!("${slot}")
    }

    fn float_placeholder(&self, slot: usize) -> String {
        format!("${slot}")
    }

    fn text_placeholder(&self, slot: usize) -> String {
        format!("${slot}")
    }

    fn bytes_placeholder(&self, slot: usize) -> String {
        format!("${slot}")
    }

    /// `$n::vector`, and the cast is not decoration.
    ///
    /// The `vector` type is created by `CREATE EXTENSION vector`, so its OID is
    /// per-database and is not a constant the driver can know at compile time.
    /// Binding the value as text and casting is what sidesteps the binary
    /// protocol's type-discovery handshake - the reason
    /// `crates/zeroship-schema/src/query.rs:4936-4940` gives for the same
    /// choice. Unlike the `bytes` wrapper this crate deleted, this one does
    /// real work and stays.
    fn vector_placeholder(&self, slot: usize) -> String {
        format!("${slot}::vector")
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    /// The platform-injected `id TEXT PRIMARY KEY`. It is stable across tuple
    /// versions and belongs to the ordinary column-grant surface.
    fn row_identity_column(&self) -> &'static str {
        "id"
    }
}

/// Quote an identifier.
///
/// The doubling is defence in depth rather than the fence: [`Ident`] refuses
/// every character outside `[A-Za-z0-9_]`, so a quote can never be present.
/// `quoting_is_unreachable_because_the_charset_forbids_it` asserts that pairing
/// holds, so if the charset is ever widened this stops being decorative.
fn quote(ident: &Ident) -> String {
    quote_raw(ident.as_str())
}

/// Quote a name this backend chose itself.
///
/// The **only** callers are [`ValueFormat::row_identity_column`]'s result and
/// [`quote`]. It deliberately does not take an [`Ident`], because the bounded
/// write identity is a spelling the backend owns, not an identifier a caller
/// may choose. No public function accepts a `&str` that reaches statement text.
fn quote_raw(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// `"namespace"."collection"`, or `"collection"` when there is no namespace.
fn write_qualified_table(out: &mut Writer, namespace: Option<&Ident>, collection: &Ident) {
    if let Some(namespace) = namespace {
        out.sql.push_str(&quote(namespace));
        out.sql.push('.');
    }
    out.sql.push_str(&quote(collection));
}

/// A comma-separated list of quoted identifiers.
///
/// Taken as a slice rather than read off the plan so the determinism mutation
/// arm can hand it an un-canonicalised list - the same reason
/// `write_predicate` is reachable from the tests here.
fn write_column_list(out: &mut Writer, columns: &[Ident]) {
    let mut first = true;
    for column in columns {
        if !first {
            out.sql.push_str(", ");
        }
        first = false;
        out.sql.push_str(&quote(column));
    }
}

/// `$n`, or the `NULL` keyword.
fn write_write_value(out: &mut Writer, value: &WriteValue) {
    match value {
        WriteValue::Bind(literal) => out.write_param(literal.clone()),
        // The keyword, not a parameter. `Literal` has no null variant and must
        // not gain one; see `crate::write`.
        WriteValue::Null => out.sql.push_str("NULL"),
    }
}

/// The `SET` list.
///
/// Taken as a slice for the same reason as [`write_column_list`].
fn write_assignments(out: &mut Writer, assignments: &[ColumnAssignment]) {
    let mut first = true;
    for assignment in assignments {
        if !first {
            out.sql.push_str(", ");
        }
        first = false;
        let column = quote(&assignment.column);
        out.sql.push_str(&column);
        out.sql.push_str(" = ");
        match &assignment.value {
            Assignment::Set(value) => write_write_value(out, value),
            Assignment::Arithmetic(arithmetic) => {
                // The left operand is the column being assigned. It is written
                // from the same `Ident`, not from a second one a caller could
                // supply, which is what keeps `a = b + 1` unrepresentable.
                out.sql.push_str(&column);
                out.sql.push(' ');
                out.sql.push_str(arithmetic.op().as_sql());
                out.sql.push(' ');
                // No `::numeric` cast: the shipped lowering needs one
                // (`query.rs:3816`) because its parameter is a `String`. A typed
                // integer or float parameter has nothing to cast.
                out.write_param(arithmetic.operand().clone());
            }
            Assignment::CurrentTimestamp => {
                out.sql.push_str(PostgresValueFormat.current_timestamp_expr());
            }
        }
    }
}

/// ` WHERE "id" IN (SELECT "id" FROM <table>[ WHERE ..] LIMIT $n FOR UPDATE)`
///
/// Emitted **unconditionally**, which is what makes an unbounded write
/// unrepresentable: the filter may simplify to `TRUE` and vanish, but the bound
/// and its clause cannot.
fn write_bounded_target(
    out: &mut Writer,
    namespace: Option<&Ident>,
    collection: &Ident,
    filter: &Predicate,
    limit: RowLimit,
) -> Result<(), RenderError> {
    let identity = quote_raw(PostgresValueFormat.row_identity_column());
    out.sql.push_str(" WHERE ");
    out.sql.push_str(&identity);
    out.sql.push_str(" IN (SELECT ");
    out.sql.push_str(&identity);
    out.sql.push_str(" FROM ");
    write_qualified_table(out, namespace, collection);
    if filter != &Predicate::always() {
        out.sql.push_str(" WHERE ");
        write_predicate(out, filter)?;
    }
    // Bound, then lock. `LIMIT` before a locking clause is the order
    // PostgreSQL's SELECT grammar requires.
    out.sql.push_str(" LIMIT ");
    out.write_param(Literal::Int(limit.get()));
    out.sql.push_str(" FOR UPDATE)");
    Ok(())
}

/// The `RETURNING` list, or nothing at all.
///
/// There is no arm that emits `*`: the only two shapes a [`Returning`] has are
/// an explicit [`crate::Projection`] and absence.
fn write_returning(out: &mut Writer, returning: &Returning) -> Result<(), RenderError> {
    let Some(projection) = returning.projection() else {
        return Ok(());
    };
    out.sql.push_str(" RETURNING ");
    let mut first = true;
    for field in projection.fields() {
        if !first {
            out.sql.push_str(", ");
        }
        first = false;
        write_projected_field(out, field, None)?;
    }
    Ok(())
}

/// `slots` carries the parameter slots a search's criterion already bound, so a
/// ranking scalar in the select list re-uses them instead of re-binding. It is
/// `None` for every family that has no criterion.
fn write_projected_field(
    out: &mut Writer,
    field: &ProjectedField,
    slots: Option<&CriterionSlots>,
) -> Result<(), RenderError> {
    match &field.source {
        ProjectionSource::Column(column) => out.sql.push_str(&quote(column)),
        ProjectionSource::Path(path) => write_path(out, path)?,
        // The parent column is deliberately NOT read: the sibling is selected
        // and aliased back to the parent's name, so `row[col]` holds the masked
        // string and there is no `<col>_masked` key for a caller to find.
        ProjectionSource::MaskedSibling { sibling, .. } => out.sql.push_str(&quote(sibling)),
        ProjectionSource::Aggregate(aggregate) => write_aggregate(out, aggregate),
        ProjectionSource::SearchScalar(kind) => {
            let Some(slots) = slots else {
                return Err(RenderError::Unsupported {
                    node: "a search scalar outside a search",
                    reason: "the scalar's operands live on the plan's criterion, and a \
                             plan without one has nothing to compute it from",
                });
            };
            write_distance_expr(out, slots, *kind)?;
        }
    }
    // The alias is always emitted, even when it repeats the column name. A
    // conditional `AS` would make the statement depend on whether two strings
    // happened to match, which is one more thing that has to be identical for
    // two equivalent plans to share a prepared statement.
    out.sql.push_str(" AS ");
    out.sql.push_str(&quote(&field.alias));
    Ok(())
}

fn write_aggregate(out: &mut Writer, aggregate: &AggregateRef) {
    out.sql.push_str(aggregate.func().as_sql());
    out.sql.push('(');
    if aggregate.is_distinct() {
        out.sql.push_str("DISTINCT ");
    }
    match aggregate.argument() {
        Some(column) => out.sql.push_str(&quote(column)),
        None => out.sql.push('*'),
    }
    out.sql.push(')');
}

fn write_path(out: &mut Writer, path: &FieldPath) -> Result<(), RenderError> {
    if path.is_nested() {
        // The shape is pinned by SC-3's shared grammar; the lowering is not
        // written. See `crate::path` for why guessing at it would mean settling
        // a PostgreSQL operator-resolution question without a server to settle
        // it against.
        return Err(RenderError::Unsupported {
            node: "a nested JSON field path",
            reason: "no lowering is written for nested access; the shape is reserved, \
                     not served",
        });
    }
    out.sql.push_str(&quote(path.root()));
    Ok(())
}

fn write_order_key(out: &mut Writer, key: &OrderKey) -> Result<(), RenderError> {
    write_path(out, &key.path)?;
    out.sql.push_str(match key.direction {
        Direction::Ascending => " ASC",
        Direction::Descending => " DESC",
    });
    // Always explicit. PostgreSQL's default is nulls-last on ASC and
    // nulls-first on DESC; SQLite's is nulls-first on both. Emitting the clause
    // unconditionally means the plan says what it means rather than inheriting
    // whichever engine ran it.
    out.sql.push_str(match key.nulls {
        NullOrder::First => " NULLS FIRST",
        NullOrder::Last => " NULLS LAST",
    });
    Ok(())
}

fn write_operand(out: &mut Writer, operand: &Operand) -> Result<(), RenderError> {
    match operand {
        Operand::Path(path) => write_path(out, path),
        Operand::Lit(value) => {
            out.write_param(value.clone());
            Ok(())
        }
        Operand::Aggregate(aggregate) => {
            write_aggregate(out, aggregate);
            Ok(())
        }
    }
}

fn write_pattern_operand(out: &mut Writer, pattern: &TextPattern) {
    // A pattern is a value. It is bound, never interpolated, so a caller's
    // wildcards stay data and a caller's quote character is not a syntax
    // question.
    out.write_param(Literal::Text(pattern.as_str().to_string()));
}

fn write_predicate(out: &mut Writer, predicate: &Predicate) -> Result<(), RenderError> {
    match predicate {
        Predicate::Const(true) => {
            out.sql.push_str("TRUE");
            Ok(())
        }
        Predicate::Const(false) => {
            out.sql.push_str("FALSE");
            Ok(())
        }
        Predicate::And(children) => write_connective(out, children, " AND "),
        Predicate::Or(children) => write_connective(out, children, " OR "),
        Predicate::Not(inner) => {
            out.sql.push_str("NOT (");
            write_predicate(out, inner)?;
            out.sql.push(')');
            Ok(())
        }
        Predicate::Compare { lhs, op, rhs } => {
            write_operand(out, lhs)?;
            out.sql.push_str(match op {
                CompareOp::Eq => " = ",
                CompareOp::Ne => " <> ",
                CompareOp::Lt => " < ",
                CompareOp::Lte => " <= ",
                CompareOp::Gt => " > ",
                CompareOp::Gte => " >= ",
            });
            write_operand(out, rhs)
        }
        Predicate::Membership { lhs, op, set } => {
            write_operand(out, lhs)?;
            out.sql.push_str(match op {
                MembershipOp::In => " IN (",
                MembershipOp::NotIn => " NOT IN (",
            });
            let mut first = true;
            for value in set.values() {
                if !first {
                    out.sql.push_str(", ");
                }
                first = false;
                out.write_param(value.clone());
            }
            out.sql.push(')');
            Ok(())
        }
        Predicate::Pattern {
            lhs,
            op,
            pattern,
            escape,
        } => {
            write_operand(out, lhs)?;
            out.sql.push_str(match op {
                PatternOp::Like => " LIKE ",
                PatternOp::NotLike => " NOT LIKE ",
                PatternOp::ILike => " ILIKE ",
                PatternOp::NotILike => " NOT ILIKE ",
            });
            write_pattern_operand(out, pattern);
            if let Some(escape) = escape {
                out.sql.push_str(" ESCAPE ");
                out.write_param(Literal::Text(escape.get().to_string()));
            }
            Ok(())
        }
        Predicate::IsNull { operand, negated } => {
            write_operand(out, operand)?;
            out.sql
                .push_str(if *negated { " IS NOT NULL" } else { " IS NULL" });
            Ok(())
        }
    }
}

fn write_connective(
    out: &mut Writer,
    children: &[Predicate],
    joiner: &str,
) -> Result<(), RenderError> {
    out.sql.push('(');
    let mut first = true;
    for child in children {
        if !first {
            out.sql.push_str(joiner);
        }
        first = false;
        write_predicate(out, child)?;
    }
    out.sql.push(')');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::IdentRole;

    fn column(name: &str) -> Ident {
        Ident::parse_as(name, IdentRole::Column).expect("valid column")
    }

    fn eq(name: &str, value: i64) -> Predicate {
        Predicate::Compare {
            lhs: Operand::column(column(name)),
            op: CompareOp::Eq,
            rhs: Operand::Lit(Literal::Int(value)),
        }
    }

    fn render_raw(predicate: &Predicate) -> String {
        let mut out = Writer::default();
        write_predicate(&mut out, predicate).expect("renderable");
        out.sql
    }

    fn render_columns_raw(columns: &[Ident]) -> String {
        let mut out = Writer::default();
        write_column_list(&mut out, columns);
        out.sql
    }

    fn render_assignments_raw(assignments: &[ColumnAssignment]) -> (String, Vec<Literal>) {
        let mut out = Writer::default();
        write_assignments(&mut out, assignments);
        (out.sql, out.params)
    }

    /// THE MUTATION ARM.
    ///
    /// SC-3 requires the determinism check to be paired with "a mutation that
    /// deletes the canonical sort and proves the arm turns red". This is that
    /// mutation, executed rather than described: the two permutations below are
    /// rendered through the same private writer the real path uses, but WITHOUT
    /// `Predicate::canonical` in front of it - which is exactly the state the
    /// tree would be in if the sort were deleted.
    ///
    /// If this assertion ever fails, the permutation fixtures have stopped
    /// being different inputs and every determinism arm built on them is
    /// passing vacuously.
    #[test]
    fn permuted_conjuncts_diverge_without_the_canonical_sort() {
        let forwards = Predicate::And(vec![eq("alpha", 1), eq("beta", 2)]);
        let backwards = Predicate::And(vec![eq("beta", 2), eq("alpha", 1)]);
        assert_ne!(
            render_raw(&forwards),
            render_raw(&backwards),
            "the two permutations rendered identically without canonicalisation, so \
             the determinism arms that use them prove nothing"
        );
    }

    /// The control for the arm above: with canonicalisation the same two
    /// inputs converge.
    #[test]
    fn permuted_conjuncts_converge_with_the_canonical_sort() {
        let forwards = Predicate::And(vec![eq("alpha", 1), eq("beta", 2)]).canonical();
        let backwards = Predicate::And(vec![eq("beta", 2), eq("alpha", 1)]).canonical();
        assert_eq!(render_raw(&forwards), render_raw(&backwards));
    }

    /// THE MUTATION ARM FOR AN INSERT'S COLUMN LIST.
    ///
    /// The determinism arm in `tests/determinism.rs` asserts two permutations of
    /// one document render identically. On its own that is also true of a
    /// renderer that was never given two different inputs, so this renders the
    /// two permutations through the same private writer the real path uses,
    /// WITHOUT the canonical sort `InsertBuilder::build` applies - which is
    /// exactly the state the tree would be in if that sort were deleted.
    #[test]
    fn permuted_insert_columns_diverge_without_the_canonical_sort() {
        let forwards = [column("name"), column("email")];
        let backwards = [column("email"), column("name")];
        assert_ne!(
            render_columns_raw(&forwards),
            render_columns_raw(&backwards),
            "the two permutations rendered identically without canonicalisation, so \
             the determinism arms that use them prove nothing"
        );
    }

    /// The control for the arm above: with the sort applied, the same two
    /// inputs converge.
    #[test]
    fn permuted_insert_columns_converge_with_the_canonical_sort() {
        let mut forwards = vec![column("name"), column("email")];
        let mut backwards = vec![column("email"), column("name")];
        forwards.sort();
        backwards.sort();
        assert_eq!(render_columns_raw(&forwards), render_columns_raw(&backwards));
    }

    /// THE MUTATION ARM FOR AN UPDATE'S SET LIST, same shape.
    ///
    /// This one also proves the divergence is visible in the PARAMETER order and
    /// not only in the statement text: a non-canonical `SET` list binds the
    /// values in the authored order, so two callers would send different
    /// arguments for the same logical update.
    #[test]
    fn permuted_assignments_diverge_without_the_canonical_sort() {
        let name = ColumnAssignment::new(column("name"), Assignment::bind(Literal::Int(7)));
        let score = ColumnAssignment::new(column("score"), Assignment::bind(Literal::Int(9)));
        let forwards = [name.clone(), score.clone()];
        let backwards = [score, name];
        let (a_sql, a_params) = render_assignments_raw(&forwards);
        let (b_sql, b_params) = render_assignments_raw(&backwards);
        assert_ne!(
            a_sql, b_sql,
            "the two permutations rendered identically without canonicalisation, so \
             the determinism arms that use them prove nothing"
        );
        assert_ne!(
            a_params, b_params,
            "a non-canonical SET list must also permute the parameters"
        );
    }

    /// The control for the arm above.
    #[test]
    fn permuted_assignments_converge_with_the_canonical_sort() {
        let name = ColumnAssignment::new(column("name"), Assignment::bind(Literal::Int(7)));
        let score = ColumnAssignment::new(column("score"), Assignment::bind(Literal::Int(9)));
        let mut forwards = vec![name.clone(), score.clone()];
        let mut backwards = vec![score, name];
        forwards.sort_by(|a, b| a.column.cmp(&b.column));
        backwards.sort_by(|a, b| a.column.cmp(&b.column));
        assert_eq!(
            render_assignments_raw(&forwards),
            render_assignments_raw(&backwards)
        );
    }

    /// The quote-doubling in [`quote`] is unreachable, and it is worth knowing
    /// that it is unreachable *because of the charset* rather than by luck. If
    /// [`Ident`] ever admits a quote, this test fails and the doubling stops
    /// being decorative.
    #[test]
    fn quoting_is_unreachable_because_the_charset_forbids_it() {
        for role in [IdentRole::Collection, IdentRole::Column, IdentRole::Alias] {
            assert!(
                Ident::parse_as("a\"b", role).is_err(),
                "a quote reached an identifier in role {role}"
            );
            assert!(Ident::parse_as("a'b", role).is_err());
            assert!(Ident::parse_as("a b", role).is_err());
        }
        assert_eq!(quote(&column("users")), "\"users\"");
    }
}
