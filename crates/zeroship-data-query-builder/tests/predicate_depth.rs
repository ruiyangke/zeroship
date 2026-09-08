//! Depth is the bound that matters, and it is checked before anything recurses.
//!
//! # Why this file exists at all
//!
//! `MAX_MEMBERSHIP_LIST_LEN` bounds a predicate's WIDTH. Until
//! [`MAX_PREDICATE_DEPTH`] there was nothing bounding its HEIGHT, and height is
//! the one that ends the process: a wide input is heap work, a deep one is stack
//! frames, and Rust has no recursion limit, so the failure is an abort rather
//! than an error anyone can catch. The worker runs many apps per thread, so that
//! abort is not one failed request.
//!
//! The translator this IR replaces already carries the bound -
//! `MAX_FILTER_NESTING_DEPTH = 16` at `query.rs:604`, enforced by
//! `count_clause_budget` at `query.rs:5666`. Shipping the IR without it would
//! have been a regression, not a missing hardening.
//!
//! # What is NOT here, deliberately
//!
//! **There is no arm that builds a tree deep enough to overflow the stack.** A
//! test that aborts the process is worse than no test: it takes the whole test
//! binary with it, so every other arm in the run reports nothing, and it cannot
//! be made to fail cleanly. These arms prove the REFUSAL, at one past the limit,
//! with a control at exactly the limit. The refusal is what makes the overflow
//! unreachable; reproducing the overflow would only re-measure Rust.
//!
//! Every arm declares the number of items it ruled on and a floor.

