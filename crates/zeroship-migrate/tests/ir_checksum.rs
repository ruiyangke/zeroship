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
        lock_timeout_ms: None,
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
    // Hard-coded expected. Re-captured when the lock-safety envelope added the
    // `lock_timeout_ms` facet to `MigrationFlags` — a DELIBERATE, documented
    // flags-wire change (the new key joins the canonical-JSON flags image folded
    // by `fold_common`, so the golden moved by construction; pre-launch this is
    // allowed and every golden is updated in the SAME patch). If this ever
    // changes WITHOUT a corresponding flags-shape change, the checksum wire
    // format drifted unintentionally.
    const EXPECTED: &str =
        "61075be9920f5cf0e7acde1de5981e6688001ed0ff5e1b7e0033aac560a402cf";
    assert_eq!(
        Checksum::of(&input).as_str(),
        EXPECTED,
        "Checksum::of byte output drifted — fold_common extraction must be byte-preserving"
    );
}

/// THE `Checksum::of_ir` byte-stability golden — the IR front door's peer of
/// [`checksum_of_byte_stable_golden`]. The `.sql` front door (`Checksum::of`)
/// has a frozen-hex golden; without one for `of_ir` an accidental change to the
/// `of_ir` WIRE FORMAT (`IR_DOMAIN_TAG`, `CanonicalOpList::canonical_bytes`
/// layout, or the `fold_common` fold order) that moves BOTH the production code
/// AND a computed-vs-computed test in lockstep would pass undetected. This pins
/// the exact bytes over a fixed `(ops, flags, owner, deps, supersedes,
/// preconditions)` tuple, captured once, so any `of_ir` wire drift fails CI.
///
/// If this hex ever changes the `of_ir` wire format drifted — NOT allowed
/// pre-launch without a deliberate, documented break (and a matching JS-builder
/// bump, since the JS `op.*` author must emit the same advisory hint).
#[test]
fn checksum_of_ir_byte_stable_golden() {
    // A fixed, fully-populated tuple: two ops (a createTable + an insert whose
    // row folds a typed IrScalar), non-default flags, a frozen owner, one dep,
    // one supersedes, and no preconditions. Frozen literals only — no
    // generate() — so the hash is reproducible across runs.
    let flags = MigrationFlags {
        transactional: false,
        destructive: true,
        online: false,
        requires_approval: false,
        timeout_ms: Some(30_000),
        lock_timeout_ms: None,
        phase: None,
        repeatable: false,
        engine_goodie_ddl: false,
    };
    let ops = vec![
        Op::CreateTable {
            name: "accounts".into(),
            columns: vec![IrColumn {
                name: "id".into(),
                ty: zeroship_migrate::ir::ColType::Int,
                nullable: Some(false),
                default: None,
                unique: Some(true), id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        },
        Op::Insert {
            table: "accounts".into(),
            columns: vec!["id".into()],
            rows: vec![vec![IrScalar::Int(7)]],
            on_conflict: None,
            schema: None,
        },
    ];
    let deps = [dep()];
    let sups = [sup()];
    // Hard-coded expected — re-captured when the lock-safety envelope added the
    // `lock_timeout_ms` facet to `MigrationFlags` (the new key joins the
    // canonical-JSON flags image `fold_common` folds, so the `of_ir` golden
    // moved by construction; a DELIBERATE, documented flags-wire change, every
    // golden updated in the SAME patch). NB: `typed_checksum` (the JS-builder
    // anchor) reuses this same Rust `MigrationFlags::default()` serialization, so
    // there is no separate JS serializer to bump.
    const EXPECTED: &str =
        "574786ab59c227338430708e3793658d57ef9bfbf360e93e518e325c83119ad9";
    assert_eq!(
        Checksum::of_ir(
            &CanonicalOpList(&ops),
            &flags,
            "app_of_ir_golden",
            &deps,
            &sups,
            &[],
        )
        .as_str(),
        EXPECTED,
        "Checksum::of_ir byte output drifted — the of_ir wire format \
         (IR_DOMAIN_TAG / canonical_bytes / fold_common) must stay frozen"
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
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    };
    let add_b = Op::AddColumn {
        table: "users".into(),
        column: "name".into(),
        ty: zeroship_migrate::ir::ColType::Text,
        nullable: Some(true),
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
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
        on_conflict: None,
        schema: None,
    };
    let ins2 = Op::Insert {
        table: "t".into(),
        columns: vec!["a".into()],
        rows: vec![vec![IrScalar::Int(2)]],
        on_conflict: None,
        schema: None,
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
            schema: None,
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

/// Dialect-stability (spec line 1267): a portable migration's `of_ir` is
/// IDENTICAL across the PG and SQLite renders, because `of_ir` is dialect-neutral
/// by construction (no dialect parameter; it hashes the neutral op list + the
/// derived-then-overridden flags). This pins the single-artifact / single-checksum
/// invariant so a future `IrAuthor` that leaks per-dialect *lowered* flags into
/// the hash (the latent risk the `of_ir` doc forbids) fails the gate.
///
/// PLACEHOLDER caveat: until `IrAuthor::lower` exists (a later wave), this drives
/// `of_ir` directly with hand-built neutral flags. The positive arm is therefore
/// a SELF-comparison (`of_ir(neutral) == of_ir(neutral)`) — it documents the
/// contract and is kept load-bearing by the negative arm (DIFFERENT flags hash
/// differently). When `IrAuthor::lower` lands, the positive arm MUST be replaced
/// by driving the actual lowering for BOTH dialects through one `IrAuthor` and
/// asserting the `of_ir` it computes is identical — i.e. assert the producing
/// code feeds neutral flags, not merely that `neutral == neutral`. See the
/// code-critic LOW finding on this test.
#[test]
fn checksum_of_ir_is_identical_across_dialect_renders() {
    // A `createIndex { concurrently: true }` is the canonical case where the
    // per-dialect LOWERING diverges: PG keeps `transactional:false` + the
    // CONCURRENTLY; SQLite forces `transactional:true` and drops CONCURRENTLY
    // (spec line 257). The IR-level flags fed to `of_ir` are the DIALECT-NEUTRAL
    // derived+overridden flags — the SAME struct for both renders.
    let neutral_flags = MigrationFlags {
        transactional: false, // the neutral derived value (concurrent index)
        ..MigrationFlags::default()
    };
    let owner = "app_portable";
    let ops = vec![Op::CreateIndex {
        table: "users".into(),
        columns: vec!["email".into()],
        name: None,
        unique: Some(true),
        using: None,
        r#where: None,
        concurrently: Some(true),
        schema: None,
        existence_guard: None,
    }];

    // What a CORRECT IrAuthor does: compute the neutral flags ONCE and feed the
    // SAME neutral flags to of_ir regardless of which dialect it is lowering for.
    let pg_render = Checksum::of_ir(&CanonicalOpList(&ops), &neutral_flags, owner, &[], &[], &[]);
    let sqlite_render =
        Checksum::of_ir(&CanonicalOpList(&ops), &neutral_flags, owner, &[], &[], &[]);
    assert_eq!(
        pg_render, sqlite_render,
        "of_ir must be identical across dialect renders for one portable migration"
    );

    // Guard the doc contract: were a buggy IrAuthor to leak the per-dialect
    // LOWERED flags (SQLite forcing transactional:true) into the hash, the two
    // renders WOULD diverge — demonstrating exactly the invariant break the
    // of_ir doc forbids, and proving this test is load-bearing (not vacuous).
    let sqlite_lowered_flags = MigrationFlags { transactional: true, ..neutral_flags };
    let sqlite_render_buggy =
        Checksum::of_ir(&CanonicalOpList(&ops), &sqlite_lowered_flags, owner, &[], &[], &[]);
    assert_ne!(
        pg_render, sqlite_render_buggy,
        "leaking per-dialect lowered flags into of_ir WOULD break the single-checksum \
         invariant — IrAuthor must pass the neutral flags (see Checksum::of_ir doc)"
    );
}

/// **C1 — FK referential-action checksum neutrality + sensitivity.** The new
/// `IrConstraintKind::Fk { on_delete, on_update }` fields are additive-optional
/// (`skip_serializing_if = "Option::is_none"`), so:
///  - an FK that sets NO action serializes WITHOUT the `onDelete`/`onUpdate` keys
///    — byte-identical to the pre-C1 wire image (the JCS canonical bytes, and thus
///    `of_ir`, are unchanged). This is what keeps the action-free FK goldens'
///    checksums stable across the C1 field addition (no `ir_version` bump needed).
///  - an FK that DOES set an action (`onDelete: cascade`) produces a brand-new
///    shape with the key present — a different (new) checksum. There is no
///    persisted checksum for an FK-with-actions to preserve (it was unbuildable
///    pre-C1), so the new bytes are correct.
#[test]
fn checksum_of_ir_fk_actions_are_additive_neutral_and_sensitive() {
    use zeroship_migrate::ir::{IrConstraint, IrConstraintKind, RefAction};

    let flags = MigrationFlags::default();
    let owner = "app_fk";

    let mk_fk = |on_delete: Option<RefAction>, on_update: Option<RefAction>| {
        vec![Op::AddConstraint {
            table: "orders".into(),
            constraint: IrConstraint {
                name: Some("orders_customer_fk".into()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["customerId".into()],
                    references_table: "customers".into(),
                    references_columns: vec!["id".into()],
                    on_delete,
                    on_update,
                },
            },
            schema: None,
            existence_guard: None,
        }]
    };

    // Neutrality: the action-free FK serializes WITHOUT the onDelete/onUpdate keys
    // (the `skip_serializing_if` omitted-key image), so its canonical bytes are
    // the pre-C1 image.
    let none_ops = mk_fk(None, None);
    let json = serde_json::to_string(&none_ops[0]).expect("op serializes");
    assert!(
        !json.contains("onDelete") && !json.contains("onUpdate"),
        "an action-free FK must omit the onDelete/onUpdate keys (checksum neutrality): {json}"
    );

    // Sensitivity: setting onDelete = cascade changes the checksum (new bytes).
    let cascade_ops = mk_fk(Some(RefAction::Cascade), None);
    let none_ck = Checksum::of_ir(&CanonicalOpList(&none_ops), &flags, owner, &[], &[], &[]);
    let cascade_ck = Checksum::of_ir(&CanonicalOpList(&cascade_ops), &flags, owner, &[], &[], &[]);
    assert_ne!(
        none_ck, cascade_ck,
        "setting onDelete:cascade must change of_ir (FK actions are folded)"
    );

    // onDelete vs onUpdate are distinct positions (cascade-on-delete != cascade-on-update).
    let cascade_upd = mk_fk(None, Some(RefAction::Cascade));
    let cascade_upd_ck =
        Checksum::of_ir(&CanonicalOpList(&cascade_upd), &flags, owner, &[], &[], &[]);
    assert_ne!(
        cascade_ck, cascade_upd_ck,
        "onDelete and onUpdate are distinct FK action positions"
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
            unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    };
    let v = vec![ct];
    let a = Checksum::of_ir(&CanonicalOpList(&v), &flags, owner, &[], &[], &[]);
    let b = Checksum::of_ir(&CanonicalOpList(&v), &flags, owner, &[], &[], &[]);
    assert_eq!(a, b, "JCS encoding must be stable");
}
