//! Property-based tests for the invariants this crate's types and docs promise.
//!
//! Every property here is a claim the crate already makes in its own docs or
//! module comments; the module cites the path and function it exercises rather
//! than restating the implementation. The properties live in the crate's unit
//! test module because the recursive wire AST is easiest to generate beside the
//! types it is built from, and because `Checksum::of_ir_strings_with_reverse` is
//! crate-private.
//!
//! The shape under test:
//!
//! - [`IrScalar`](crate::ir::IrScalar), [`IrValue`], and [`Expr`] serialize to a
//!   wire form and parse back equal, and re-serializing the parsed value yields
//!   the same bytes (the canonical-form stability the single-checksum invariant
//!   rests on; `src/ir.rs` module docs).
//! - The numeric carriers admit EXACTLY their documented domains
//!   ([`IrScalar`](crate::ir::IrScalar), [`SafeU64`], [`SafeI64`]).
//! - [`Checksum::of_ir`] is deterministic and sensitive to every field the docs
//!   say it covers, and a declared reverse is distinct from no reverse at all
//!   (`src/migration.rs`).
//! - [`MigrationId`] / the `src/id.rs` base36 codec round-trip and order the way
//!   their docs say.
//! - The structural validator never panics on the closed AST and refuses the
//!   `ColRef` / `splitPart` shapes its docs say it refuses (`src/validate.rs`).

use std::collections::BTreeMap;

use proptest::collection::{btree_map, vec as pvec};
use proptest::option;
use proptest::prelude::*;
use proptest::sample::select;

use crate::dialect::DialectId;
use crate::expr::{
    AggFunc, BinaryOp, CaseBranch, CastTarget, Duration, Expr, ExtractField, ScalarFn, SynthFn,
    UnaryOp,
};
use crate::id as typed_id;
use crate::ir::{
    BackfillSetValue, CanonicalOpList, IrFlagsOverride, IrScalar, IrValue, MigrationIr, Op,
    PerRowGenerator, SafeI64, SafeU64,
};
use crate::migration::{
    migration_id_for_version, Checksum, ChecksumInput, MigrationFlags, MigrationId, OnlinePhase,
    ReverseDomain, VERSION_CEILING,
};
use crate::precondition::{CmpOp, OnUnmet, Precondition, PreconditionCheck};
use crate::validate::{
    validate_expr, ExprDialectFeature, ExprDialectRejection, ExprDialectValidator,
    ExprDialectValidatorSet, TargetScope,
};

/// The boundary of exact integer representation in an IEEE-754 double. Mirrors
/// the crate-private `MAX_EXACT_INT` in `src/ir.rs`; the docs on `IrScalar` /
/// `SafeU64` / `SafeI64` define the admitted domain as magnitude `< 2^53`.
const MAX_EXACT_INT: i64 = 1 << 53;

// -- Strategies --------------------------------------------------------------

fn safe_u64_strategy() -> impl Strategy<Value = SafeU64> {
    (0u64..MAX_EXACT_INT as u64).prop_map(|n| SafeU64::new(n).expect("in-range"))
}

/// The exact JS safe-integer boundary plus arbitrary values, so the numeric
/// domain property exercises the edge instead of waiting for a random draw to
/// land on it.
fn boundary_i64_strategy() -> impl Strategy<Value = i64> {
    prop_oneof![
        Just(MAX_EXACT_INT),
        Just(MAX_EXACT_INT - 1),
        Just(-MAX_EXACT_INT),
        Just(-(MAX_EXACT_INT - 1)),
        Just(0),
        any::<i64>(),
    ]
}

fn boundary_u64_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(MAX_EXACT_INT as u64),
        Just(MAX_EXACT_INT as u64 - 1),
        Just(0),
        any::<u64>(),
    ]
}

/// Plausible TypeID-prefix shapes plus the exact length boundary, so the
/// grammar property reaches the accepted side of the predicate.
fn type_id_prefix_candidate_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z_]{0,8}".prop_map(String::from),
        "[A-Za-z0-9_]{0,8}".prop_map(String::from),
        any::<String>(),
        Just(String::new()),
        Just("abcdef".to_string()),
        Just("abcdefg".to_string()),
        Just("_abc".to_string()),
        Just("abc_".to_string()),
    ]
}

fn dialect_id_strategy() -> impl Strategy<Value = DialectId> {
    select(vec![
        DialectId::new("postgres"),
        DialectId::new("sqlite"),
        DialectId::new("mysql"),
        DialectId::new("duckdb"),
    ])
}

fn online_phase_strategy() -> impl Strategy<Value = OnlinePhase> {
    select(vec![OnlinePhase::Expand, OnlinePhase::Contract])
}

/// A [`IrScalar`] over every carrier: null, bool, JS-safe int, the tagged
/// `int64` / `decimal` / `bytes` objects, and arbitrary UTF-8 strings.
fn ir_scalar_strategy() -> impl Strategy<Value = IrScalar> {
    prop_oneof![
        Just(IrScalar::Null),
        any::<bool>().prop_map(IrScalar::Bool),
        (-(MAX_EXACT_INT - 1)..=(MAX_EXACT_INT - 1)).prop_map(IrScalar::Int),
        any::<i64>().prop_map(IrScalar::Int64),
        (any::<i64>(), option::of(1u32..1_000_000u32)).prop_map(|(whole, frac)| {
            IrScalar::Decimal(
                frac.map_or_else(|| whole.to_string(), |fraction| format!("{whole}.{fraction}")),
            )
        }),
        any::<String>().prop_map(IrScalar::Str),
        pvec(any::<u8>(), 0..32).prop_map(IrScalar::Bytes),
    ]
}

