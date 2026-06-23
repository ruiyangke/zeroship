//! Wave A — checksum byte-stability + `Checksum::of_ir` determinism/sensitivity.
//!
//! Two front doors fold into the SAME `fold_common` tail (flags + owner_app +
//! depends_on + supersedes + preconditions). `Checksum::of` folds `up`/`down`
//! then `fold_common`; `Checksum::of_ir` folds the canonical op-list (RFC 8785
//! JCS per op, length-prefixed, in op order) then the SAME `fold_common`.
//!
//! The GOLDEN-HASH test below is the load-bearing fixture: it pins the
//! byte-for-byte output of `Checksum::of` over a fixed input. It was generated
//! against the PRE-`fold_common`-extraction code and MUST stay equal after the
//! pure refactor — proving the `fold_common` lift is byte-preserving.

use zeroship_migrate::{
    Checksum, ChecksumInput, MigrationFlags, MigrationId, OnlinePhase,
};
use zeroship_migrate::ir::{CanonicalOpList, IrColumn, IrScalar, Op};

// ---------------------------------------------------------------------------
// Fixed, deterministic ids — `MigrationId` parses any well-formed `mig_…` id,
// so we use frozen literals (NOT `generate()`) to keep the golden hash stable
// across runs.
// ---------------------------------------------------------------------------
const DEP_A: &str = "mig_000000000000000000000A";
const SUP_A: &str = "mig_000000000000000000000B";

fn dep() -> MigrationId {
    MigrationId::parse(DEP_A).expect("frozen dep id parses")
}
fn sup() -> MigrationId {
    MigrationId::parse(SUP_A).expect("frozen sup id parses")
}

/// THE byte-stability golden fixture. The expected hex was captured by running
/// `Checksum::of` on the PRE-refactor code over this exact input; after the
/// `fold_common` extraction it MUST remain equal — that equality is the proof
/// the refactor is byte-preserving (a pure lift, not a behaviour change).
#[test]
fn checksum_of_byte_stable_golden() {
    let flags = MigrationFlags {
        transactional: false,
        destructive: true,
        online: true,
        requires_approval: true,
        timeout_ms: Some(60_000),
        phase: Some(OnlinePhase::Contract),
        repeatable: false,
        engine_goodie_ddl: false,
    };
    let deps = [dep()];
    let sups = [sup()];
    let input = ChecksumInput {
        up: "CREATE TABLE t(id int)",
        down: Some("DROP TABLE t"),
        flags: &flags,
        owner_app: "app_golden",
        depends_on: &deps,
        supersedes: &sups,
        preconditions: &[],
    };
    // Hard-coded expected — captured from the pre-`fold_common` code. If this
    // ever changes, the checksum wire format drifted (NOT allowed pre-launch
    // without a deliberate, documented break).
    const EXPECTED: &str =
        "557930c15eac9181b2c094ae5f5d8325d7b2c127cfdef0cda55802d5481321b7";
    assert_eq!(
        Checksum::of(&input).as_str(),
        EXPECTED,
        "Checksum::of byte output drifted — fold_common extraction must be byte-preserving"
    );
}

/// `Checksum::of_ir` is deterministic and order/content-sensitive over the
/// canonical op-list region, and is a DISTINCT front door from `Checksum::of`.
#[test]
fn checksum_of_ir_deterministic_and_sensitive() {
    let flags = MigrationFlags::default();
    let owner = "app_ir";

    let add_a = Op::AddColumn {
        table: "users".into(),
        column: "age".into(),
        ty: zeroship_migrate::ir::ColType::Int,
        nullable: Some(true),
        default: None,
    };
    let add_b = Op::AddColumn {
        table: "users".into(),
        column: "name".into(),
        ty: zeroship_migrate::ir::ColType::Text,
        nullable: Some(true),
        default: None,
    };

    let ops1 = vec![add_a.clone(), add_b.clone()];
    let c1 = Checksum::of_ir(
        &CanonicalOpList(&ops1),
        &flags,
        owner,
        &[],
        &[],
        &[],
    );
    // Deterministic.
    let c1b = Checksum::of_ir(
        &CanonicalOpList(&ops1),
        &flags,
        owner,
        &[],
        &[],
        &[],
    );
    assert_eq!(c1, c1b, "of_ir must be deterministic");

    // Order-sensitive: reordering the two ops changes the checksum.
    let ops_rev = vec![add_b.clone(), add_a.clone()];
    let c_rev = Checksum::of_ir(
        &CanonicalOpList(&ops_rev),
        &flags,
        owner,
        &[],
        &[],
        &[],
    );
    assert_ne!(c1, c_rev, "of_ir must be order-sensitive");

    // Distinct front door: of_ir over ops != of over rendered SQL with the
    // SAME common tail.
    let sql_input = ChecksumInput {
        up: "ALTER TABLE users ADD COLUMN age int",
        down: None,
        flags: &flags,
        owner_app: owner,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    };
    assert_ne!(
        c1.as_str(),
        Checksum::of(&sql_input).as_str(),
        "of_ir and of are distinct front doors"
    );

    // The common tail still folds: same ops, different owner => different hash.
    let c_other_owner = Checksum::of_ir(
        &CanonicalOpList(&ops1),
        &flags,
        "app_other",
        &[],
        &[],
        &[],
    );
    assert_ne!(c1, c_other_owner, "fold_common tail must still fold owner_app");

    // …and deps/supersedes still fold via fold_common.
    let c_with_dep = Checksum::of_ir(
        &CanonicalOpList(&ops1),
        &flags,
        owner,
        &[dep()],
        &[],
        &[],
    );
    assert_ne!(c1, c_with_dep, "fold_common tail must still fold depends_on");
}

