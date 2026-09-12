//! Search grammar and PostgreSQL rendering contracts. Database execution is
//! covered by the ORM PostgreSQL search suites.

use zeroship_data_orm::sql::render::postgres::{RenderError, render, render_search};
use zeroship_data_orm::sql::{
    CompareOp, DbPlan, Direction, GeoPoint, Ident, IdentRole, Literal, LiteralError,
    MAX_PREDICATE_DEPTH, MAX_RADIUS_METRES, MAX_ROW_LIMIT, MAX_VECTOR_DIMS, NullOrder, Operand,
    OrderKey, PlanError, Predicate, ProjectedField, Projection, QueryVector, RadiusMetres,
    RowLimit, Search, SearchCriterion, SearchError, SearchScalarKind, VectorMetric,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn collection(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Collection).expect("valid collection")
}

fn rows(names: &[&str]) -> Projection {
    Projection::rows(
        names
            .iter()
            .map(|n| ProjectedField::column(column(n)).expect("field"))
            .collect(),
    )
    .expect("row projection")
}

fn vector(values: &[f32]) -> QueryVector {
    QueryVector::new(values).expect("valid query vector")
}

fn vector_criterion(metric: VectorMetric) -> SearchCriterion {
    SearchCriterion::Vector {
        column: column("embedding"),
        query: vector(&[0.1, 0.2, 0.3]),
        metric,
    }
}

fn geo_criterion() -> SearchCriterion {
    SearchCriterion::Geo {
        column: column("location"),
        point: GeoPoint::new(51.5, -0.12).expect("valid point"),
        radius: RadiusMetres::new(1_000.0).expect("valid radius"),
    }
}

fn eq(name: &str, value: i64) -> Predicate {
    Predicate::Compare {
        lhs: Operand::column(column(name)),
        op: CompareOp::Eq,
        rhs: Operand::Lit(Literal::Int(value)),
    }
}

// ---------------------------------------------------------------------------
// The shape of the lowering
// ---------------------------------------------------------------------------