fn duration_strategy() -> impl Strategy<Value = Duration> {
    (
        option::of(any::<i64>()),
        option::of(any::<i64>()),
        option::of(any::<i64>()),
        option::of(any::<i64>()),
        option::of(any::<i64>()),
        option::of(any::<i64>()),
    )
        .prop_map(
            |(years, months, days, hours, minutes, seconds)| Duration {
                years,
                months,
                days,
                hours,
                minutes,
                seconds,
            },
        )
}

fn string_list() -> impl Strategy<Value = Vec<String>> {
    pvec(any::<String>(), 0..3)
}

/// A recursive [`Expr`] over the whole closed node set. The recursion depth is
/// bounded so the generated trees stay small enough to serialize cheaply.
fn expr_strategy() -> BoxedStrategy<Expr> {
    let leaf = prop_oneof![
        (any::<String>(), option::of(any::<String>()))
            .prop_map(|(name, table)| Expr::ColRef { name, table }),
        ir_scalar_strategy().prop_map(|value| Expr::Literal { value }),
        Just(Expr::UuidV4),
        Just(Expr::UuidV7),
        duration_strategy().prop_map(|duration| Expr::Interval { duration }),
    ];
    leaf.prop_recursive(4, 48, 8, |inner| {
        prop_oneof![
            (select(binary_ops()), inner.clone(), inner.clone()).prop_map(|(op, lhs, rhs)| {
                Expr::BinOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }),
            (select(unary_ops()), inner.clone()).prop_map(|(op, operand)| Expr::UnaryOp {
                op,
                operand: Box::new(operand),
            }),
            (
                pvec(
                    (inner.clone(), inner.clone())
                        .prop_map(|(when, then)| CaseBranch { when, then }),
                    0..3,
                ),
                option::of(inner.clone()),
            )
                .prop_map(|(branches, r#else)| Expr::Case {
                    branches,
                    r#else: r#else.map(Box::new),
                }),
            (select(scalar_fns()), pvec(inner.clone(), 0..3))
                .prop_map(|(r#fn, args)| Expr::FnCall { r#fn, args }),
            (select(synth_fns()), pvec(inner.clone(), 0..3))
                .prop_map(|(r#fn, args)| Expr::FnSynth { r#fn, args }),
            (inner.clone(), select(cast_targets())).prop_map(|(operand, target)| Expr::Cast {
                operand: Box::new(operand),
                target,
            }),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(
                |(operand, low, high)| Expr::Between {
                    operand: Box::new(operand),
                    low: Box::new(low),
                    high: Box::new(high),
                }
            ),
            (inner.clone(), inner.clone()).prop_map(|(operand, pattern)| Expr::Like {
                operand: Box::new(operand),
                pattern: Box::new(pattern),
            }),
            (inner.clone(), inner.clone()).prop_map(|(left, right)| Expr::DistinctFrom {
                left: Box::new(left),
                right: Box::new(right),
            }),
            (
                select(agg_funcs()),
                option::of(inner.clone()),
                option::of(inner.clone()),
                any::<bool>(),
            )
                .prop_map(|(func, arg, delimiter, distinct)| Expr::Agg {
                    func,
                    arg: arg.map(Box::new),
                    delimiter: delimiter.map(Box::new),
                    distinct,
                }),
            (inner.clone(), pvec(ir_scalar_strategy(), 0..3), any::<bool>()).prop_map(
                |(expr, elems, negated)| Expr::InList {
                    expr: Box::new(expr),
                    elems,
                    negated,
                }
            ),
            (inner.clone(), any::<String>()).prop_map(|(expr, pattern)| Expr::RegexMatch {
                expr: Box::new(expr),
                pattern,
            }),
            inner.clone().prop_map(|expr| Expr::StorageSize {
                expr: Box::new(expr),
            }),
            (select(extract_fields()), inner.clone()).prop_map(|(field, from)| Expr::Extract {
                field,
                from: Box::new(from),
            }),
            (btree_map(dialect_id_strategy(), inner, 0..3)).prop_map(|legs| {
                Expr::Dialectal {
                    legs: legs
                        .into_iter()
                        .map(|(id, expr)| (id, Box::new(expr)))
                        .collect(),
                }
            }),
        ]
    })
    .boxed()
}

fn binary_ops() -> Vec<BinaryOp> {
    vec![
        BinaryOp::Eq,
        BinaryOp::Ne,
        BinaryOp::Lt,
        BinaryOp::Le,
        BinaryOp::Gt,
        BinaryOp::Ge,
        BinaryOp::And,
        BinaryOp::Or,
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Mul,
        BinaryOp::Div,
        BinaryOp::Concat,
    ]
}

fn unary_ops() -> Vec<UnaryOp> {
    vec![
        UnaryOp::Not,
        UnaryOp::IsNull,
        UnaryOp::IsNotNull,
        UnaryOp::IsTrue,
        UnaryOp::IsFalse,
    ]
}

fn scalar_fns() -> Vec<ScalarFn> {
    vec![
        ScalarFn::Coalesce,
        ScalarFn::Nullif,
        ScalarFn::Lower,
        ScalarFn::Upper,
        ScalarFn::Trim,
        ScalarFn::Length,
        ScalarFn::Abs,
        ScalarFn::Mod,
        ScalarFn::Round,
        ScalarFn::Floor,
        ScalarFn::Ceil,
        ScalarFn::Substr,
        ScalarFn::Replace,
        ScalarFn::CurrentSetting,
        ScalarFn::CurrentUser,
    ]
}

fn synth_fns() -> Vec<SynthFn> {
    vec![SynthFn::ConcatWs, SynthFn::SplitPart, SynthFn::Now]
}

fn cast_targets() -> Vec<CastTarget> {
    vec![
        CastTarget::Text,
        CastTarget::Int,
        CastTarget::Real,
        CastTarget::Boolean,
        CastTarget::Bytes,
        CastTarget::Uuid,
    ]
}

fn extract_fields() -> Vec<ExtractField> {
    vec![
        ExtractField::Year,
        ExtractField::Month,
        ExtractField::Day,
        ExtractField::Hour,
        ExtractField::Minute,
        ExtractField::Dow,
        ExtractField::Second,
        ExtractField::Doy,
        ExtractField::Epoch,
        ExtractField::Quarter,
        ExtractField::Week,
        ExtractField::Isodow,
        ExtractField::Isoyear,
        ExtractField::Century,
        ExtractField::Decade,
        ExtractField::Millennium,
        ExtractField::Microseconds,
        ExtractField::Milliseconds,
        ExtractField::Timezone,
        ExtractField::TimezoneHour,
        ExtractField::TimezoneMinute,
    ]
}

fn agg_funcs() -> Vec<AggFunc> {
    vec![
        AggFunc::Count,
        AggFunc::Sum,
        AggFunc::Avg,
        AggFunc::Min,
        AggFunc::Max,
        AggFunc::StringAgg,
        AggFunc::ArrayAgg,
        AggFunc::BoolAnd,
        AggFunc::BoolOr,
    ]
}

/// A DML value over both untagged arms (a scalar and a closed expression).
fn dml_value_strategy() -> impl Strategy<Value = IrValue> {
    prop_oneof![
        ir_scalar_strategy().prop_map(IrValue::Scalar),
        expr_strategy().prop_map(IrValue::Expr),
    ]
}

fn per_row_generator_strategy() -> impl Strategy<Value = PerRowGenerator> {
    prop_oneof![
        Just(PerRowGenerator::UuidV4),
        Just(PerRowGenerator::UuidV7),
        any::<String>().prop_map(|prefix| PerRowGenerator::TypeId { prefix }),
    ]
}

fn backfill_set_value_strategy() -> impl Strategy<Value = BackfillSetValue> {
    prop_oneof![
        dml_value_strategy().prop_map(BackfillSetValue::Value),
        per_row_generator_strategy()
            .prop_map(|per_row| BackfillSetValue::PerRow { per_row }),
    ]
}

/// An [`Op`] drawn from the attribute-free DDL/DML variants, so the strategy
/// needs no vendor attribute carrier. Enough to exercise the canonical byte
/// image (op count + JCS body) and the checksum fold.
fn checksum_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (
            any::<String>(),
            string_list(),
            pvec(pvec(dml_value_strategy(), 0..3), 0..3),
            option::of(any::<String>()),
        )
            .prop_map(|(table, columns, rows, schema)| Op::Insert {
                table,
                columns,
                rows,
                on_conflict: None,
                schema,
            }),
        (
            any::<String>(),
            btree_map(any::<String>(), dml_value_strategy(), 0..3),
            option::of(expr_strategy()),
            option::of(any::<String>()),
        )
            .prop_map(|(table, set, r#where, schema)| Op::Update {
                table,
                set,
                r#where,
                schema,
            }),
        (
            any::<String>(),
            expr_strategy(),
            option::of(safe_u64_strategy()),
            option::of(any::<String>()),
        )
            .prop_map(|(table, r#where, limit, schema)| Op::Delete {
                table,
                r#where,
                limit,
                schema,
            }),
        (any::<String>(), string_list(), option::of(any::<String>()))
            .prop_map(|(name, values, schema)| Op::CreateEnum {
                name,
                schema,
                values,
            }),
    ]
}

fn op_list_strategy() -> impl Strategy<Value = Vec<Op>> {
    pvec(checksum_op_strategy(), 0..4)
}

fn migration_flags_strategy() -> impl Strategy<Value = MigrationFlags> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        option::of(safe_u64_strategy()),
        option::of(safe_u64_strategy()),
        option::of(online_phase_strategy()),
    )
        .prop_map(
            |(
                transactional,
                destructive,
                online,
                requires_approval,
                repeatable,
                timeout_ms,
                lock_timeout_ms,
                phase,
            )| MigrationFlags {
                transactional,
                destructive,
                online,
                requires_approval,
                timeout_ms: timeout_ms.map(SafeU64::get),
                lock_timeout_ms: lock_timeout_ms.map(SafeU64::get),
                phase,
                repeatable,
            },
        )
}