use zeroship_data_query_builder::{
    Assignment, ColumnAssignment, CompareOp, Delete, Ident, IdentRole, Literal, Operand, PlanError,
    Predicate, PredicateError, ProjectedField, Projection, Returning, RowLimit, Select, Update,
    WriteError, MAX_PREDICATE_DEPTH,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn collection() -> Ident {
    Ident::parse_as("users", IdentRole::Collection).expect("valid collection")
}

fn rows() -> Projection {
    Projection::rows(vec![
        ProjectedField::column(column("name")).expect("projectable")
    ])
    .expect("row projection")
}

fn leaf() -> Predicate {
    Predicate::compare(
        Operand::column(column("age")),
        CompareOp::Eq,
        Operand::Lit(Literal::Int(1)),
    )
}

/// A tree of exactly `depth`, built from the PUBLIC variants so it bypasses the
/// smart constructors' own check.
///
/// The shape is an ALTERNATING `And`/`Or` chain with two distinct children at
/// every level, and each of those properties is load-bearing against a different
/// simplification in `canonical()`:
///
/// * **alternating**, because same-connective nesting is FLATTENED, so an `And`
///   spine collapses to depth 2;
/// * **two children**, because a one-child connective is UNWRAPPED, so
///   `And([Or([x])])` collapses all the way to `x`;
/// * **distinct** leaves, because children are sorted and DEDUPLICATED.
///
/// A `Not` spine fails the same way and was the first thing tried here: double
/// negations collapse recursively, so a chain of fifteen `Not`s canonicalises to
/// one. The render arm below caught it, which is the point of having an arm that
/// looks at the statement rather than at the plan.
///
/// None of that weakens the arms that check the REFUSAL: the bound is measured
/// on the authored tree, before `canonical()` runs, because `canonical()` is
/// itself one of the recursive surfaces. The shape matters only where a tree has
/// to survive canonicalisation to be observed.
fn spine(depth: usize) -> Predicate {
    assert!(depth >= 1, "a tree is at least a leaf");
    if depth == 1 {
        return leaf();
    }
    let sibling = Predicate::compare(
        Operand::column(column(&format!("c{depth}"))),
        CompareOp::Eq,
        Operand::Lit(Literal::Int(i64::try_from(depth).expect("small"))),
    );
    let children = vec![sibling, spine(depth - 1)];
    if depth.is_multiple_of(2) {
        Predicate::And(children)
    } else {
        Predicate::Or(children)
    }
}

/// The generator has to produce what it claims, or every arm below is ruling on
/// the wrong depth. This is the control for the controls.
#[test]
fn the_depth_generator_produces_the_depth_it_claims() {
    let mut ruled_on = 0_usize;
    for depth in [1_usize, 2, 8, MAX_PREDICATE_DEPTH, MAX_PREDICATE_DEPTH + 1] {
        assert_eq!(spine(depth).depth(), depth, "spine({depth}) mismeasured");
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 5);
    println!("ruled on {ruled_on} generated depths");
}

/// THE ARM. One past the limit is a typed refusal from every composing
/// constructor, with the measured depth in the error so the caller can see how
/// far over they were.
#[test]
fn a_tree_one_past_the_limit_is_refused_by_the_smart_constructors() {
    // `spine(MAX)` under one more connective is `MAX + 1`.
    let over = spine(MAX_PREDICATE_DEPTH);
    let expected = PredicateError::TooDeep {
        depth: MAX_PREDICATE_DEPTH + 1,
    };

    let mut ruled_on = 0_usize;
    assert_eq!(
        Predicate::and(vec![over.clone()]).expect_err("must refuse"),
        expected
    );
    ruled_on += 1;
    assert_eq!(
        Predicate::or(vec![over.clone()]).expect_err("must refuse"),
        expected
    );
    ruled_on += 1;
    assert_eq!(Predicate::negate(over).expect_err("must refuse"), expected);
    ruled_on += 1;

    assert_eq!(ruled_on, 3, "every composing constructor must be ruled on");
    println!(
        "ruled on {ruled_on} constructors at depth {}",
        MAX_PREDICATE_DEPTH + 1
    );
}

/// THE CONTROL. Exactly at the limit must succeed, or the refusal above is a
/// blanket ban that proves nothing about depth.
#[test]
fn a_tree_at_exactly_the_limit_is_accepted() {
    let at = spine(MAX_PREDICATE_DEPTH - 1);
    let mut ruled_on = 0_usize;
    for built in [
        Predicate::and(vec![at.clone()]),
        Predicate::or(vec![at.clone()]),
        Predicate::negate(at),
    ] {
        let built = built.expect("exactly at the limit must be accepted");
        assert!(
            built.depth() <= MAX_PREDICATE_DEPTH,
            "canonicalisation must not deepen a tree: got {}",
            built.depth()
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);
    println!("ruled on {ruled_on} constructors at depth {MAX_PREDICATE_DEPTH}");
}

/// The constructors are not the only door. `Predicate`'s variants are public, so
/// a caller can assemble an over-deep tree by hand - and the plan builders are
/// the boundary every predicate must cross to reach a renderer, a `canonical()`
/// sort, or the derived `Ord`. All four boundaries are ruled on: a read's filter
/// and its HAVING, an update's filter, and a delete's.
#[test]
fn a_hand_built_over_deep_tree_is_refused_at_every_plan_boundary() {
    let over = spine(MAX_PREDICATE_DEPTH + 1);
    let depth = MAX_PREDICATE_DEPTH + 1;
    let mut ruled_on = 0_usize;

    assert_eq!(
        Select::builder(collection(), rows())
            .filter(over.clone())
            .build()
            .expect_err("must refuse"),
        PlanError::PredicateTooDeep {
            position: "filter",
            depth
        }
    );
    ruled_on += 1;

    // HAVING is checked too. It is refused for depth BEFORE the
    // `HavingWithoutAggregate` rule runs, which is the ordering that matters: a
    // depth check placed after another rule is a check the other rule can skip.
    assert_eq!(
        Select::builder(collection(), rows())
            .having(over.clone())
            .build()
            .expect_err("must refuse"),
        PlanError::PredicateTooDeep {
            position: "having",
            depth
        }
    );
    ruled_on += 1;

    assert_eq!(
        Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(1))
            ))
            .filter(over.clone())
            .build()
            .expect_err("must refuse"),
        WriteError::FilterTooDeep { depth }
    );
    ruled_on += 1;

    assert_eq!(
        Delete::builder(collection(), RowLimit::default(), Returning::nothing())
            .filter(over)
            .build()
            .expect_err("must refuse"),
        WriteError::FilterTooDeep { depth }
    );
    ruled_on += 1;

    assert_eq!(ruled_on, 4, "every plan boundary must be ruled on");
    println!("ruled on {ruled_on} plan boundaries");
}

