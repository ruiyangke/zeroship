//! A backend may only declare attributes in its OWN namespace.
//!
//! [`AttrKey`] guarantees a key is SHAPED `<dialect>.<name>`, and it does so at compile
//! time — a malformed key in a vendor's `static` is a const-eval failure in that vendor's
//! own crate. What the key cannot guarantee is OWNERSHIP: nothing stops the PostgreSQL
//! crate declaring `mysql.row_format`, because "which dialect owns this crate" is a
//! registry fact and a leaf key has no registry.
//!
//! So ownership is checked here, in the one composition that can see all three vendors at
//! once. Getting it wrong is not a loud failure: a key declared under the wrong namespace
//! is simply never matched — `check` skips keys belonging to another dialect by design,
//! which is what keeps a node authored for three backends portable. The mis-declared knob
//! would silently do nothing on every target, forever.
//!
//! # This census must not fail open
//!
//! Every assertion below ranges over a DISCOVERED set: the vendors that exist and the
//! attributes they happen to declare. Both can go to zero, and a zero-length loop passes
//! every assertion inside it. The floors are what stop that, and each one is measured
//! against the tree rather than guessed.

use std::collections::BTreeMap;

use zeroship_migrate_backend::registry::BackendVendor;

/// The vendors this file reaches through their OWN statics — the one place naming a
/// vendor crate is the design rather than a leak.
fn shipping_vendors() -> [&'static BackendVendor; 3] {
    [
        &zeroship_migrate_postgres::VENDOR,
        &zeroship_migrate_sqlite::VENDOR,
        &zeroship_migrate_mysql::VENDOR,
    ]
}

const REGISTERED_VENDOR_FLOOR: usize = 3;

/// The total number of declared attributes the walk must actually SEE.
///
/// Without it, emptying every vocabulary turns each loop below into a no-op and the
/// whole file passes while measuring nothing. Measured at 12 on the tree that introduced
/// vendor attributes (PostgreSQL 5, MySQL 5, SQLite 2); the floor sits below that so
/// ordinary additions do not churn it, and above zero so deletions cannot hide.
const DECLARED_ATTRIBUTE_FLOOR: usize = 8;

/// Each of the three shipping backends declares at least one attribute today.
///
/// This is stated per-vendor rather than only in the total because the total alone
/// cannot tell "SQLite declares two" from "SQLite declares none and PostgreSQL gained
/// two". It is NOT a claim that every conceivable backend must declare attributes — a
/// future backend may legitimately declare an empty vocabulary — it is a claim about the
/// three named above, which is what makes it checkable at all.
const PER_VENDOR_ATTRIBUTE_FLOOR: usize = 1;

#[test]
fn a_backend_declares_attributes_only_in_its_own_namespace() {
    let vendors = shipping_vendors();
    assert!(
        vendors.len() >= REGISTERED_VENDOR_FLOOR,
        "the walk sees {} vendor(s), expected at least {REGISTERED_VENDOR_FLOOR}",
        vendors.len()
    );

    let mut checked = 0_usize;
    for vendor in vendors {
        let owner = vendor.descriptor.id.as_str();
        for def in vendor.attributes.iter() {
            assert_eq!(
                def.key.dialect(),
                owner,
                "the `{owner}` backend declares `{}`, which is `{}`'s to declare — a key \
                 in a foreign namespace is never matched, so this knob would silently do \
                 nothing on every target",
                def.key,
                def.key.dialect()
            );
            checked += 1;
        }
    }

    assert!(
        checked >= DECLARED_ATTRIBUTE_FLOOR,
        "the ownership walk examined {checked} attribute(s), expected at least \
         {DECLARED_ATTRIBUTE_FLOOR} — a walk over an empty set passes every assertion \
         inside it"
    );
}

#[test]
fn each_shipping_backend_declares_at_least_one_attribute() {
    for vendor in shipping_vendors() {
        let declared = vendor.attributes.len();
        assert!(
            declared >= PER_VENDOR_ATTRIBUTE_FLOOR,
            "the `{}` backend declares {declared} attribute(s), expected at least \
             {PER_VENDOR_ATTRIBUTE_FLOOR}",
            vendor.descriptor.id.as_str()
        );
    }
}