fn migration_id_strategy() -> impl Strategy<Value = MigrationId> {
    (0u64..VERSION_CEILING).prop_map(migration_id_for_version)
}

fn precondition_strategy() -> impl Strategy<Value = Precondition> {
    prop_oneof![
        any::<String>().prop_map(|table| Precondition::TableExists { table }),
        any::<String>().prop_map(|table| Precondition::TableNotExists { table }),
        (any::<String>(), any::<String>())
            .prop_map(|(table, column)| Precondition::ColumnExists { table, column }),
        (any::<String>(), any::<String>())
            .prop_map(|(table, column)| Precondition::ColumnNotExists { table, column }),
        (any::<String>(), any::<String>()).prop_map(|(table, column)| {
            Precondition::ColumnHasNoBlockingDependents { table, column }
        }),
        (any::<String>(), any::<String>()).prop_map(|(table, column)| {
            Precondition::ColumnTypeChangeHasNoBlockers { table, column }
        }),
        (
            any::<String>(),
            select(vec![
                CmpOp::Eq,
                CmpOp::Ne,
                CmpOp::Lt,
                CmpOp::Le,
                CmpOp::Gt,
                CmpOp::Ge,
            ]),
            any::<i64>(),
        )
            .prop_map(|(table, op, value)| Precondition::RowCount { table, op, value }),
        any::<String>().prop_map(|sql| Precondition::SqlBoolean { sql }),
    ]
}

