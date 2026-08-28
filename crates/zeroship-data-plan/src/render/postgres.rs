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
use crate::plan::{DbPlan, Direction, NullOrder, OrderKey, Select};
use crate::predicate::{
    AggregateRef, CompareOp, MembershipOp, Operand, PatternOp, Predicate, TextPattern,
};
use crate::projection::{ProjectedField, ProjectionSource};
use crate::render::{RenderedSql, ValueFormat};
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
    if let Some(namespace) = plan.namespace() {
        out.sql.push_str(&quote(namespace));
        out.sql.push('.');
    }
    out.sql.push_str(&quote(plan.collection()));

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
}

/// Quote an identifier.
///
/// The doubling is defence in depth rather than the fence: [`Ident`] refuses
/// every character outside `[A-Za-z0-9_]`, so a quote can never be present.
/// `quoting_is_unreachable_because_the_charset_forbids_it` asserts that pairing
/// holds, so if the charset is ever widened this stops being decorative.
fn quote(ident: &Ident) -> String {
    let mut out = String::with_capacity(ident.as_str().len() + 2);
    out.push('"');
    for ch in ident.as_str().chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
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
