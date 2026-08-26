//! MySQL's guard answers, which are security decisions, held by tests in its own crate.
//!
//! `MysqlGuard` implements `MigrationGuard` in about a hundred lines, and every answer
//! is a deliberate posture choice whose consequence is written into its doc. None of
//! them was tested before this file: the crate's one test binary exercises the execution
//! half against the contract, and the shared suites reach this vendor through the
//! registry rather than asking the guard directly.
//!
//! # Why these particular answers
//!
//! Two of them turn a belt OFF when flipped, and would do it silently:
//!
//! - `refuses_destructive_ops_itself()` is the switch on
//!   `check_ir_data_security_policy`'s neutral posture walk, and that walk is the ONLY
//!   enforcement `data_security.destructive_ops = forbid` has on this backend - this
//!   guard is built without the composed policy and cannot read the knob. Answering
//!   `true` claims "I refuse destructive ops myself", the walk steps aside, and the knob
//!   goes inert. Its doc records that exact state as the one that once let a
//!   `DROP TABLE` through under the default `forbid`.
//! - `flags_for_sql()` returns `requires_approval: true` because there is no MySQL
//!   parser in this workspace, so no facet can be read out of an `up` blob. Returning
//!   `MigrationFlags::default()` would look like a tidy-up and would silently assert
//!   "not destructive, no approval needed" about text nobody inspected.
//!
//! The other two say this vendor has no raw door. A raw island is `Op::Raw` - PostgreSQL
//! text - so reaching MySQL with one is a mis-dispatch, not a trusted operation.
//! Returning `Ok` would grant MySQL an unchecked raw door that no MySQL author can even
//! open, and refusing under another vendor's id would blame the backend that produced
//! the text rather than the one that received it.
//!
//! # The vendor id is load-bearing here, not decoration
//!
//! This guard and SQLite's were once ONE type, a shared `SqliteDescriptorGuard` whose
//! own doc admitted it "serves BOTH descriptor-only engines - SQLite and MySQL - despite
//! the name". They were split so that a change to one vendor's posture could not
//! silently become a change to the other's. Asserting the id each door refuses under is
//! what keeps that split real: a body copied between the two crates without swapping the
//! constant fails here rather than passing quietly.

use zero_migrate_backend::guard::{GuardError, GuardOutcome, MigrationGuard};
use zero_migrate_ir::migration::MigrationFlags;
use zero_migrate_mysql::guard::MysqlGuard;
use zero_migrate_mysql::DIALECT;

/// A raw island is refused, and refused as MySQL's mis-dispatch.
#[test]
fn the_raw_island_doors_refuse_with_this_vendors_own_id() {
    let guard = MysqlGuard::new();

    let err = guard
        .check_raw_island_sql("DROP TABLE widgets")
        .expect_err("MySQL has no raw author, so an island here is a mis-dispatch");
    assert_eq!(
        err,
        GuardError::RawSqlRejected { dialect: DIALECT },
        "the raw-SQL door must refuse under MySQL's own id; `Ok` would grant an \
         unchecked raw door, and SQLite's id would mean this body was copied across \
         the split without being re-read"
    );

    let err = guard
        .check_raw_island_body("DELETE FROM widgets", "CREATE FUNCTION f() ...")
        .expect_err("a raw function body is PostgreSQL text on the wrong path");
    assert_eq!(
        err,
        GuardError::RawSqlRejected { dialect: DIALECT },
        "the raw-body door must refuse under MySQL's own id, for the same reason"
    );
}

/// The net-state walk is told there is no island to miss, not that islands are trusted.
#[test]
fn no_raw_island_can_escape_the_net_state_walk() {
    assert!(
        !MysqlGuard::new().raw_island_escapes_rls_net_state("DROP TABLE widgets"),
        "`false` here means MySQL HAS NO RAW DOOR — every op the walk sees is \
         structured. It must not become `true`, which would claim an island exists \
         whose net table state the walk cannot read."
    );
}

/// This guard does not claim to enforce the destructive knob, so the neutral walk runs.
///
/// This is the single most consequential answer in the file. It is a switch, and the
/// direction that fails open is `true`.
#[test]
fn the_destructive_belt_stays_on_because_this_guard_does_not_claim_it() {
    assert!(
        !MysqlGuard::new().refuses_destructive_ops_itself(),
        "answering `true` TURNS OFF `check_ir_data_security_policy`'s posture walk, \
         which is the only enforcement `data_security.destructive_ops = forbid` has on \
         MySQL — this guard is built without the composed policy and cannot read the \
         knob at all. Flipping this makes the knob silently inert."
    );
}

/// Text this vendor cannot parse is gated on a human, not waved through as clean.
#[test]
fn unclassifiable_sql_is_gated_on_approval_rather_than_assumed_safe() {
    let flags = MysqlGuard::new()
        .flags_for_sql("DROP TABLE widgets")
        .expect("classifying text conservatively cannot itself fail");

    assert!(
        flags.requires_approval,
        "there is no MySQL parser in this workspace, so no facet can be read out of an \
         `up` blob. The conservative answer is approval. Returning the default flag set \
         would assert `not destructive, no approval needed` about text nobody inspected."
    );
    assert_eq!(
        flags,
        MigrationFlags {
            requires_approval: true,
            ..MigrationFlags::default()
        },
        "approval is the ONLY facet this vendor may assert about unparsed text; \
         claiming any other one would be reading something it cannot read"
    );
}

/// The descriptor path is trusted at line 1, and that grant is bounded.
#[test]
fn the_descriptor_path_is_granted_a_clean_outcome_and_nothing_more() {
    let outcome = MysqlGuard::new()
        .check("CREATE TABLE widgets (id INT PRIMARY KEY)")
        .expect("the descriptor path is trusted at line 1 and cannot deny");

    assert_eq!(
        outcome,
        GuardOutcome::default(),
        "MySQL's line-1 grant is the EMPTY outcome: no denial, and no claim about \
         destructiveness either. Destructive/approval facets come from the migration's \
         own author flags via `plan()`, and the data-security posture comes from the \
         neutral walk over the structured IR — not from here."
    );
    assert!(
        !outcome.destructive,
        "an empty outcome must not assert destructiveness it never inspected"
    );
    assert!(
        outcome.advisories.is_empty(),
        "the descriptor path emits no advisories"
    );
}

/// The factory hands back this vendor's own guard, and ignores the config by design.
///
/// Without this, the four answers above could all be correct on `MysqlGuard` while the
/// registry handed the engine something else entirely.
#[test]
fn the_registry_factory_yields_a_guard_that_gives_these_same_answers() {
    let registry = zero_migrate_ir::policy_registry::builtin_registry();
    let cfg = zero_migrate_backend::guard::GuardConfig::from_policy(
        zero_migrate_policy::EffectivePolicy::deny_all(&registry),
        DIALECT,
    );
    let guard = zero_migrate_mysql::guard::guard(&cfg);

    assert!(
        !guard.refuses_destructive_ops_itself(),
        "the guard the registry builds must keep the neutral walk on, whatever config \
         it was handed — this one was handed `deny_all`"
    );
    assert_eq!(
        guard
            .check_raw_island_sql("DROP TABLE widgets")
            .expect_err("the registry's guard must refuse a raw island too"),
        GuardError::RawSqlRejected { dialect: DIALECT },
    );
}