/// The vector lowering, pinned whole.
///
/// The three properties this fixture exists to hold, none of which is visible
/// from a builder call:
///
/// * the query vector is bound **once** (`$1`) and referenced twice;
/// * the `ORDER BY` re-emits the distance *expression*, not the output alias,
///   which is what lets an `hnsw`/`ivfflat` index serve the sort;
/// * the direction is written out rather than left to the default.
#[test]
fn a_vector_search_binds_the_query_once_and_orders_by_the_expression() {
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .namespace(Ident::parse_as("app", IdentRole::Namespace).expect("ns"))
    .limit(RowLimit::new(10).expect("limit"))
    .build()
    .expect("buildable");

    let sql = render_search(&plan).expect("renderable");

    assert!(
        sql.sql()
            .contains(r#""embedding" <=> $1::vector AS "_distance""#),
        "the scalar must project the distance expression under its alias: {}",
        sql.sql()
    );
    assert!(
        sql.sql()
            .contains(r#"ORDER BY "embedding" <=> $1::vector ASC"#),
        "the ORDER BY must re-emit the expression, not the alias: {}",
        sql.sql()
    );
    assert!(
        !sql.sql().contains(r#"ORDER BY "_distance""#),
        "ordering by the output alias leaves the planner to prove the two are the \
         same thing: {}",
        sql.sql()
    );

    // Two occurrences of `$1`, one bound value. This is the property that made
    // `placeholder_slots` necessary; see the assertion pair below.
    assert_eq!(
        sql.sql().matches("$1::vector").count(),
        2,
        "the distance expression appears in the select list and the ORDER BY"
    );
    assert_eq!(
        sql.params().len(),
        2,
        "one vector and one limit, not two vectors: {:?}",
        sql.params()
    );
    assert!(matches!(sql.params()[0], Literal::Vector(_)));
    assert_eq!(sql.params()[1], Literal::Int(10));
}

/// The geo lowering, pinned whole.
///
/// `ST_DWithin` is the indexable predicate and `ST_Distance` the ordering, both
/// over one point that is bound once. The longitude precedes the latitude,
/// which is `ST_MakePoint`'s `(x, y)` order and the inverse of every layer
/// above.
#[test]
fn a_geo_search_bounds_by_st_dwithin_and_ranks_by_st_distance() {
    let plan = Search::builder(collection("places"), geo_criterion(), rows(&["name"]))
        .limit(RowLimit::new(25).expect("limit"))
        .build()
        .expect("buildable");

    let sql = render_search(&plan).expect("renderable");

    assert!(
        sql.sql().contains(
            r#"ST_Distance("location", ST_MakePoint($1, $2)::geography) AS "_distance_m""#
        ),
        "{}",
        sql.sql()
    );
    assert!(
        sql.sql()
            .contains(r#"WHERE ST_DWithin("location", ST_MakePoint($1, $2)::geography, $3)"#),
        "the radius must be an indexable ST_DWithin, not a distance comparison: {}",
        sql.sql()
    );
    assert!(
        sql.sql()
            .contains(r#"ORDER BY ST_Distance("location", ST_MakePoint($1, $2)::geography) ASC"#),
        "{}",
        sql.sql()
    );

    // $1 is the LONGITUDE. A transposed pair is two valid coordinates and a
    // different place on Earth, so the order is asserted against the values.
    assert_eq!(
        sql.params()[0],
        Literal::float(-0.12).expect("finite"),
        "ST_MakePoint takes (x, y) = (longitude, latitude); $1 must be the longitude"
    );
    assert_eq!(sql.params()[1], Literal::float(51.5).expect("finite"));
    assert_eq!(sql.params()[2], Literal::float(1_000.0).expect("finite"));
    assert_eq!(sql.params()[3], Literal::Int(25));
}

/// The radius clause is emitted whether or not there is a filter, which is what
/// makes an unbounded geo search unrepresentable.
///
/// The control is the pair: the same plan with a filter still carries the
/// radius, and the filter is `AND`ed after it rather than replacing it.
#[test]
fn the_radius_clause_survives_both_the_presence_and_the_absence_of_a_filter() {
    let bare = Search::builder(collection("places"), geo_criterion(), rows(&["name"]))
        .build()
        .expect("buildable");
    let filtered = Search::builder(collection("places"), geo_criterion(), rows(&["name"]))
        .filter(eq("tenant_id", 7))
        .build()
        .expect("buildable");

    let bare_sql = render_search(&bare).expect("renderable");
    let filtered_sql = render_search(&filtered).expect("renderable");

    assert!(bare_sql.sql().contains("ST_DWithin("));
    assert!(filtered_sql.sql().contains("ST_DWithin("));
    assert!(
        filtered_sql.sql().contains(") AND "),
        "a filter is ANDed after the radius, never in place of it: {}",
        filtered_sql.sql()
    );
    assert_eq!(
        bare_sql.sql().matches(" WHERE ").count(),
        1,
        "exactly one WHERE, carrying the radius: {}",
        bare_sql.sql()
    );
}

/// A vector search has no radius, so an unfiltered one emits no `WHERE` at all.
/// The control for the arm above: the clause is a property of the criterion,
/// not something the renderer always writes.
#[test]
fn an_unfiltered_vector_search_emits_no_where_clause() {
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::L2),
        rows(&["title"]),
    )
    .build()
    .expect("buildable");
    let sql = render_search(&plan).expect("renderable");
    assert!(
        !sql.sql().contains(" WHERE "),
        "a k-nearest search has nothing to bound: {}",
        sql.sql()
    );
}

// ---------------------------------------------------------------------------
// Canonicalisation: two spellings of one search collapse to one plan
// ---------------------------------------------------------------------------

/// Two callers who expressed one search differently produce **one** plan and
/// therefore one statement and one binding order.
///
/// Three independent permutations are applied at once, so the arm rules on all
/// three rather than on whichever happens to be first: the projection field
/// order, the conjunct order in the filter, and the operand order of a
/// comparison (`7 = tenant_id` against `tenant_id = 7`).
#[test]
fn two_spellings_of_one_search_collapse_to_one_plan() {
    let mirrored = Predicate::Compare {
        lhs: Operand::Lit(Literal::Int(7)),
        op: CompareOp::Eq,
        rhs: Operand::column(column("tenant_id")),
    };

    let forwards = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title", "body"]),
    )
    .filter(Predicate::And(vec![eq("tenant_id", 7), eq("status", 1)]))
    .limit(RowLimit::new(10).expect("limit"))
    .build()
    .expect("buildable");

    let backwards = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["body", "title"]),
    )
    .filter(Predicate::And(vec![eq("status", 1), mirrored]))
    .limit(RowLimit::new(10).expect("limit"))
    .build()
    .expect("buildable");

    assert_eq!(
        forwards, backwards,
        "two spellings of one search must be one plan"
    );

    let a = render_search(&forwards).expect("renderable");
    let b = render_search(&backwards).expect("renderable");
    assert_eq!(a.sql(), b.sql(), "one plan, one prepared statement");
    assert_eq!(
        a.params(),
        b.params(),
        "one plan, one binding order - a divergence here sends different \
         arguments for the same query"
    );
}