fn precondition_check_strategy() -> impl Strategy<Value = PreconditionCheck> {
    (
        precondition_strategy(),
        select(vec![OnUnmet::Halt, OnUnmet::Skip]),
    )
        .prop_map(|(check, on_unmet)| PreconditionCheck { check, on_unmet })
}

fn ir_flags_override_strategy() -> impl Strategy<Value = IrFlagsOverride> {
    (
        option::of(any::<bool>()),
        option::of(any::<bool>()),
        option::of(any::<bool>()),
        option::of(any::<bool>()),
        option::of(any::<bool>()),
        option::of(safe_u64_strategy()),
        option::of(safe_u64_strategy()),
        option::of(online_phase_strategy()),
    )
        .prop_map(
            |(
                transactional,
                destructive,
                online,
                requires_approval,
                repeatable,
                timeout_ms,
                lock_timeout_ms,
                phase,
            )| IrFlagsOverride {
                transactional,
                destructive,
                online,
                requires_approval,
                repeatable,
                timeout_ms,
                lock_timeout_ms,
                phase,
            },
        )
}

fn migration_ir_strategy() -> impl Strategy<Value = MigrationIr> {
    (
        any::<u32>(),
        any::<String>(),
        any::<String>(),
        op_list_strategy(),
        option::of(op_list_strategy()),
        option::of(any::<String>()),
        ir_flags_override_strategy(),
        pvec(migration_id_strategy(), 0..3),
        pvec(migration_id_strategy(), 0..3),
        pvec(precondition_check_strategy(), 0..3),
        option::of(any::<String>()),
    )
        .prop_map(
            |(
                ir_version,
                name,
                owner_app,
                ops,
                inverse_ops,
                irreversible,
                flags,
                depends_on,
                supersedes,
                preconditions,
                checksum,
            )| MigrationIr {
                ir_version,
                name,
                owner_app,
                ops,
                inverse_ops,
                irreversible,
                flags,
                depends_on: depends_on
                    .into_iter()
                    .map(|id| id.as_str().to_string())
                    .collect(),
                supersedes: supersedes
                    .into_iter()
                    .map(|id| id.as_str().to_string())
                    .collect(),
                preconditions,
                checksum,
            },
        )
}

// -- Serialization round-trips and canonical stability ------------------------