/// The control for the arm above: the same four boundaries accept a tree at
/// exactly the limit.
#[test]
fn every_plan_boundary_accepts_a_tree_at_exactly_the_limit() {
    let at = spine(MAX_PREDICATE_DEPTH);
    assert_eq!(at.depth(), MAX_PREDICATE_DEPTH);
    let mut ruled_on = 0_usize;

    assert!(Select::builder(collection(), rows())
        .filter(at.clone())
        .build()
        .is_ok());
    ruled_on += 1;

    // A HAVING at the limit still fails the aggregate rule, which is the RIGHT
    // failure - and asserting on the specific variant is what shows the depth
    // check let it through rather than refusing it for the wrong reason.
    assert_eq!(
        Select::builder(collection(), rows())
            .having(at.clone())
            .build()
            .expect_err("HAVING needs an aggregate projection"),
        PlanError::HavingWithoutAggregate
    );
    ruled_on += 1;

    assert!(
        Update::builder(collection(), RowLimit::default(), Returning::nothing())
            .set(ColumnAssignment::new(
                column("name"),
                Assignment::bind(Literal::Int(1))
            ))
            .filter(at.clone())
            .build()
            .is_ok()
    );
    ruled_on += 1;

    assert!(
        Delete::builder(collection(), RowLimit::default(), Returning::nothing())
            .filter(at)
            .build()
            .is_ok()
    );
    ruled_on += 1;

    assert_eq!(ruled_on, 4);
    println!("ruled on {ruled_on} plan boundaries at the limit");
}

/// A plan accepted at the limit must also RENDER, because the renderer is one of
/// the recursive surfaces the bound exists to protect. An accepted plan the
/// renderer could not survive would mean the bound was set in the wrong place.
///
/// The count is the discriminating part: it proves the depth reached the
/// statement rather than being canonicalised away, which is the failure the
/// first version of this file had.
#[test]
fn a_plan_at_the_limit_renders_with_its_full_depth() {
    let at = spine(MAX_PREDICATE_DEPTH);
    let plan = Select::builder(collection(), rows())
        .filter(at)
        .build()
        .expect("at the limit");
    let rendered =
        zeroship_data_query_builder::render::postgres::render_select(&plan).expect("renders");
    let sql = rendered.sql();
    // Every level has exactly two children, so it contributes exactly one
    // joiner; a leaf contributes none.
    let connectives = sql.matches(" AND ").count() + sql.matches(" OR ").count();
    assert_eq!(
        connectives,
        MAX_PREDICATE_DEPTH - 1,
        "the tree did not survive canonicalisation to the statement: {sql}"
    );
    println!("ruled on 1 statement carrying {connectives} nested connectives");
}

/// The IR's bound must not be TIGHTER than the translator's, or filters the
/// current system accepts stop being expressible. It is the same number for the
/// same reason, and this arm pins the pairing rather than the value.
#[test]
fn the_ir_bound_matches_the_translator_it_replaces() {
    assert_eq!(
        MAX_PREDICATE_DEPTH, 16,
        "MAX_FILTER_NESTING_DEPTH is 16 at crates/zeroship-data-query-builder/src/compile.rs:604; a \
         replacement that refuses filters the current translator accepts is a \
         regression, whatever the new number's merits"
    );
    println!("ruled on 1 bound");
}
