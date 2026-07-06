use std::collections::{BTreeMap, BTreeSet};

use zeroship_migrate::model::expr::{
    AggFunc, BinaryOp, CaseBranch, CastTarget, Expr, ExtractField, PgExtractField, ScalarFn,
    SynthFn, UnaryOp,
};
use zeroship_migrate::model::ir::{IrScalar, IrValue};
use zeroship_migrate::model::validate::{validate_expr, Dialect, TargetScope};
use zeroship_migrate::render::dml::assemble_backfill_clauses;
use zeroship_migrate::SqlDialect;

const EXPECTED_PORTABLE_EXPR_VARIANTS: &[&str] = &[
    "Agg",
    "Between",
    "BinOp",
    "Case",
    "Cast",
    "ColRef",
    "DistinctFrom",
    "Extract",
    "FnCall",
    "FnSynth",
    "InList",
    "Like",
    "Literal",
    "UnaryOp",
];

const EXPECTED_SYNTAX_ONLY_GAPS: &[&str] = &["Like"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProofClaim {
    SemanticEquivalence,
    SyntaxOnlyGap(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprCoverage {
    Portable {
        variant: &'static str,
        proof: &'static str,
        claim: ProofClaim,
    },
    Vendor {
        variant: &'static str,
        reason: &'static str,
    },
}

fn portable(
    variant: &'static str,
    proof: &'static str,
    claim: ProofClaim,
) -> ExprCoverage {
    ExprCoverage::Portable {
        variant,
        proof,
        claim,
    }
}

fn vendor(variant: &'static str, reason: &'static str) -> ExprCoverage {
    ExprCoverage::Vendor { variant, reason }
}

fn classify_expr(expr: &Expr) -> ExprCoverage {
    match expr {
        Expr::ColRef { .. } => portable(
            "ColRef",
            "render::dml::tests::qualified_colref_renders_dotted_per_dialect",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::Literal { .. } => portable(
            "Literal",
            "render::dml::tests::between_renders_identically_on_all_three_dialects",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::BinOp { .. } => portable(
            "BinOp",
            "render::dml::tests::concat_renders_per_dialect_pg_sqlite_mysql",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::UnaryOp { .. } => portable(
            "UnaryOp",
            "render::dml::tests::is_true_is_false_rewritten_for_sqlite",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::Case { .. } => portable(
            "Case",
            "render::dml::tests::portable_predicate_nodes_render_through_case_arms",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::FnCall { r#fn, .. } => match r#fn {
            ScalarFn::Coalesce
            | ScalarFn::Nullif
            | ScalarFn::Lower
            | ScalarFn::Upper
            | ScalarFn::Trim
            | ScalarFn::Length
            | ScalarFn::Abs
            | ScalarFn::Mod
            | ScalarFn::Round
            | ScalarFn::Floor
            | ScalarFn::Ceil
            | ScalarFn::Substr
            | ScalarFn::Replace => portable(
                "FnCall",
                "render::dml::tests::portable_scalar_fns_render_identically_on_all_three",
                ProofClaim::SemanticEquivalence,
            ),
            ScalarFn::CurrentSetting => vendor(
                "FnCall::CurrentSetting",
                "PostgreSQL GUC read has no portable SQLite/MySQL equivalent",
            ),
            ScalarFn::CurrentUser => vendor(
                "FnCall::CurrentUser",
                "PostgreSQL identity scalar has no portable SQLite/MySQL equivalent",
            ),
        },
        Expr::FnSynth { r#fn, .. } => match r#fn {
            SynthFn::ConcatWs | SynthFn::SplitPart | SynthFn::Now | SynthFn::GenRandomUuid => {
                portable(
                    "FnSynth",
                    "ir_splitpart_parity_pg::split_part_apply_matches_sqlite_parity_path and ir_dml_sqlite fnSynth apply tests",
                    ProofClaim::SemanticEquivalence,
                )
            }
        },
        Expr::Cast { .. } => portable(
            "Cast",
            "render::dml::tests::cast_renders_per_dialect_type_names",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::Between { .. } => portable(
            "Between",
            "render::dml::tests::between_renders_identically_on_all_three_dialects",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::Like { .. } => portable(
            "Like",
            "render::dml::tests::like_renders_same_syntax_on_all_three_dialects",
            ProofClaim::SyntaxOnlyGap(
                "LIKE syntax renders on all three dialects, but case-sensitivity semantics remain collation/dialect dependent",
            ),
        ),
        Expr::DistinctFrom { .. } => portable(
            "DistinctFrom",
            "render::dml::tests::distinct_from_diverges_pg_sqlite_vs_mysql",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::Agg { .. } => portable(
            "Agg",
            "render::dml::tests::agg_renders_identically_on_all_three_dialects",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::InList { .. } => portable(
            "InList",
            "mysql_jsdriver_e2e::in_list_not_in_and_empty_list_predicates_apply_equivalently",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::PgRegexMatch { .. } => vendor(
            "PgRegexMatch",
            "PostgreSQL regex operator is a PG-only vendor expression",
        ),
        Expr::PgColumnSize { .. } => vendor(
            "PgColumnSize",
            "pg_column_size is a PG-only storage-layout expression",
        ),
        Expr::Extract { .. } => portable(
            "Extract",
            "extract_equivalence::portable_extract_fields_are_live_equivalent_on_all_three_dialects",
            ProofClaim::SemanticEquivalence,
        ),
        Expr::PgExtract { .. } => vendor(
            "PgExtract",
            "PG-only EXTRACT fields have no portable SQLite/MySQL equivalent",
        ),
        Expr::PgInterval { .. } => vendor(
            "PgInterval",
            "PostgreSQL interval literals have no portable SQLite/MySQL equivalent",
        ),
        Expr::Dialectal { .. } => vendor(
            "Dialectal",
            "Layer-2 per-dialect value escape, not a cross-dialect equivalence node",
        ),
    }
}

fn lit_int(value: i64) -> Expr {
    Expr::lit(IrScalar::Int(value))
}

fn lit_str(value: &str) -> Expr {
    Expr::lit(IrScalar::Str(value.to_string()))
}

fn portable_expr_samples() -> Vec<Expr> {
    vec![
        // covered by render::dml::tests::qualified_colref_renders_dotted_per_dialect
        Expr::col("a"),
        // covered by render::dml literal binding assertions in predicate tests
        lit_str("literal"),
        // covered by render::dml::tests::concat_renders_per_dialect_pg_sqlite_mysql
        Expr::BinOp {
            op: BinaryOp::Concat,
            lhs: Box::new(Expr::col("first")),
            rhs: Box::new(Expr::col("last")),
        },
        // covered by render::dml::tests::is_true_is_false_rewritten_for_sqlite
        Expr::UnaryOp {
            op: UnaryOp::IsTrue,
            operand: Box::new(Expr::col("active")),
        },
        // covered by render::dml CASE render path and this gate's 3-dialect smoke
        Expr::Case {
            branches: vec![CaseBranch {
                when: Expr::UnaryOp {
                    op: UnaryOp::IsNotNull,
                    operand: Box::new(Expr::col("a")),
                },
                then: lit_str("present"),
            }],
            r#else: Some(Box::new(lit_str("missing"))),
        },
        // covered by render::dml::tests::portable_scalar_fns_render_identically_on_all_three
        Expr::FnCall {
            r#fn: ScalarFn::Length,
            args: vec![Expr::col("name")],
        },
        // covered by splitPart/concatWs parity tests plus this gate's render smoke
        Expr::FnSynth {
            r#fn: SynthFn::ConcatWs,
            args: vec![lit_str("-"), Expr::col("first"), Expr::col("last")],
        },
        // covered by render::dml cast tests plus this gate's render smoke
        Expr::Cast {
            operand: Box::new(Expr::col("a")),
            target: CastTarget::Text,
        },
        // covered by render::dml::tests::between_renders_identically_on_all_three_dialects
        Expr::Between {
            operand: Box::new(Expr::col("age")),
            low: Box::new(lit_int(18)),
            high: Box::new(lit_int(65)),
        },
        // covered by render::dml::tests::like_renders_same_syntax_on_all_three_dialects
        Expr::Like {
            operand: Box::new(Expr::col("name")),
            pattern: Box::new(lit_str("A%")),
        },
        // covered by render::dml::tests::distinct_from_diverges_pg_sqlite_vs_mysql
        Expr::DistinctFrom {
            left: Box::new(Expr::col("a")),
            right: Box::new(Expr::col("b")),
        },
        // covered by render::dml::tests::agg_renders_identically_on_all_three_dialects
        Expr::Agg {
            func: AggFunc::Count,
            arg: Some(Box::new(Expr::col("x"))),
            distinct: true,
        },
        // covered by render and MySQL live inList/notIn/empty-list proofs
        Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec!["active".to_string(), "trial".to_string()],
            negated: false,
        },
        // covered by extract_equivalence::portable_extract_fields_are_live_equivalent_on_all_three_dialects
        Expr::Extract {
            field: ExtractField::Year,
            from: Box::new(Expr::col("ts")),
        },
    ]
}

fn dialect_pairs() -> [(Dialect, SqlDialect); 3] {
    [
        (Dialect::Postgres, SqlDialect::Postgres),
        (Dialect::Sqlite, SqlDialect::Sqlite),
        (Dialect::Mysql, SqlDialect::Mysql),
    ]
}

fn scope_columns() -> Vec<String> {
    [
        "a", "b", "first", "last", "active", "name", "age", "x", "status", "ts",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn assert_validates_and_renders_on_all_three(expr: &Expr, variant: &str) {
    let columns = scope_columns();
    let scope = TargetScope::new("t", &columns);
    for (validator_dialect, sql_dialect) in dialect_pairs() {
        validate_expr(expr, validator_dialect, &scope, 0, None).unwrap_or_else(|err| {
            panic!("{variant} must validate on {validator_dialect:?}: {err:?}")
        });

        let mut set = BTreeMap::new();
        set.insert("out".to_string(), IrValue::Expr(expr.clone()));
        let rendered = assemble_backfill_clauses(sql_dialect, "t", &set, Some(expr))
            .unwrap_or_else(|err| panic!("{variant} must render on {sql_dialect:?}: {err:?}"));
        assert!(
            !rendered.set_clause.trim().is_empty(),
            "{variant} set clause rendered empty on {sql_dialect:?}"
        );
        assert!(
            rendered
                .filter
                .as_deref()
                .is_some_and(|filter| !filter.trim().is_empty()),
            "{variant} filter rendered empty on {sql_dialect:?}"
        );
    }
}

#[test]
fn every_portable_expr_variant_has_registered_three_dialect_coverage() {
    let mut seen = BTreeSet::new();
    let mut syntax_only = Vec::new();
    let mut semantic_count = 0usize;

    for expr in portable_expr_samples() {
        match classify_expr(&expr) {
            ExprCoverage::Portable {
                variant,
                proof,
                claim,
            } => {
                assert!(!proof.trim().is_empty(), "{variant} must name its proof");
                assert!(
                    seen.insert(variant),
                    "{variant} appears more than once in the portable gate"
                );
                assert_validates_and_renders_on_all_three(&expr, variant);
                match claim {
                    ProofClaim::SemanticEquivalence => semantic_count += 1,
                    ProofClaim::SyntaxOnlyGap(reason) => {
                        assert!(!reason.trim().is_empty(), "{variant} gap must be explicit");
                        syntax_only.push(variant);
                    }
                }
            }
            ExprCoverage::Vendor { variant, reason } => {
                panic!("portable sample classified as vendor: {variant} ({reason})")
            }
        }
    }

    let expected: BTreeSet<&'static str> = EXPECTED_PORTABLE_EXPR_VARIANTS.iter().copied().collect();
    assert_eq!(
        seen, expected,
        "the portable Expr variant gate must be updated with the closed Expr enum"
    );
    assert_eq!(
        syntax_only, EXPECTED_SYNTAX_ONLY_GAPS,
        "syntax-only gaps must stay explicit; do not claim semantic equivalence silently"
    );
    assert_eq!(
        semantic_count,
        EXPECTED_PORTABLE_EXPR_VARIANTS.len() - EXPECTED_SYNTAX_ONLY_GAPS.len(),
        "every non-gap portable Expr variant must name a semantic equivalence proof"
    );
}

#[test]
fn vendor_expr_variants_are_classified_out_of_the_portable_gate() {
    let vendor_samples = vec![
        Expr::FnCall {
            r#fn: ScalarFn::CurrentSetting,
            args: vec![lit_str("zeroship.tenant_app"), Expr::lit(IrScalar::Bool(true))],
        },
        Expr::FnCall {
            r#fn: ScalarFn::CurrentUser,
            args: vec![],
        },
        Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^a".to_string(),
        },
        Expr::PgColumnSize {
            expr: Box::new(Expr::col("name")),
        },
        Expr::PgExtract {
            field: PgExtractField::Epoch,
            from: Box::new(Expr::col("ts")),
        },
        Expr::PgInterval {
            duration: zeroship_migrate::Duration {
                years: None,
                months: None,
                days: None,
                hours: None,
                minutes: Some(1),
                seconds: None,
            },
        },
        Expr::Dialectal {
            default: None,
            pg: Some(Box::new(lit_str("pg"))),
            sqlite: None,
            mysql: None,
        },
    ];

    for expr in vendor_samples {
        match classify_expr(&expr) {
            ExprCoverage::Vendor { variant, reason } => {
                assert!(!variant.trim().is_empty());
                assert!(!reason.trim().is_empty());
            }
            ExprCoverage::Portable { variant, .. } => {
                panic!("vendor/escape Expr classified as portable: {variant}")
            }
        }
    }
}