proptest! {
    /// `src/ir.rs`: `IrScalar` serializes to its wire form, parses back equal,
    /// and re-serializes byte-identically. The `bytes` carrier stores the
    /// DECODED payload, so the round-trip is the normalization the doc promises.
    #[test]
    fn ir_scalar_round_trips_through_its_wire_form(scalar in ir_scalar_strategy()) {
        let wire = serde_json::to_string(&scalar).expect("IrScalar serializes");
        let parsed: IrScalar = serde_json::from_str(&wire).expect("IrScalar parses");
        prop_assert_eq!(&parsed, &scalar);
        prop_assert_eq!(serde_json::to_string(&parsed).expect("reserializes"), wire);
    }

    /// `src/ir.rs` `IrScalar` deserialize: exact integers `|v| < 2^53` are
    /// admitted and become `Int`; every larger magnitude is refused.
    #[test]
    fn ir_scalar_admits_exactly_the_js_safe_integer_domain(value in boundary_i64_strategy()) {
        let parsed = serde_json::from_str::<IrScalar>(&value.to_string());
        let in_domain = value.unsigned_abs() < MAX_EXACT_INT as u64;
        prop_assert_eq!(parsed.is_ok(), in_domain);
        if in_domain {
            prop_assert_eq!(parsed.expect("in-domain parses"), IrScalar::Int(value));
        }
    }

    /// `src/ir.rs` `IrScalar` deserialize: a fractional / exponential token is
    /// refused with the structured `EXPR_INVALID_NUMERIC` code.
    #[test]
    fn ir_scalar_refuses_fractional_and_exponential_numbers(
        whole in any::<i64>(),
        fraction in 1u32..1_000_000u32,
        mantissa in 1i64..1_000_000i64,
        exponent in -30i32..30,
    ) {
        let fractional = format!("{whole}.{fraction}");
        let exponential = format!("{mantissa}e{exponent}");
        for token in [fractional, exponential] {
            let error = serde_json::from_str::<IrScalar>(&token)
                .expect_err("a fractional/exponential number must be refused");
            prop_assert!(
                error.to_string().contains(crate::ir::EXPR_INVALID_NUMERIC),
                "expected EXPR_INVALID_NUMERIC for {token}, got {error}"
            );
        }
    }

    /// `src/ir.rs` `SafeU64` / `SafeI64`: the deserialize bound is exactly the
    /// JS safe-integer domain the docs name.
    #[test]
    fn structural_integers_admit_exactly_the_js_safe_integer_domain(
        unsigned in boundary_u64_strategy(),
        signed in boundary_i64_strategy(),
    ) {
        let u = serde_json::from_str::<SafeU64>(&unsigned.to_string());
        prop_assert_eq!(u.is_ok(), unsigned < MAX_EXACT_INT as u64);
        if let Ok(parsed) = u {
            prop_assert_eq!(parsed.get(), unsigned);
        }

        let i = serde_json::from_str::<SafeI64>(&signed.to_string());
        prop_assert_eq!(i.is_ok(), signed > -MAX_EXACT_INT && signed < MAX_EXACT_INT);
        if let Ok(parsed) = i {
            prop_assert_eq!(parsed.get(), signed);
        }
    }

    /// `src/ir.rs`: the untagged `IrValue` / `BackfillSetValue` arms serialize
    /// and parse back equal, and the `perRow` wrapper is never confused with a
    /// plain scalar or expression.
    #[test]
    fn dml_value_carriers_round_trip_through_their_untagged_wire_form(
        value in dml_value_strategy(),
        backfill in backfill_set_value_strategy(),
    ) {
        let value_wire = serde_json::to_string(&value).expect("IrValue serializes");
        prop_assert_eq!(
            serde_json::from_str::<IrValue>(&value_wire).expect("IrValue parses"),
            value
        );

        let backfill_wire =
            serde_json::to_string(&backfill).expect("BackfillSetValue serializes");
        prop_assert_eq!(
            serde_json::from_str::<BackfillSetValue>(&backfill_wire)
                .expect("BackfillSetValue parses"),
            backfill
        );
    }

    /// `src/expr.rs`: the closed expression AST is a faithful wire contract -
    /// serialize, parse, and the re-serialized bytes are identical.
    #[test]
    fn expressions_round_trip_through_their_closed_wire_form(expr in expr_strategy()) {
        let wire = serde_json::to_string(&expr).expect("Expr serializes");
        let parsed: Expr = serde_json::from_str(&wire).expect("Expr parses");
        prop_assert_eq!(&parsed, &expr);
        prop_assert_eq!(serde_json::to_string(&parsed).expect("reserializes"), wire);
    }

    /// `src/ir.rs` module docs: the full envelope round-trips, and the canonical
    /// form is stable (`to_string(parse(to_string(x))) == to_string(x)`), which
    /// is what lets a null-bearing and an omitted-optional envelope checksum
    /// the same.
    #[test]
    fn migration_ir_round_trips_and_keeps_its_canonical_form(ir in migration_ir_strategy()) {
        let wire = serde_json::to_string(&ir).expect("MigrationIr serializes");
        let parsed: MigrationIr = serde_json::from_str(&wire).expect("MigrationIr parses");
        prop_assert_eq!(&parsed, &ir);
        prop_assert_eq!(serde_json::to_string(&parsed).expect("reserializes"), wire);
    }

    /// `src/ir.rs` `validate_type_id_prefix`: acceptance matches the documented
    /// grammar exactly - empty is valid; otherwise non-empty, at most the bound,
    /// lowercase-ASCII-letter at both ends, and only letters/underscores inside.
    #[test]
    fn type_id_prefix_acceptance_matches_the_documented_grammar(
        prefix in type_id_prefix_candidate_strategy(),
    ) {
        let bytes = prefix.as_bytes();
        let expected = prefix.is_empty()
            || (prefix.len() <= crate::ir::TYPE_ID_MAX_PREFIX_LEN
                && bytes.first().is_some_and(u8::is_ascii_lowercase)
                && bytes.last().is_some_and(u8::is_ascii_lowercase)
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || *byte == b'_'));
        prop_assert_eq!(crate::ir::validate_type_id_prefix(&prefix).is_ok(), expected);
    }

    /// `src/ir.rs` `is_decimal_string`: a well-formed plain numeric literal is
    /// accepted with no exponent and at least one digit.
    #[test]
    fn decimal_strings_accept_plain_numeric_literals(
        whole in any::<i64>(),
        fraction in option::of(1u32..1_000_000u32),
    ) {
        let literal = fraction
            .map_or_else(|| whole.to_string(), |fraction| format!("{whole}.{fraction}"));
        prop_assert!(crate::ir::is_decimal_string(&literal), "{literal}");
    }

    /// `src/ir.rs` `is_conservative_type_ref`: every accepted reference carries
    /// no whitespace, at most one schema qualifier, and no statement terminator.
    #[test]
    fn accepted_conservative_type_refs_keep_the_grammar_shape(
        input in prop_oneof![
            conservative_type_ref_strategy(),
            any::<String>(),
            Just(String::new()),
            Just("a.b.c".to_string()),
            Just("a b".to_string()),
            Just("a;b".to_string()),
        ],
    ) {
        if crate::ir::is_conservative_type_ref(&input) {
            prop_assert!(!input.chars().any(char::is_whitespace), "{input:?}");
            prop_assert!(input.matches('.').count() <= 1, "{input:?}");
            prop_assert!(!input.contains(';'), "{input:?}");
        }
    }
}

/// A generated reference drawn from the conservative type-reference grammar:
/// an identifier, an optional schema qualifier, an optional precision, and any
/// number of array suffixes.
fn conservative_type_ref_strategy() -> impl Strategy<Value = String> {
    (
        "[A-Za-z_][A-Za-z0-9_]*",
        option::of("\\.[A-Za-z_][A-Za-z0-9_]*"),
        option::of("(\\([0-9]+\\)|\\([0-9]+,[0-9]+\\))"),
        "(\\[\\]){0,3}",
    )
        .prop_map(|(base, qualifier, precision, arrays)| {
            format!(
                "{base}{}{}{arrays}",
                qualifier.unwrap_or_default(),
                precision.unwrap_or_default()
            )
        })
}