/// The prefix claim, checked rather than reasoned about: no key is declared twice, across
/// the whole workspace and not merely within one vendor.
#[test]
fn no_two_backends_declare_the_same_key_on_the_same_op() {
    // The (key, OP) pair, not the key. One knob is commonly legal on several ops —
    // PostgreSQL's `fillfactor` on `createTable`, `createPartition`, `setTableOptions`
    // AND `createIndex` — so deduping on the key alone would call a legitimate second
    // declaration a collision, which is how a census stops a correct change instead of a
    // wrong one.
    let mut seen: BTreeMap<(String, &'static str), &str> = BTreeMap::new();
    for vendor in shipping_vendors() {
        let owner = vendor.descriptor.id.as_str();
        for def in vendor.attributes.iter() {
            for op in def.ops {
                if let Some(previous) = seen.insert((def.key.to_string(), *op), owner) {
                    panic!(
                        "`{}` is declared on `{op}` by both `{previous}` and `{owner}`",
                        def.key
                    );
                }
            }
        }
    }
    assert!(
        seen.len() >= DECLARED_ATTRIBUTE_FLOOR,
        "only {} distinct (key, op) pair(s) seen, expected at least \
         {DECLARED_ATTRIBUTE_FLOOR}",
        seen.len()
    );
}

/// `docs` is not decoration. It is what an `Undeclared` refusal offers the user and what
/// becomes the doc comment on the generated TypeScript field, so an empty one ships a
/// knob nobody explained.
#[test]
fn every_declared_attribute_carries_documentation() {
    let mut checked = 0_usize;
    for vendor in shipping_vendors() {
        for def in vendor.attributes.iter() {
            assert!(
                !def.docs.trim().is_empty(),
                "`{}` is declared with no documentation",
                def.key
            );
            checked += 1;
        }
    }
    assert!(checked >= DECLARED_ATTRIBUTE_FLOOR, "walked {checked}");
}

/// Every op a vendor declares against must be a REAL op.
///
/// This is the check the op axis costs, and it is not optional. An op is named by a
/// STRING, so `"createTabel"` compiles, passes ownership, passes dedup — and matches
/// nothing, forever, because `check` looks the knob up by the op it was carried on. The
/// knob would be declared, exported, typed in TypeScript, and dead.
///
/// `dialect-support.toml` is the canonical list: one row per (op-kind, variant), already
/// checked in, already gated, and already the source the dialect table is generated from.
/// Using it rather than a hand-written list here means the two cannot disagree.
#[test]
fn every_declared_op_is_a_real_op() {
    let toml = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dialect-support.toml"),
    )
    .expect("dialect-support.toml is readable");

    let known: std::collections::BTreeSet<&str> = toml
        .lines()
        .filter_map(|l| l.strip_prefix("kind = \""))
        .filter_map(|l| l.split('"').next())
        .collect();

    // Floor: a parse that found nothing would make every assertion below vacuous, and the
    // op set is 56 — measured, not guessed.
    const OP_KIND_FLOOR: usize = 50;
    assert!(
        known.len() >= OP_KIND_FLOOR,
        "parsed {} op kind(s) from dialect-support.toml, expected at least \
         {OP_KIND_FLOOR} — the parse is broken and this census proves nothing",
        known.len()
    );

    let mut checked = 0_usize;
    for vendor in shipping_vendors() {
        for def in vendor.attributes.iter() {
            assert!(
                !def.ops.is_empty(),
                "`{}` is declared against NO op, so nothing can ever carry it",
                def.key
            );
            for op in def.ops {
                assert!(
                    known.contains(op),
                    "`{}` is declared on `{op}`, which is not an op kind. \
                     dialect-support.toml knows {} kinds and this is not one of them — a \
                     misspelled op declares a knob that matches nothing, forever.",
                    def.key,
                    known.len()
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked >= DECLARED_ATTRIBUTE_FLOOR,
        "walked {checked} (attribute, op) pair(s)"
    );
}