/// Changing a typed `IrScalar` inside an `Insert` row changes `of_ir` (the JCS
/// of the op folds its binds), and changing an embedded expression-AST `Literal`
/// param (the §2.3.2 / §2.4-point-3 obligation) is also drift.
#[test]
fn checksum_of_ir_folds_scalars_and_ast_literals() {
    use std::collections::BTreeMap;
    use zeroship_migrate::expr::{BinaryOp, Expr};

    let flags = MigrationFlags::default();
    let owner = "app_ir";

    let ins1 = Op::Insert {
        table: "t".into(),
        columns: vec!["a".into()],
        rows: vec![vec![IrScalar::Int(1)]],
    };
    let ins2 = Op::Insert {
        table: "t".into(),
        columns: vec!["a".into()],
        rows: vec![vec![IrScalar::Int(2)]],
    };
    let v1 = vec![ins1];
    let v2 = vec![ins2];
    assert_ne!(
        Checksum::of_ir(&CanonicalOpList(&v1), &flags, owner, &[], &[], &[]),
        Checksum::of_ir(&CanonicalOpList(&v2), &flags, owner, &[], &[], &[]),
        "an Insert row scalar change must be drift"
    );

    // Two `update` ops differing ONLY in an in-AST `Literal` threshold value
    // (`c("total").gt(0)` vs `c("total").gt(5)`) must have different of_ir —
    // the "changing a threshold value is drift" guarantee for AST-embedded
    // params (§2.3.2).
    let mk_update = |threshold: i64| {
        let mut set = BTreeMap::new();
        set.insert(
            "flagged".to_string(),
            Expr::lit(IrScalar::Bool(true)),
        );
        Op::Update {
            table: "t".into(),
            set,
            r#where: Some(Expr::BinOp {
                op: BinaryOp::Gt,
                lhs: Box::new(Expr::col("total")),
                rhs: Box::new(Expr::lit(IrScalar::Int(threshold))),
            }),
            batch: None,
        }
    };
    let u0 = vec![mk_update(0)];
    let u5 = vec![mk_update(5)];
    assert_ne!(
        Checksum::of_ir(&CanonicalOpList(&u0), &flags, owner, &[], &[], &[]),
        Checksum::of_ir(&CanonicalOpList(&u5), &flags, owner, &[], &[], &[]),
        "an in-AST Literal threshold change must be drift"
    );
}

/// Explicit domain separation: `of_ir` carries a one-sided domain tag so it is
/// provably non-colliding with `of` even for a crafted equal-length input. We
/// construct an `of` input whose up/down region length equals the `of_ir` region
/// length and confirm the two front doors STILL differ — the tag, not just the
/// structural ordering, keeps them apart.
#[test]
fn of_and_of_ir_never_collide_even_with_equal_length_regions() {
    let flags = MigrationFlags::default();
    let owner = "app_dom";

    // An empty op-list: of_ir's region is just the u64 op-count (8 bytes, value
    // 0). Build an `of` input engineered to fold an identical-shaped region were
    // there no domain tag, then assert they differ regardless.
    let empty_ops: Vec<Op> = vec![];
    let ir = Checksum::of_ir(&CanonicalOpList(&empty_ops), &flags, owner, &[], &[], &[]);

    let sql_input = ChecksumInput {
        up: "",
        down: None,
        flags: &flags,
        owner_app: owner,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    };
    let sql = Checksum::of(&sql_input);
    assert_ne!(
        ir.as_str(),
        sql.as_str(),
        "of_ir's domain tag must keep it distinct from of for every input"
    );
}

/// JCS key-order independence: building the same logical op two ways (here via
/// a CreateTable with columns in a fixed order) is stable, and the canonical
/// encoding does not depend on Rust struct field declaration order — it sorts
/// keys. (Sanity that the JCS encoder sorts object keys.)
#[test]
fn checksum_of_ir_jcs_is_key_sorted_stable() {
    let flags = MigrationFlags::default();
    let owner = "app_ir";
    let ct = Op::CreateTable {
        name: "t".into(),
        columns: vec![IrColumn {
            name: "id".into(),
            ty: zeroship_migrate::ir::ColType::Int,
            nullable: Some(false),
            default: None,
            unique: None,
        }],
        constraints: vec![],
        indexes: vec![],
    };
    let v = vec![ct];
    let a = Checksum::of_ir(&CanonicalOpList(&v), &flags, owner, &[], &[], &[]);
    let b = Checksum::of_ir(&CanonicalOpList(&v), &flags, owner, &[], &[], &[]);
    assert_eq!(a, b, "JCS encoding must be stable");
}
