//! The recorded reverse of a data migration, at the wire and the load gate.
//!
//! A `data()` migration declares exactly one of `inverse()` or `irreversible`.
//! Before this, the recorder recorded the inverse and `buildEnvelope` then
//! dropped it: the author wrote a reverse, every gate accepted it, and nothing
//! downstream ever saw it. That is the accepted-and-discarded shape this project
//! treats as a defect in its own right (F651, F652).
//!
//! Four properties are worth pinning, and three of them are about what must NOT
//! change:
//!
//!   1. an envelope declaring no reverse checksums EXACTLY as before. Every
//!      already-applied migration journaled a digest computed without these
//!      fields, and the deploy path's drift gate compares against those. The
//!      pinned golden in `ir_checksum.rs` is the other half of this proof.
//!   2. `Some(vec![])` and `None` must not collide. An author who wrote an empty
//!      `inverse()` said something different from one who wrote no reverse, and a
//!      fold that skips empty regions cannot tell them apart.
//!   3. declaring BOTH a reverse and a reason there is none is refused. The two
//!      are alternatives; an artifact carrying both leaves a rollback with no
//!      single answer, and picking one silently is how an operator gets a reverse
//!      that was explicitly disclaimed.
//!   4. the inverse is validated when the migration is AUTHORED, by the same gate
//!      the forward ops pass. A reverse first checked when a rollback needs it is
//!      checked at the worst possible moment, by someone who did not write it.
//!
//! GATE: none. Every case is offline.

use std::collections::BTreeMap;

use zero_migrate::model::ir::{ColType, IrColumn, IrScalar, IrValue, MigrationIr, Op};
use zero_migrate::model::load::{authoritative_ir_checksum, load_ir_document, IrLoadError};
use zero_migrate::model::validate::Dialect;

const OWNER: &str = "app_reverse";
const TABLE: &str = "acct";

fn registry() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert(TABLE.to_string(), OWNER.to_string());
    map
}

/// One row in, by id — the forward half of a reversible data migration.
fn insert_op() -> Op {
    Op::Insert {
        table: TABLE.to_string(),
        columns: vec!["id".to_string()],
        rows: vec![vec![IrScalar::Int(1).into()]],
        on_conflict: None,
        schema: None,
    }
}

/// The same row out again — an exact reverse.
fn delete_op() -> Op {
    delete_from(TABLE)
}

fn delete_from(table: &str) -> Op {
    Op::Delete {
        table: table.to_string(),
        r#where: zero_migrate::Expr::BinOp {
            op: zero_migrate::BinaryOp::Eq,
            lhs: Box::new(zero_migrate::Expr::col("id")),
            rhs: Box::new(zero_migrate::Expr::lit(IrScalar::Int(1))),
        },
        limit: None,
        schema: None,
    }
}