proptest! {
    /// `src/ir.rs` `is_conservative_type_ref`: the grammar it documents accepts
    /// the references it describes.
    #[test]
    fn conservative_type_refs_accept_generated_references(reference in conservative_type_ref_strategy()) {
        prop_assert!(
            crate::ir::is_conservative_type_ref(&reference),
            "a grammar-shaped reference must be accepted: {reference:?}"
        );
    }
}

// -- Checksum invariants ------------------------------------------------------

/// Build the [`ChecksumInput`] a rendered-SQL migration folds.
fn rendered_checksum_input<'a>(
    up: &'a str,
    down: Option<&'a str>,
    flags: &'a MigrationFlags,
    owner_app: &'a str,
    depends_on: &'a [MigrationId],
    supersedes: &'a [MigrationId],
    preconditions: &'a [PreconditionCheck],
) -> ChecksumInput<'a> {
    ChecksumInput {
        up,
        down,
        flags,
        owner_app,
        depends_on,
        supersedes,
        preconditions,
    }
}

proptest! {
    /// `src/migration.rs` `Checksum::of_ir`: equal inputs produce the same
    /// digest.
    #[test]
    fn of_ir_is_deterministic(
        ops in op_list_strategy(),
        flags in migration_flags_strategy(),
        owner in any::<String>(),
        depends_on in pvec(migration_id_strategy(), 0..3),
        supersedes in pvec(migration_id_strategy(), 0..3),
        preconditions in pvec(precondition_check_strategy(), 0..3),
    ) {
        let first = Checksum::of_ir(
            &CanonicalOpList(&ops),
            &flags,
            &owner,
            &depends_on,
            &supersedes,
            &preconditions,
        );
        let second = Checksum::of_ir(
            &CanonicalOpList(&ops),
            &flags,
            &owner,
            &depends_on,
            &supersedes,
            &preconditions,
        );
        prop_assert_eq!(first, second);
    }

    /// `src/migration.rs` `Checksum::of_ir`: the canonical op region folds the
    /// op count and each op body, so appending an op changes the digest.
    #[test]
    fn of_ir_changes_when_the_op_list_grows(
        ops in op_list_strategy(),
        extra in checksum_op_strategy(),
    ) {
        let flags = MigrationFlags::default();
        let before = Checksum::of_ir(&CanonicalOpList(&ops), &flags, "", &[], &[], &[]);
        let mut extended = ops;
        extended.push(extra);
        let after = Checksum::of_ir(&CanonicalOpList(&extended), &flags, "", &[], &[], &[]);
        prop_assert_ne!(before, after);
    }

    /// `src/ir.rs` `CanonicalOpList::canonical_bytes`: the op region is
    /// order-sensitive, so reordering two distinct ops shifts the bytes.
    #[test]
    fn canonical_op_bytes_are_order_sensitive(first in checksum_op_strategy(), second in checksum_op_strategy()) {
        prop_assume!(first != second);
        let forward = CanonicalOpList(&[first.clone(), second.clone()]).canonical_bytes();
        let reverse = CanonicalOpList(&[second, first]).canonical_bytes();
        prop_assert_ne!(forward, reverse);
    }

    /// `src/migration.rs` `Checksum::of_ir`: an apply flag flip changes the
    /// digest (flags are part of the migration identity).
    #[test]
    fn of_ir_changes_when_an_apply_flag_changes(
        ops in op_list_strategy(),
        mut flags in migration_flags_strategy(),
        owner in any::<String>(),
    ) {
        let before = Checksum::of_ir(&CanonicalOpList(&ops), &flags, &owner, &[], &[], &[]);
        flags.destructive = !flags.destructive;
        let after = Checksum::of_ir(&CanonicalOpList(&ops), &flags, &owner, &[], &[], &[]);
        prop_assert_ne!(before, after);
    }

    /// `src/migration.rs` `Checksum::of_ir`: `owner_app` is folded, so a
    /// re-owned migration drifts.
    #[test]
    fn of_ir_changes_when_the_owner_changes(
        ops in op_list_strategy(),
        first_owner in any::<String>(),
        second_owner in any::<String>(),
    ) {
        prop_assume!(first_owner != second_owner);
        let flags = MigrationFlags::default();
        let first = Checksum::of_ir(&CanonicalOpList(&ops), &flags, &first_owner, &[], &[], &[]);
        let second = Checksum::of_ir(&CanonicalOpList(&ops), &flags, &second_owner, &[], &[], &[]);
        prop_assert_ne!(first, second);
    }

    /// `src/migration.rs` `Checksum::of_ir`: `depends_on` and `supersedes` are
    /// both folded and domain-separated (a dependency list and a supersession
    /// list with the same member do not collide).
    #[test]
    fn of_ir_folds_dependencies_and_supersessions_apart(
        ops in op_list_strategy(),
        member in migration_id_strategy(),
    ) {
        let flags = MigrationFlags::default();
        let as_dependency =
            Checksum::of_ir(&CanonicalOpList(&ops), &flags, "", std::slice::from_ref(&member), &[], &[]);
        let as_supersession =
            Checksum::of_ir(&CanonicalOpList(&ops), &flags, "", &[], &[member], &[]);
        prop_assert_ne!(as_dependency, as_supersession);
    }

    /// `src/migration.rs`: a declared reverse is distinct from no reverse at
    /// all, and a declared empty inverse is distinct from both, so the reverse
    /// is not silently dropped from the identity.
    #[test]
    fn a_declared_reverse_is_distinct_from_its_absence(
        reason in any::<String>(),
        ops in op_list_strategy(),
    ) {
        let flags = MigrationFlags::default();
        let col = CanonicalOpList(&ops);
        let empty_inverse = CanonicalOpList(&[]);
        let none = Checksum::of_ir_strings_with_reverse(
            &col, &flags, "", &[], &[], &[], &ReverseDomain::None,
        );
        let inverse = Checksum::of_ir_strings_with_reverse(
            &col,
            &flags,
            "",
            &[],
            &[],
            &[],
            &ReverseDomain::Inverse(&empty_inverse),
        );
        let irreversible = Checksum::of_ir_strings_with_reverse(
            &col,
            &flags,
            "",
            &[],
            &[],
            &[],
            &ReverseDomain::Irreversible(&reason),
        );
        prop_assert_ne!(&none, &inverse);
        prop_assert_ne!(&none, &irreversible);
    }

    /// `src/load.rs` `authoritative_ir_checksum`: declaring an inverse moves the
    /// drift anchor, so editing the recorded reverse is drift exactly as editing
    /// the forward ops is.
    #[test]
    fn authoritative_ir_checksum_covers_the_declared_reverse(mut ir in migration_ir_strategy()) {
        ir.inverse_ops = None;
        ir.irreversible = None;
        let before = crate::load::authoritative_ir_checksum(&ir);
        ir.inverse_ops = Some(vec![]);
        let after = crate::load::authoritative_ir_checksum(&ir);
        prop_assert_ne!(before, after);
    }

    /// `src/migration.rs` `Checksum::of` and `Checksum::of_ir` are
    /// domain-separated: the IR checksum carries a domain tag, so the same
    /// identity tail cannot collide with a rendered-SQL migration.
    #[test]
    fn of_and_of_ir_are_domain_separated(
        op in checksum_op_strategy(),
        flags in migration_flags_strategy(),
        owner in any::<String>(),
    ) {
        let ops = [op];
        let rendered = Checksum::of(&rendered_checksum_input(
            "CREATE TABLE t()",
            None,
            &flags,
            &owner,
            &[],
            &[],
            &[],
        ));
        let ir = Checksum::of_ir(&CanonicalOpList(&ops), &flags, &owner, &[], &[], &[]);
        prop_assert_ne!(rendered, ir);
    }
}