/// THE MUTATION CONTROL for the arm above.
///
/// "Two permutations agree" is also true of a renderer that was never given two
/// different inputs. These are the same three permutations with
/// canonicalisation **removed** - the state the tree would be in if the sorts
/// were deleted - and they must come out different, or the arm above proves
/// nothing.
///
/// It is asserted on the raw `Predicate` and the raw field list rather than
/// through the builder, because the builder is what applies the sorts.
#[test]
fn the_search_permutation_fixtures_are_actually_different_inputs() {
    let forwards = Predicate::And(vec![eq("tenant_id", 7), eq("status", 1)]);
    let backwards = Predicate::And(vec![
        eq("status", 1),
        Predicate::Compare {
            lhs: Operand::Lit(Literal::Int(7)),
            op: CompareOp::Eq,
            rhs: Operand::column(column("tenant_id")),
        },
    ]);
    assert_ne!(
        forwards, backwards,
        "the two filter spellings are identical before canonicalisation, so the \
         canonicalisation arm is passing vacuously"
    );
    assert_eq!(
        forwards.canonical(),
        backwards.canonical(),
        "and they must converge after it, or the arm above is asserting a \
         coincidence"
    );
}

/// `k` and `near.limit` are the same concept and collapse to one field with one
/// ceiling.
///
/// Today they are two: `k` is capped at 500 by the `PostgreSQL` builder and not
/// at all by the `SQLite` one, and `near.limit` defaults to 100 where `k`
/// defaults to 10. Here there is one type, one cap, and no absent value.
#[test]
fn the_search_bound_is_one_type_with_one_ceiling_and_no_absent_value() {
    assert_eq!(
        RowLimit::default().get(),
        MAX_ROW_LIMIT,
        "an omitted bound is the cap, not absence"
    );
    assert!(matches!(
        RowLimit::new(MAX_ROW_LIMIT + 1),
        Err(PlanError::LimitOutOfRange { .. })
    ));
    assert!(matches!(
        RowLimit::new(0),
        Err(PlanError::LimitOutOfRange { .. })
    ));

    // The `k` the SQLite arm accepts today and Postgres refuses. One bound now
    // refuses it on both, before a backend is chosen.
    assert!(
        RowLimit::new(750).is_err(),
        "750 is served by the SQLite arm and refused by the Postgres one today; \
         one bound must refuse it before either backend sees the plan"
    );

    // And the bound reaches the statement as a PARAMETER, so one prepared
    // statement serves every k.
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .limit(RowLimit::new(10).expect("limit"))
    .build()
    .expect("buildable");
    let sql = render_search(&plan).expect("renderable");
    assert!(
        sql.sql().ends_with("LIMIT $2"),
        "k is bound, not interpolated - the SQLite arm formats it into the text \
         today: {}",
        sql.sql()
    );
}