fn envelope(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: "seed".to_string(),
        owner_app: OWNER.to_string(),
        ops,
        inverse_ops: None,
        irreversible: None,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

#[test]
fn declaring_no_reverse_leaves_the_checksum_byte_identical() {
    // The digest is pinned as a literal captured from the code BEFORE the reverse
    // fields existed. If a future change folds something unconditionally, this is
    // the test that catches it -- and the cost of missing it is every stored
    // journal digest going stale at once, which surfaces as a drift abort on a
    // migration nobody touched.
    // Captured by running `authoritative_ir_checksum` over this exact envelope in
    // a worktree at the commit BEFORE the reverse fields existed, not by copying
    // what the current code prints.
    const EXPECTED: &str = "f848c0e92cd6be196d4b5af59bc361de78cd652112a556baf1e48ddc6f381738";
    let ir = envelope(vec![insert_op()]);
    assert_eq!(
        authoritative_ir_checksum(&ir).as_str(),
        EXPECTED,
        "an envelope declaring no reverse must hash exactly as it did before the \
         reverse fields existed; every already-applied migration's journaled \
         digest depends on it"
    );
}

#[test]
fn an_empty_inverse_and_no_inverse_are_different_artifacts() {
    let none = envelope(vec![insert_op()]);
    let mut empty = envelope(vec![insert_op()]);
    empty.inverse_ops = Some(vec![]);

    assert_ne!(
        authoritative_ir_checksum(&none).as_str(),
        authoritative_ir_checksum(&empty).as_str(),
        "an author who wrote an empty inverse() said something different from one \
         who wrote no reverse at all; a fold that skips empty regions collapses \
         the two and silently accepts a swap between them"
    );
}

#[test]
fn editing_the_inverse_moves_the_checksum() {
    // Without this, the reverse could be carried and NOT covered -- editing it
    // would leave the drift anchor unmoved, which is the same defect one level
    // down: a field that travels but does not count.
    let mut a = envelope(vec![insert_op()]);
    a.inverse_ops = Some(vec![delete_op()]);
    let mut b = envelope(vec![insert_op()]);
    b.inverse_ops = Some(vec![insert_op()]);

    assert_ne!(
        authoritative_ir_checksum(&a).as_str(),
        authoritative_ir_checksum(&b).as_str(),
        "the reverse is part of the artifact, so editing it must move the anchor"
    );
}

#[test]
fn a_reverse_and_a_reason_it_has_none_cannot_both_be_declared() {
    let mut ir = envelope(vec![insert_op()]);
    ir.inverse_ops = Some(vec![delete_op()]);
    ir.irreversible = Some("the source rows are gone".to_string());

    let error = load_ir_document(
        &serde_json::to_string(&ir).expect("envelope serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect_err("an envelope declaring both must be refused");

    assert!(
        matches!(error, IrLoadError::ReverseDeclaredBothWays { .. }),
        "got {error:?}"
    );
    let text = error.to_string();
    assert!(
        text.contains("inverse_ops") && text.contains("irreversible"),
        "the refusal must name BOTH fields so the author knows which to drop; got {text}"
    );
}

#[test]
fn an_inverse_reaching_a_table_the_app_does_not_own_is_refused() {
    // The forward gate never looks at the inverse, so without a matching check
    // the reverse is a way to reach a table ownership denies -- one that only
    // executes later, during a rollback, when nobody is reading the diff.
    let mut ir = envelope(vec![insert_op()]);
    ir.inverse_ops = Some(vec![delete_from("someone_elses")]);

    let error = load_ir_document(
        &serde_json::to_string(&ir).expect("envelope serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect_err("an inverse targeting an unowned table must be refused");

    assert!(
        matches!(error, IrLoadError::NotTableOwner { .. }),
        "ownership must cover the inverse, not just the forward ops; got {error:?}"
    );
}

#[test]
fn an_inverse_that_could_never_apply_is_refused_when_the_migration_is_authored() {
    // THIS is why the inverse is validated here rather than at rollback. An
    // aggregate in a row predicate is refused by the structural gate; if only the
    // forward ops went through it, this reverse would sit in the artifact looking
    // fine and fail the moment an operator reached for it.
    let mut ir = envelope(vec![insert_op()]);
    ir.inverse_ops = Some(vec![Op::Delete {
        table: TABLE.to_string(),
        r#where: zero_migrate::Expr::BinOp {
            op: zero_migrate::BinaryOp::Gt,
            lhs: Box::new(zero_migrate::Expr::Agg {
                func: zero_migrate::model::expr::AggFunc::Count,
                arg: Some(Box::new(zero_migrate::Expr::col("id"))),
                delimiter: None,
                distinct: false,
            }),
            rhs: Box::new(zero_migrate::Expr::lit(IrScalar::Int(0))),
        },
        limit: None,
        schema: None,
    }]);

    let error = load_ir_document(
        &serde_json::to_string(&ir).expect("envelope serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect_err("an inverse the validator refuses must not reach the artifact");

    assert!(
        matches!(error, IrLoadError::InvalidInverse { .. }),
        "the failure must be reported as an INVERSE failure, not a forward one -- \
         the author has to know which half to fix; got {error:?}"
    );
    assert!(
        error.to_string().contains("inverse"),
        "and the message must say so: {error}"
    );
}

#[test]
fn a_valid_reverse_loads_and_survives_the_round_trip() {
    // The control. Without it, every refusal above is equally consistent with
    // having broken reverses altogether.
    let mut ir = envelope(vec![insert_op()]);
    ir.inverse_ops = Some(vec![delete_op()]);
    let bytes = serde_json::to_string(&ir).expect("envelope serializes");

    let loaded = load_ir_document(&bytes, OWNER, Dialect::Postgres, &registry(), None)
        .expect("a data migration with a valid inverse must load");

    assert_eq!(
        loaded.inverse_ops,
        Some(vec![delete_op()]),
        "the inverse must arrive intact on the far side of the boundary"
    );

    // And the wire itself carries it under the name the recorder emits.
    let json: serde_json::Value = serde_json::from_str(&bytes).expect("valid JSON");
    assert!(
        json.get("inverse_ops").is_some(),
        "the wire field is inverse_ops; a rename here silently drops the reverse"
    );
}

#[test]
fn an_envelope_with_no_reverse_omits_both_keys_entirely() {
    // `skip_serializing_if` is what keeps a schema migration's wire shape at
    // exactly three keys. A `null` here would be a new key on every envelope the
    // engine ever writes.
    let ir = envelope(vec![insert_op()]);
    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&ir).expect("serializes")).expect("valid JSON");
    assert!(json.get("inverse_ops").is_none(), "absent, not null");
    assert!(json.get("irreversible").is_none(), "absent, not null");
}

#[test]
fn an_irreversible_reason_survives_the_boundary() {
    let mut ir = envelope(vec![insert_op()]);
    ir.irreversible = Some("the pre-image is not recoverable".to_string());

    let loaded = load_ir_document(
        &serde_json::to_string(&ir).expect("serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect("an irreversible data migration must load");

    assert_eq!(
        loaded.irreversible.as_deref(),
        Some("the pre-image is not recoverable"),
        "an operator reads this reason mid-incident, so it must arrive in the \
         author's words rather than as a bare flag"
    );
}

#[test]
fn a_declared_reverse_makes_an_advisory_hint_refuse_rather_than_compare() {
    // The hint domain does not fold the reverse yet, and no builder computes it.
    // Comparing a hint against a domain that excludes a present field both
    // false-rejects a correct hint and false-accepts tampering of what was left
    // out, so the gate refuses -- the same rule flags/deps already follow.
    let mut ir = envelope(vec![insert_op()]);
    ir.inverse_ops = Some(vec![delete_op()]);
    ir.checksum = Some("0".repeat(64));

    let error = load_ir_document(
        &serde_json::to_string(&ir).expect("serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect_err("a hint alongside a reverse must be refused, not compared");

    assert!(
        matches!(
            error,
            IrLoadError::ChecksumHintNotComputable { field, .. } if field == "inverse_ops"
        ),
        "got {error:?}"
    );
}

/// A `createTable` in the forward ops of a table the registry does not name still
/// establishes ownership -- and that must keep working once the inverse is also
/// checked, or every create-then-seed pair breaks.
#[test]
fn the_create_table_ownership_rule_still_applies_with_a_reverse_present() {
    let mut ir = envelope(vec![Op::CreateTable {
        name: "fresh".to_string(),
        columns: vec![IrColumn {
            name: "id".to_string(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }],
        primary_key: Some(vec!["id".to_string()]),
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }]);
    ir.irreversible = Some("dropping the table would take the rows with it".to_string());

    load_ir_document(
        &serde_json::to_string(&ir).expect("serializes"),
        OWNER,
        Dialect::Postgres,
        &BTreeMap::new(),
        None,
    )
    .expect("a createTable still establishes its own ownership");
}