// -- Typed-id codec and migration ids -----------------------------------------

proptest! {
    /// `src/id.rs` `uuid_to_base36` / `base36_to_uuid`: fixed-width base36 is a
    /// bijection with the UUID bytes.
    #[test]
    fn uuid_base36_codec_round_trips(value in any::<u128>()) {
        let uuid = uuid::Uuid::from_u128(value);
        let encoded = typed_id::uuid_to_base36(&uuid);
        prop_assert_eq!(encoded.len(), typed_id::BODY_LEN);
        prop_assert_eq!(
            typed_id::base36_to_uuid(&encoded).expect("round-trips"),
            uuid
        );
    }

    /// `src/id.rs` base36 alphabet: a fixed-width base36 encoding is
    /// order-preserving over the encoded integer, which is the property the
    /// UUIDv7 sort-order invariant depends on.
    #[test]
    fn uuid_base36_order_matches_the_encoded_integer(first in any::<u128>(), second in any::<u128>()) {
        let first = typed_id::uuid_to_base36(&uuid::Uuid::from_u128(first));
        let second = typed_id::uuid_to_base36(&uuid::Uuid::from_u128(second));
        prop_assert_eq!(
            first.cmp(&second),
            typed_id::base36_to_uuid(&first)
                .expect("first decodes")
                .as_u128()
                .cmp(&typed_id::base36_to_uuid(&second).expect("second decodes").as_u128())
        );
    }

    /// `src/migration.rs` `MigrationId::generate` / `parse`: a minted id carries
    /// the `mig` prefix and round-trips through `parse`.
    #[test]
    fn generated_migration_ids_parse_back_to_themselves(seed in any::<u64>()) {
        let id = migration_id_for_version(seed % VERSION_CEILING);
        prop_assert_eq!(&id, &MigrationId::parse(id.as_str()).expect("generated id parses"));
    }

    /// `src/migration.rs` `migration_id_for_version`: the numeric file version
    /// occupies the high 48 bits and the base36 string preserves that order, so
    /// string-sorting version ids yields numeric apply order.
    #[test]
    fn version_ids_preserve_numeric_order(first in 0u64..VERSION_CEILING, second in 0u64..VERSION_CEILING) {
        let first_id = migration_id_for_version(first);
        let second_id = migration_id_for_version(second);
        prop_assert_eq!(first_id.timestamp_ms(), first);
        prop_assert_eq!(first_id.cmp(&second_id), first.cmp(&second));
        prop_assert_eq!(first_id.as_str().cmp(second_id.as_str()), first.cmp(&second));
    }

    /// `src/migration.rs` `MigrationId::derive`: the derivation is deterministic
    /// and always carries the derived high-48-bit marker, so it can never be
    /// mistaken for a versioned id.
    #[test]
    fn derived_migration_ids_are_deterministic_and_marked(
        tag in any::<String>(),
        seed in pvec(any::<u8>(), 0..64),
    ) {
        let first = MigrationId::derive(&tag, &seed);
        let second = MigrationId::derive(&tag, &seed);
        prop_assert_eq!(&first, &second);
        prop_assert_eq!(first.timestamp_ms(), 0xFFFF_FFFF_FFFF);
    }

    /// `src/migration.rs` `MigrationId::parse`: arbitrary input either parses to
    /// an id that round-trips, or is refused - it never panics.
    #[test]
    fn migration_id_parse_never_panics(input in any::<String>()) {
        if let Ok(id) = MigrationId::parse(&input) {
            prop_assert_eq!(id.as_str(), input.as_str());
            prop_assert_eq!(
                MigrationId::parse(id.as_str()).expect("parsed id parses"),
                id
            );
        }
    }

    /// `src/migration.rs` `Checksum::of` over a rendered migration distinguishes
    /// `down: Some("")` from `down: None`, so a reversible empty down is not the
    /// same artifact as an explicitly irreversible one.
    #[test]
    fn rendered_checksum_separates_empty_down_from_no_down(up in any::<String>()) {
        let flags = MigrationFlags::default();
        let empty = Checksum::of(&rendered_checksum_input(
            &up,
            Some(""),
            &flags,
            "",
            &[],
            &[],
            &[],
        ));
        let absent = Checksum::of(&rendered_checksum_input(
            &up, None, &flags, "", &[], &[], &[],
        ));
        prop_assert_ne!(empty, absent);
    }
}

