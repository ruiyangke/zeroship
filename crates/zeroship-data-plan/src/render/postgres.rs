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
use crate::projection::{ProjectedField, ProjectionSource};
use crate::render::{RenderedSql, ValueFormat};
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
        write_projected_field(&mut out, field)?;
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
///   WHERE "ctid" IN (SELECT "ctid" FROM "ns"."t" WHERE .. LIMIT $3 FOR UPDATE)
///   RETURNING ...
/// ```
///
/// # Why the bound is a subquery over `ctid` and not a `LIMIT` on the `UPDATE`
///
/// `PostgreSQL` has no `LIMIT` on `UPDATE`, so the bound has to be expressed as
/// a set of rows chosen by a subquery. That is the shape `query.rs` already uses
/// for the single-row case (`WHERE ctid = (SELECT ctid ... LIMIT 1)`,
/// `query.rs:4004-4007`); this generalises it from one row to `n` and makes it
/// unconditional, so there is no arm where the clause is absent.
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
/// A `ctid` names a physical tuple, not a row: once a tuple is dead and `VACUUM`
/// has reclaimed the line pointer, the same `ctid` can name a **different** row.
/// Without a lock, the tuples the subquery chose could be replaced between the
/// scan and the update. `FOR UPDATE` locks them, which is exactly why
/// `build_write_target_probe` appends it on the `PostgreSQL` arm
/// (`query.rs:2876-2878`).
///
/// The clause goes after `LIMIT`, which is where `PostgreSQL`'s `SELECT` grammar
/// puts a locking clause, and where `query.rs:2877` puts it - it appends to a
/// statement that already ends in `LIMIT $n OFFSET $n`.
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
        let slot = self.params.len() + 1;
        let placeholder = crate::render::placeholder_for(&PostgresValueFormat, slot, &value);
        self.params.push(value);
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

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    /// `ctid`. `PostgreSQL`'s physical row locator, the same one
    /// `query.rs:4000` and `:4285` reach for on the `SqlDialect::Postgres` arm.
    fn row_identity_column(&self) -> &'static str {
        "ctid"
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
/// [`quote`]. It deliberately does not take an [`Ident`], because `ctid` is not
/// an identifier a caller may name - it is a spelling the backend owns - and
/// deliberately does not take a caller string either, because nothing in this
/// crate has one to give it: no public function accepts a `&str` that reaches
/// statement text.
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

/// ` WHERE "ctid" IN (SELECT "ctid" FROM <table>[ WHERE ..] LIMIT $n FOR UPDATE)`
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
        write_projected_field(out, field)?;
    }
    Ok(())
}

fn write_projected_field(out: &mut Writer, field: &ProjectedField) -> Result<(), RenderError> {
    match &field.source {
        ProjectionSource::Column(column) => out.sql.push_str(&quote(column)),
        ProjectionSource::Path(path) => write_path(out, path)?,
        // The parent column is deliberately NOT read: the sibling is selected
        // and aliased back to the parent's name, so `row[col]` holds the masked
        // string and there is no `<col>_masked` key for a caller to find.
        ProjectionSource::MaskedSibling { sibling, .. } => out.sql.push_str(&quote(sibling)),
        ProjectionSource::Aggregate(aggregate) => write_aggregate(out, aggregate),
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
