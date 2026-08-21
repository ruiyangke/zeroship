//! The vendor-op surface answers PER VENDOR, through the contract.
//!
//! # What this replaces
//!
//! `zero_migrate::render::vendor` used to re-export `zero_migrate_postgres::render_vendor_op`,
//! and the engine called it BY NAME at three sites covering sixteen privileged op
//! kinds — roles, grants, RLS, policies, functions, extensions, schemas, `pgRaw` —
//! none of which touch `DmlRenderer`. `render/vendor.rs` recorded that honestly as
//! "the vendor-op surface is not behind the contract", and
//! `zero-migrate-sqlite/src/dml.rs` recorded the mirror image: "PostgreSQL is STILL
//! in the position SQLite just left, via `render::vendor`".
//!
//! It is behind the contract now: `DmlRenderer::render_vendor_op`, answered by each
//! vendor crate, reached by the engine through `render::backends`.
//!
//! # Why the assertion is a REFUSAL on two vendors and not three implementations
//!
//! MEASURED before it was designed, not reasoned from the shape:
//!
//! * `crates/zero-migrate-sqlite/src` and `crates/zero-migrate-mysql/src` contain no
//!   vendor-op renderer and never did — `dml`, `guard`, `lib`, `schema` (+ `collation`
//!   on MySQL) and nothing else.
//! * Every one of the sixteen op kinds is `dialect_scope = PgOnly`.
//! * The engine's lower seam refuses a non-PostgreSQL target BEFORE it renders, and it
//!   refuses on a CAPABILITY (`Capability::PostgresVendorPrimitives`), not on a dialect
//!   match.
//!
//! So the two other vendors have nothing to render and the honest answer is a refusal
//! they WRITE rather than one they inherit. The trait gives the method no default body
//! — the same rule the rest of `DmlRenderer` follows, and the same rule
//! `BackendVendor::guard` follows for the same reason: a fourth backend must ANSWER
//! the question, in its own crate, in its own diff, instead of acquiring an answer by
//! omitting something.
//!
//! # Why this test is not redundant with the census
//!
//! `core_names_no_vendor_crate.rs` proves the engine does not NAME
//! `zero_migrate_postgres`. It cannot prove the dispatch is real. A refactor that
//! routed all three vendors to PostgreSQL's renderer would satisfy the census
//! completely and be exactly the defect the crate split exists to prevent — the same
//! shape as the SQLite-identifiers-quoted-by-PostgreSQL bug this repo already had,
//! which compiled clean and passed every emitted-SQL assertion because the two vendors
//! agreed on the bytes. Here they do not agree: two of them have no answer at all, and
//! that disagreement is what makes it testable.

use zero_migrate::model::ir::Op;

/// One privileged op, used for every leg so the only variable is the vendor.
fn create_schema() -> Op {
    Op::CreateSchema {
        name: "analytics".to_string(),
        if_not_exists: None,
        authorization: None,
    }
}

/// PostgreSQL's REGISTERED renderer still renders the vendor ops, byte for byte.
///
/// The positive control. Reached through `BackendVendor::dml` — the same
/// `&'static dyn DmlRenderer` the engine's registry hands out — rather than through
/// `zero_migrate_postgres::render_vendor_op`, which is no longer reachable from
/// outside that crate at all (`mod vendor` is private and the `pub use` is gone, so
/// this is a privacy error rather than a convention).
#[test]
fn postgres_renders_a_vendor_op_through_the_contract() {
    let stmts = zero_migrate_postgres::VENDOR
        .dml
        .render_vendor_op(&create_schema(), "app")
        .expect("PostgreSQL owns the vendor-op surface");
    assert_eq!(stmts.len(), 1);
    assert!(
        stmts[0].up.contains("CREATE SCHEMA"),
        "the PostgreSQL spelling is unchanged by the routing: {}",
        stmts[0].up
    );
    assert!(
        stmts[0]
            .down
            .as_deref()
            .is_some_and(|d| d.contains("DROP SCHEMA")),
        "an unguarded create still owns its drop: {:?}",
        stmts[0].down
    );
}

/// SQLite REFUSES, in its own crate, in writing.
#[test]
fn sqlite_refuses_a_vendor_op() {
    let err = zero_migrate_sqlite::VENDOR
        .dml
        .render_vendor_op(&create_schema(), "app")
        .expect_err(
            "SQLite has no vendor-op renderer and must say so, not inherit \
             PostgreSQL's answer",
        );
    assert!(
        matches!(
            err,
            zero_migrate_backend::vendor::VendorError::VendorOpsUnsupported(_)
        ),
        "expected the vendor's own refusal, got: {err:?}"
    );
}

/// MySQL REFUSES, for the same reason and in the same shape.
#[test]
fn mysql_refuses_a_vendor_op() {
    let err = zero_migrate_mysql::VENDOR
        .dml
        .render_vendor_op(&create_schema(), "app")
        .expect_err("MySQL has no vendor-op renderer and must say so");
    assert!(
        matches!(
            err,
            zero_migrate_backend::vendor::VendorError::VendorOpsUnsupported(_)
        ),
        "expected the vendor's own refusal, got: {err:?}"
    );
}

/// The three answers are DISTINCT — the property a shared implementation would break.
///
/// Without this, three vendors all delegating to PostgreSQL would pass the two
/// positive assertions above (nothing checks that SQLite's `Ok` is absent) as long as
/// somebody also relaxed the refusal tests. Asserting the SHAPE of the disagreement —
/// exactly one vendor renders, exactly two refuse — is what a single wired-wrong
/// registry entry cannot satisfy.
#[test]
fn exactly_one_shipping_vendor_renders_the_vendor_ops() {
    let op = create_schema();
    let renders: Vec<&str> = [
        ("postgres", zero_migrate_postgres::VENDOR.dml),
        ("sqlite", zero_migrate_sqlite::VENDOR.dml),
        ("mysql", zero_migrate_mysql::VENDOR.dml),
    ]
    .into_iter()
    .filter(|(_, dml)| dml.render_vendor_op(&op, "app").is_ok())
    .map(|(name, _)| name)
    .collect();
    assert_eq!(
        renders,
        vec!["postgres"],
        "every vendor op is `dialect_scope = PgOnly`, so exactly one shipping vendor \
         may render one. A second entry here means a vendor acquired PostgreSQL's \
         renderer; an empty list means the surface was lost."
    );
}