// -- Structural validator behavior --------------------------------------------

#[derive(Debug)]
struct AcceptAll;

impl ExprDialectValidator for AcceptAll {
    fn validate_expr_feature(
        &self,
        _feature: ExprDialectFeature<'_>,
    ) -> Result<(), ExprDialectRejection> {
        Ok(())
    }
}

struct Validators;

impl ExprDialectValidatorSet for Validators {
    fn get(&self, dialect: &DialectId) -> Option<&dyn ExprDialectValidator> {
        (dialect == &DialectId::new("postgres")).then_some(&AcceptAll)
    }
}

const POSTGRES: DialectId = DialectId::new("postgres");

proptest! {
    /// `src/validate.rs` `validate_expr`: the structural walk over the closed AST
    /// returns a verdict for every well-typed expression without panicking.
    #[test]
    fn validate_expr_never_panics_on_the_closed_ast(expr in expr_strategy()) {
        let scope = TargetScope::structural_only("t");
        let _ = validate_expr(&expr, &POSTGRES, &Validators, &scope, 0);
    }

    /// `src/validate.rs` rule (c): a `ColRef` naming a column absent from the
    /// resolved target scope is refused; one that resolves is accepted.
    #[test]
    fn col_refs_resolve_against_the_target_scope(
        name in any::<String>(),
        columns in pvec(any::<String>(), 0..4),
    ) {
        let resolved = columns.contains(&name);
        let scope = TargetScope::new("t", &columns);
        let expr = Expr::ColRef { name, table: None };
        prop_assert_eq!(
            validate_expr(&expr, &POSTGRES, &Validators, &scope, 0).is_ok(),
            resolved,
        );
    }

    /// `src/validate.rs` rule (b): a `splitPart` with a non-positive literal
    /// part index is outside the dialect-neutral envelope and is refused.
    #[test]
    fn split_part_refuses_a_non_positive_part_index(index in i64::MIN..=0) {
        let expr = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("x"),
                Expr::lit(IrScalar::Str("_".to_string())),
                Expr::lit(IrScalar::Int(index)),
            ],
        };
        let scope = TargetScope::structural_only("t");
        prop_assert!(validate_expr(&expr, &POSTGRES, &Validators, &scope, 0).is_err());
    }

    /// `src/validate.rs` rule (b): a `splitPart` with an empty literal
    /// delimiter is outside the envelope and is refused even when the index is
    /// positive.
    #[test]
    fn split_part_refuses_an_empty_delimiter(index in 1i64..1_000i64) {
        let expr = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("x"),
                Expr::lit(IrScalar::Str(String::new())),
                Expr::lit(IrScalar::Int(index)),
            ],
        };
        let scope = TargetScope::structural_only("t");
        prop_assert!(validate_expr(&expr, &POSTGRES, &Validators, &scope, 0).is_err());
    }

    /// `src/validate.rs` rule (b) control: an in-envelope `splitPart` is
    /// accepted, so the refusals above are not an always-reject walk.
    #[test]
    fn split_part_accepts_an_in_envelope_call(
        delimiter in "[a-z]{1,4}",
        index in 1i64..1_000i64,
    ) {
        let expr = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("x"),
                Expr::lit(IrScalar::Str(delimiter)),
                Expr::lit(IrScalar::Int(index)),
            ],
        };
        let scope = TargetScope::structural_only("t");
        prop_assert!(validate_expr(&expr, &POSTGRES, &Validators, &scope, 0).is_ok());
    }

    /// `src/validate.rs`: a `dialect({})` with no leg covers no target and is
    /// refused for every dialect.
    #[test]
    fn a_legless_dialectal_expression_is_unsupported(target in dialect_id_strategy()) {
        let expr = Expr::Dialectal {
            legs: BTreeMap::new(),
        };
        let scope = TargetScope::structural_only("t");
        prop_assert!(validate_expr(&expr, &target, &Validators, &scope, 0).is_err());
    }
}