/// Every parameter is referenced, and every reference resolves.
///
/// This is SC-3's third invariant in the form the search family forced. A bare
/// occurrence count cannot express it here: the distance expression writes `$1`
/// twice, so `placeholder_count()` exceeds `params().len()` by design and a
/// count-based arm would either fail or have to be weakened to nothing.
#[test]
fn every_parameter_is_referenced_and_every_reference_resolves() {
    let cases: Vec<DbPlan> = vec![
        DbPlan::Search(
            Search::builder(
                collection("docs"),
                vector_criterion(VectorMetric::Cosine),
                rows(&["title"]),
            )
            .filter(eq("tenant_id", 7))
            .build()
            .expect("buildable"),
        ),
        DbPlan::Search(
            Search::builder(collection("places"), geo_criterion(), rows(&["name"]))
                .filter(Predicate::And(vec![eq("tenant_id", 7), eq("status", 1)]))
                .build()
                .expect("buildable"),
        ),
    ];

    let mut ruled_on = 0_usize;
    for plan in &cases {
        let sql = render(plan).expect("renderable");
        let slots = sql.placeholder_slots();
        let expected: Vec<usize> = (1..=sql.params().len()).collect();
        assert_eq!(
            slots,
            expected,
            "the statement's slots must be exactly 1..={}: {}",
            sql.params().len(),
            sql.sql()
        );
        assert!(
            sql.placeholder_count() > slots.len(),
            "a search re-uses at least one slot; if it does not, the saving \
             `Writer::bind` exists for has been lost: {}",
            sql.sql()
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 2, "both criterion kinds must be ruled on");
    println!("ruled on {ruled_on} search plans");
}

// ---------------------------------------------------------------------------
// The depth bound
// ---------------------------------------------------------------------------

/// A predicate of exactly `depth`, built from the **public variants** rather
/// than the smart constructors - which is the hostile shape the builder's guard
/// exists for, since a caller can assemble one by hand.
///
/// The connectives **alternate**, and that is not decoration. Two simpler
/// spellings do not survive canonicalisation, and both were tried here first:
///
/// * a chain of `Not` collapses under double-negation elimination, so
///   `nest(16)` reaches the renderer as a single `NOT (..)`;
/// * a chain of one connective is **flattened** by `canonical_connective`, so
///   `And(And(And(x)))` reaches it as one `AND` of three.
///
/// Both still exercise the depth *guard*, which runs before `canonical()` - but
/// neither can be used to check that a deep tree survives to the statement,
/// because after canonicalisation it is not a deep tree. Alternating `And` and
/// `Or` is the shape flattening cannot merge.
fn nest(depth: usize) -> Predicate {
    let mut node = eq("leaf", 0);
    for level in 1..depth {
        // A distinct value per level, so deduplication has nothing to remove.
        let sibling = eq("guard", i64::try_from(level).expect("small"));
        node = if level % 2 == 1 {
            Predicate::And(vec![sibling, node])
        } else {
            Predicate::Or(vec![sibling, node])
        };
    }
    node
}

/// The control for [`nest`]: it must actually build the depth it claims, and
/// canonicalisation must not shrink it. Without this, every depth arm below
/// would be measuring a tree that had already collapsed.
#[test]
fn the_depth_fixture_builds_the_depth_it_claims_and_survives_canonicalisation() {
    for depth in [1_usize, 2, 8, MAX_PREDICATE_DEPTH, MAX_PREDICATE_DEPTH + 1] {
        let tree = nest(depth);
        assert_eq!(tree.depth(), depth, "nest({depth}) built the wrong depth");
        assert_eq!(
            tree.canonical().depth(),
            depth,
            "canonicalisation collapsed nest({depth}); a Not chain and a \
             single-connective chain both do, which is why this fixture alternates"
        );
    }
}

/// The bound is closed at the search builder, ahead of everything recursive.
///
/// `Predicate`'s variants are public, so a caller can assemble an over-deep
/// tree by hand and hand it to a builder. `SearchBuilder::build` is one of the
/// boundaries such a predicate must cross to become executable, and it checks
/// before `canonical()`, `mentions_aggregate()` and the renderer - all three of
/// which recurse, and any of which would abort the worker rather than fail a
/// request.
#[test]
fn a_search_filter_past_the_depth_bound_is_refused_before_anything_recurses() {
    let at_bound = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .filter(nest(MAX_PREDICATE_DEPTH))
    .build();
    assert!(
        at_bound.is_ok(),
        "the bound is inclusive; a filter the current system accepts must stay \
         expressible"
    );

    let over = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .filter(nest(MAX_PREDICATE_DEPTH + 1))
    .build();
    match over {
        Err(SearchError::FilterTooDeep { depth }) => {
            assert_eq!(depth, MAX_PREDICATE_DEPTH + 1);
        }
        other => panic!("expected FilterTooDeep, got {other:?}"),
    }
}

/// The search family adds no recursive shape of its own, so the filter really
/// is the whole exposure.
///
/// Asserted rather than claimed: a criterion cannot contain a criterion (the
/// enum has no self-referential field), a query vector is a flat list, and the
/// tiebreak keys are `FieldPath`s already bounded by `MAX_PATH_SEGMENTS`. The
/// check available to a test is that a search whose filter is at the bound
/// renders without the renderer being handed anything deeper.
#[test]
fn a_search_at_the_depth_bound_renders() {
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .filter(nest(MAX_PREDICATE_DEPTH))
    .build()
    .expect("at the bound");
    let sql = render_search(&plan).expect("renderable");
    assert_eq!(
        sql.sql().matches(r#""guard" = "#).count(),
        MAX_PREDICATE_DEPTH - 1,
        "the whole tree must reach the statement: {}",
        sql.sql()
    );
    // The parameter invariant must still hold over a tree at the bound: one
    // slot per guard, one for the leaf, one for the vector, one for the limit.
    assert_eq!(
        sql.placeholder_slots(),
        (1..=sql.params().len()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Where the backends legitimately differ
// ---------------------------------------------------------------------------

/// THE DIVERGENCE ARM.
///
/// `pgvector` serves inner product (`vector_ip_ops`); `vec0` serves cosine and
/// L2 only. The IR represents the metric either way, and the difference is
/// asserted **as a difference**: `PostgreSQL` renders it, and the `SQLite`
/// backend refuses it with a typed error.
///
/// The `SQLite` half of the pair lives in
/// `crates/zeroship-data-orm/src/tests/postgres/search_ir.rs`, because
/// `reject_inner_product` is that crate's function and this one has no
/// dependencies. What is asserted **here** is the half that belongs to the
/// grammar: that all three metrics are representable and that `PostgreSQL`
/// renders each to its own operator - so the `SQLite` refusal is a backend
/// declining a plan it was genuinely given, not a plan that could not be built.
#[test]
fn all_three_metrics_are_representable_and_postgres_renders_each_distinctly() {
    let expected = [
        (VectorMetric::Cosine, " <=> "),
        (VectorMetric::L2, " <-> "),
        (VectorMetric::InnerProduct, " <#> "),
    ];
    let mut ruled_on = 0_usize;
    let mut seen: Vec<String> = Vec::new();
    for (metric, operator) in expected {
        let plan = Search::builder(
            collection("docs"),
            vector_criterion(metric),
            rows(&["title"]),
        )
        .build()
        .expect(
            "every metric must be BUILDABLE; a metric one backend cannot serve \
                 is refused by that backend, not made unrepresentable",
        );
        let sql = render_search(&plan).expect("postgres serves all three metrics");
        assert!(
            sql.sql().contains(operator),
            "{} must render {operator}: {}",
            metric.name(),
            sql.sql()
        );
        seen.push(sql.sql().to_string());
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 3);

    // The three must be three DIFFERENT statements. Without this, a lowering
    // that ignored the metric entirely would pass every `contains` above if the
    // operators happened to be substrings of one another.
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen.len(),
        3,
        "the metric must change the statement; three metrics produced fewer than \
         three distinct plans"
    );
    println!("ruled on {ruled_on} metrics");
}

/// The second divergence, and the one a reviewer is most likely to miss: the
/// two backends do not sort ties alike, and the IR lets a caller pin the order.
///
/// A tiebreak carries its own `NullOrder`, because the engines' defaults differ
/// (`PostgreSQL` sorts nulls last on ASC, `SQLite` sorts them first). The clause
/// is therefore always emitted, so the plan says what it means rather than
/// inheriting whichever engine ran it.
#[test]
fn a_tiebreak_pins_the_order_of_equal_distances_and_spells_its_null_placement() {
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .tiebreak(vec![OrderKey {
        path: zeroship_data_orm::sql::FieldPath::column(column("created_at")),
        direction: Direction::Descending,
        nulls: NullOrder::Last,
    }])
    .build()
    .expect("buildable");

    let sql = render_search(&plan).expect("renderable");
    assert!(
        sql.sql()
            .contains(r#"ORDER BY "embedding" <=> $1::vector ASC, "created_at" DESC NULLS LAST"#),
        "the distance key comes first and the tiebreak spells its null placement: {}",
        sql.sql()
    );
}

/// The ranking key is **unspellable**, which is what makes the plan canonical.
///
/// If a caller could name `_distance` as a sort key, `search(v, k)` and
/// `search(v, k).sortBy(_distance)` would be two plans for one query. Two
/// fences hold, and both are asserted, because either alone would look
/// sufficient:
///
/// * `_distance` is refused as a **column**, so the ordinary route to a
///   `FieldPath` is closed;
/// * a tiebreak naming it is refused by the builder, which closes the route via
///   an alias-parsed identifier smuggled into a column position.
#[test]
fn the_ranking_key_cannot_be_named_by_a_caller() {
    for kind in [
        SearchScalarKind::VectorDistance,
        SearchScalarKind::GeoDistanceMetres,
    ] {
        assert!(
            Ident::parse_as(kind.alias_str(), IdentRole::Column).is_err(),
            "'{}' must be refused as a column so a creator cannot shadow it",
            kind.alias_str()
        );
        assert!(
            Ident::parse_as(kind.alias_str(), IdentRole::Alias).is_ok(),
            "'{}' must be legal as an alias, or the platform cannot project it",
            kind.alias_str()
        );
    }

    // The second fence. The identifier is built through the Alias role, which
    // is the only role that admits it, and then offered as a sort key.
    let smuggled = Ident::parse_as("_distance", IdentRole::Alias).expect("legal alias");
    let refused = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .tiebreak(vec![OrderKey {
        path: zeroship_data_orm::sql::FieldPath::column(smuggled),
        direction: Direction::Ascending,
        nulls: NullOrder::Last,
    }])
    .build();
    match refused {
        Err(SearchError::TiebreakNamesTheScalar { alias }) => assert_eq!(alias, "_distance"),
        other => panic!("expected TiebreakNamesTheScalar, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A ranking scalar has no meaning outside a search, and it is unconstructible
/// there rather than merely undocumented.
///
/// The first two fences MOVED IN-CRATE on 2026-09-07, to
/// `projection::tests::a_stray_ranking_scalar_is_refused_by_both_projections`.
/// They needed a `ProjectedField` struct literal to build the adversary, and
/// `ProjectedField`'s fields are private now - so from outside this crate the
/// smuggling those two fences refuse is no longer constructible at all, which is
/// a stronger guarantee than a runtime refusal and an untestable one from here.
/// The refusals still matter for in-crate callers and are still asserted, one
/// module down.
///
/// What stays here is the fence that needs no literal, because the scalar
/// reaches the renderer through a legitimate search projection.
#[test]
fn a_ranking_scalar_is_unreachable_outside_a_search() {
    // The renderer refuses one that reached it anyway, with a typed
    //    error rather than a panic. Reached by rendering the search's own
    //    projection - which legitimately carries a scalar - through the READ
    //    lowering, which has no criterion to give it.
    let search = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .build()
    .expect("buildable");
    let select =
        zeroship_data_orm::sql::Select::builder(collection("docs"), search.projection().clone())
            .build()
            .expect("a Select accepts the projection value; the refusal is at render");
    match zeroship_data_orm::sql::render::postgres::render_select(&select) {
        Err(RenderError::Unsupported { node, .. }) => {
            assert_eq!(node, "a search scalar outside a search");
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// A search returns ranked rows, so it cannot aggregate.
#[test]
fn an_aggregate_projection_is_refused_by_a_search() {
    let aggregate = Projection::aggregate(vec![ProjectedField::aggregate(
        zeroship_data_orm::sql::AggregateRef::count_rows(),
        Ident::parse_as("n", IdentRole::Alias).expect("alias"),
    )])
    .expect("aggregate projection");

    assert!(matches!(
        Search::builder(
            collection("docs"),
            vector_criterion(VectorMetric::Cosine),
            aggregate
        )
        .build(),
        Err(SearchError::AggregateProjection)
    ));
}

/// An aggregate operand in a search's filter is a `PostgreSQL` error
/// (`aggregate functions are not allowed in WHERE`), refused at construction.
#[test]
fn an_aggregate_operand_in_a_search_filter_is_refused() {
    let filter = Predicate::Compare {
        lhs: Operand::Aggregate(zeroship_data_orm::sql::AggregateRef::count_rows()),
        op: CompareOp::Gt,
        rhs: Operand::Lit(Literal::Int(1)),
    };
    assert!(matches!(
        Search::builder(
            collection("docs"),
            vector_criterion(VectorMetric::Cosine),
            rows(&["title"])
        )
        .filter(filter)
        .build(),
        Err(SearchError::AggregateInFilter)
    ));
}

/// A query vector's three refusals, each for a reason `pgvector` would
/// otherwise raise after the whole buffer had crossed the wire.
#[test]
fn a_query_vector_refuses_the_empty_the_wide_and_the_non_finite() {
    assert!(matches!(
        QueryVector::new(&[]),
        Err(LiteralError::EmptyQueryVector)
    ));
    assert!(matches!(
        QueryVector::new(&vec![0.0_f32; MAX_VECTOR_DIMS + 1]),
        Err(LiteralError::QueryVectorTooWide { .. })
    ));
    assert!(
        QueryVector::new(&vec![0.0_f32; MAX_VECTOR_DIMS]).is_ok(),
        "the bound is inclusive; pgvector creates a vector(16000) column"
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            matches!(
                QueryVector::new(&[1.0, bad]),
                Err(LiteralError::NonFiniteFloat)
            ),
            "a {bad} element must be refused: it makes the distance sort \
             arbitrarily, which is a ranking that silently means nothing"
        );
    }
}

/// A vector's element order is its meaning, so it is the one list this crate
/// does not canonicalise by sorting.
///
/// Worth an arm because every neighbouring list *is* sorted: an implementer
/// following the local convention would break the search silently, returning
/// plausible rows ranked against a vector the caller never sent.
#[test]
fn a_query_vector_is_not_sorted() {
    let descending = vector(&[3.0, 2.0, 1.0]);
    let elements: Vec<f32> = descending.elements().iter().map(|e| e.get()).collect();
    assert_eq!(elements, vec![3.0, 2.0, 1.0]);
    assert_ne!(
        vector(&[3.0, 2.0, 1.0]),
        vector(&[1.0, 2.0, 3.0]),
        "two orderings of one multiset are two different vectors"
    );
}

/// Coordinates and radii are validated, and the transposition hazard is named
/// in the error because two valid coordinates in the wrong order is a different
/// place on Earth and no error anywhere else.
#[test]
fn a_geo_point_and_radius_refuse_what_is_not_on_the_globe() {
    assert!(matches!(
        GeoPoint::new(91.0, 0.0),
        Err(SearchError::LatitudeOutOfRange { .. })
    ));
    assert!(matches!(
        GeoPoint::new(0.0, 181.0),
        Err(SearchError::LongitudeOutOfRange { .. })
    ));
    // A NaN passes every range comparison, so it must be refused one step
    // earlier or it would arrive as a valid point.
    assert!(matches!(
        GeoPoint::new(f64::NAN, 0.0),
        Err(SearchError::Literal(LiteralError::NonFiniteFloat))
    ));
    assert!(
        GeoPoint::new(90.0, 180.0).is_ok(),
        "the poles are on the globe"
    );

    assert!(matches!(
        RadiusMetres::new(0.0),
        Err(SearchError::RadiusOutOfRange { .. })
    ));
    assert!(matches!(
        RadiusMetres::new(-1.0),
        Err(SearchError::RadiusOutOfRange { .. })
    ));
    assert!(matches!(
        RadiusMetres::new(MAX_RADIUS_METRES * 2.0),
        Err(SearchError::RadiusOutOfRange { .. })
    ));
    assert!(RadiusMetres::new(MAX_RADIUS_METRES).is_ok());
}

/// The message a transposed pair produces has to name the transposition, or the
/// hazard is documented only where nobody reading the error will look.
#[test]
fn the_latitude_refusal_names_the_transposition_hazard() {
    let err = GeoPoint::new(-0.12_f64.mul_add(0.0, 120.0), 51.5).expect_err("out of range");
    let message = err.to_string();
    assert!(
        message.contains("ST_MakePoint") && message.contains("longitude, latitude"),
        "the refusal must name why a valid-looking pair is out of range: {message}"
    );
}

/// A repeated tiebreak key is a no-op and is dropped; the order of the keys
/// that survive is NOT sorted, because `ORDER BY a, b` and `ORDER BY b, a` are
/// different queries.
#[test]
fn tiebreak_keys_dedupe_but_do_not_reorder() {
    let key = |name: &str, direction| OrderKey {
        path: zeroship_data_orm::sql::FieldPath::column(column(name)),
        direction,
        nulls: NullOrder::Last,
    };
    let plan = Search::builder(
        collection("docs"),
        vector_criterion(VectorMetric::Cosine),
        rows(&["title"]),
    )
    .tiebreak(vec![
        key("zeta", Direction::Ascending),
        key("alpha", Direction::Descending),
        // A repeat of the first key, with the opposite direction. The first
        // occurrence wins - a second sort on a column already sorted cannot
        // change anything.
        key("zeta", Direction::Descending),
    ])
    .build()
    .expect("buildable");

    assert_eq!(plan.tiebreak().len(), 2);
    let sql = render_search(&plan).expect("renderable");
    let zeta = sql.sql().find(r#""zeta""#).expect("zeta present");
    let alpha = sql.sql().find(r#""alpha""#).expect("alpha present");
    assert!(
        zeta < alpha,
        "the authored order must survive; sorting it would change the query: {}",
        sql.sql()
    );
    assert!(sql.sql().contains(r#""zeta" ASC"#), "{}", sql.sql());
}
