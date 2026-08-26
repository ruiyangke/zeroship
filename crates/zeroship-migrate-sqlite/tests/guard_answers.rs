//! SQLite's guard answers, which are security decisions, held by tests in its own crate.
//!
//! `SqliteGuard` implements `MigrationGuard` in about a hundred lines and every one of
//! its answers is a deliberate posture choice with a consequence written into its doc.
//! Until this file existed, none of those answers was tested anywhere: the crate had no
//! `tests/` directory at all, and the shared suites reach this vendor through the
//! registry rather than asking the guard directly.
//!
//! # Why these particular answers
//!
//! Two of them turn a belt OFF when flipped, and would do it silently:
//!
//! - `refuses_destructive_ops_itself()` is the switch on
//!   `check_ir_data_security_policy`'s neutral posture walk. That walk is the ONLY
//!   enforcement `data_security.destructive_ops = forbid` has on this backend, because
//!   this guard is handed no policy and cannot read the knob. Answering `true` would
//!   assert "I refuse destructive ops myself", the walk would step aside, and the knob
//!   would become inert - which its doc records as the exact state that once let a
//!   `DROP TABLE` through under the default `forbid`.
//! - `flags_for_sql()` returns `requires_approval: true` because no SQLite parser exists
//!   here, so no facet can be read out of an `up` blob. Returning
//!   `MigrationFlags::default()` instead would look like a tidy-up and would silently
//!   assert "not destructive, no approval needed" about text nobody inspected.
//!
//! The other two say this vendor has no raw door. `check_raw_island_sql` and
//! `check_raw_island_body` refuse rather than returning `Ok`, and they refuse with
//! SQLite's own dialect id. `Ok` would grant an unchecked raw door that no SQLite author
//! can even open, and the wrong id would report the mis-dispatch against the vendor that
//! produced the text rather than the one that received it.
//!
//! # On the two raw-island methods specifically
//!
//! Their contract doc records that nothing on the lowering path calls them today: they
//! were reached only for a config that skipped the deny-list belt, and that posture
//! (Trusted) is gone. They are kept as each vendor's declared answer to "what survives a
//! belt-skip", so that reinstating an unconfined posture cannot hand it an unchecked raw
//! door. That rationale rests on the answers being written AND checked. They were written
//! here and checked only for PostgreSQL, whose `guard_smoke.rs` pins its own. This file
//! is the missing half for SQLite.

use zero_migrate_backend::guard::{GuardError, GuardOutcome, MigrationGuard};
use zero_migrate_ir::migration::MigrationFlags;
use zero_migrate_sqlite::guard::SqliteGuard;
use zero_migrate_sqlite::DIALECT;

/// A raw island is refused, and refused as SQLite's mis-dispatch.
#[test]
fn the_raw_island_doors_refuse_with_this_vendors_own_id() {
    let guard = SqliteGuard::new();

    let err = guard
        .check_raw_island_sql("DROP TABLE widgets")
        .expect_err("SQLite has no raw author, so an island here is a mis-dispatch");
    assert_eq!(
        err,
        GuardError::RawSqlRejected { dialect: DIALECT },
        "the raw-SQL door must refuse under SQLite's own id; `Ok` would grant an \
         unchecked raw door, and another vendor's id would blame the wrong backend"
    );

    let err = guard
        .check_raw_island_body("DELETE FROM widgets", "CREATE FUNCTION f() ...")
        .expect_err("a raw function body is PostgreSQL text on the wrong path");
    assert_eq!(
        err,
        GuardError::RawSqlRejected { dialect: DIALECT },
        "the raw-body door must refuse under SQLite's own id, for the same reason"
    );
}

/// The net-state walk is told there is no island to miss, not that islands are trusted.
#[test]
fn no_raw_island_can_escape_the_net_state_walk() {
    assert!(
        !SqliteGuard::new().raw_island_escapes_rls_net_state("DROP TABLE widgets"),
        "`false` here means SQLite HAS NO RAW DOOR — every op the walk sees is \
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
        !SqliteGuard::new().refuses_destructive_ops_itself(),
        "answering `true` TURNS OFF `check_ir_data_security_policy`'s posture walk, \
         which is the only enforcement `data_security.destructive_ops = forbid` has on \
         SQLite — this guard is built without the composed policy and cannot read the \
         knob at all. Flipping this makes the knob silently inert."
    );
}

/// Text this vendor cannot parse is gated on a human, not waved through as clean.
#[test]
fn unclassifiable_sql_is_gated_on_approval_rather_than_assumed_safe() {
    let flags = SqliteGuard::new()
        .flags_for_sql("DROP TABLE widgets")
        .expect("classifying text conservatively cannot itself fail");

    assert!(
        flags.requires_approval,
        "there is no SQLite parser here, so no facet can be read out of an `up` blob. \
         The conservative answer is approval. Returning the default flag set would \
         assert `not destructive, no approval needed` about text nobody inspected."
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

/// The descriptor-diff path is trusted at line 1, and that grant is bounded.
#[test]
fn the_descriptor_path_is_granted_a_clean_outcome_and_nothing_more() {
    let outcome = SqliteGuard::new()
        .check("CREATE TABLE widgets (id INTEGER PRIMARY KEY)")
        .expect("the descriptor path is trusted at line 1 and cannot deny");

    assert_eq!(
        outcome,
        GuardOutcome::default(),
        "SQLite's line-1 grant is the EMPTY outcome: no denial, and no claim about \
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
/// Without this, the four answers above could all be correct on `SqliteGuard` while the
/// registry handed the engine something else entirely.
#[test]
fn the_registry_factory_yields_a_guard_that_gives_these_same_answers() {
    let registry = zero_migrate_ir::policy_registry::builtin_registry();
    let cfg = zero_migrate_backend::guard::GuardConfig::from_policy(
        zero_migrate_policy::EffectivePolicy::deny_all(&registry),
        DIALECT,
    );
    let guard = zero_migrate_sqlite::guard::guard(&cfg);

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
