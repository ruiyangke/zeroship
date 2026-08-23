//! What the MySQL EXECUTION half must be able to name from here, written BEFORE
//! the code that needs it arrives.
//!
//! # The invariant
//!
//! **Everything `MysqlBackend` touches on the apply path is reachable from a vendor
//! crate.** Not "is neutral" — REACHABLE. A vendor crate cannot depend on the engine
//! (the engine depends on all three vendors, so the edge back is a cycle Cargo
//! refuses), so an apply-path item still sitting in `zero-migrate` is an item
//! MySQL's executor will not be able to call once it lives here.
//!
//! # Why this is a compile-time assertion and not a behaviour test
//!
//! `crates/zero-migrate/src/apply/backend/mysql/` — eight files, 16,262 lines, of
//! which 8,490 are production — is being extracted into this crate. The extraction
//! is blocked by exactly one thing: the items its production half reaches through
//! `crate::…` that live in the engine rather than in `zero-migrate-backend`.
//!
//! Naming them here makes those blockers a COMPILER question instead of a reading
//! exercise. A name that does not compile is an item that has not come down yet; a
//! name that STOPS compiling later is an item somebody pushed back up into the
//! engine, which would strand the extracted backend. Neither failure is visible to
//! any behaviour test, because behaviour tests run in a crate that CAN see the
//! engine.
//!
//! It is a `tests/` file rather than a `src/` one on purpose: an integration test
//! links this crate from outside, so it proves the items are reachable through the
//! same public surface the executor will use and not through an in-crate shortcut.
//!
//! # Reachability, and where a cheap behavioural check is available
//!
//! The subject is reachability, so the minimum every item gets is being NAMED at a
//! declared type — for a function, a `fn` pointer coercion, which fails to compile
//! if the item is absent, private, or has a different signature. Where an item can
//! also be exercised without building an `EffectivePolicy` or a `Migration` (neither
//! of which this crate can cheaply construct), it is, because a reachable item that
//! silently does nothing is the other way to be green for the wrong reason.
//!
//! # Scope, said out loud
//!
//! This file does not assert NEUTRALITY. That an item can be named here says nothing
//! about whether it should have moved — `registry_resolution_stays_core_only` is what
//! answers that, and it reads this crate too. The two are complements: this one fails
//! when the contract is too SMALL, that one fails when a vendor reaches for something
//! it should have answered for itself.
//!
//! # What is deliberately NOT here yet
//!
//! The blockers still outstanding, written down rather than left to be rediscovered.
//! Each resolves a vendor from a `DialectId` today, so each needs a
//! `&'static BackendVendor` (or a `&dyn` renderer) parameter before it can come down:
//!
//! * `zero_migrate::render::value_format` — `catalog_id_default`,
//!   `catalog_text_id_default`, `catalog_uuid_id_default`, `recover_format_check`
//!   and `RecoveredFormatCheck`, read by `drift_sql.rs`.
//! * `zero_migrate::render::existence_probe` — `decide` and `GuardVerdict`, read by
//!   `session.rs`.
//! * `zero_migrate::render::declarative` — `constraintdef_cols` and
//!   `ir_fk_constraint_snapshot_for_columns`, read by `drift_sql.rs`.
//!
//! Add each one's line here in the commit that moves it.

use std::time::Duration;

use zero_migrate_backend::backend::{PROJECT_LOCK_TRY_ATTEMPTS, PROJECT_LOCK_TRY_BACKOFF};
use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::drift::{compare_applied_to_set, ChecksumDriftReport};
use zero_migrate_backend::executor::{authorize_existence_guard_schema, ApplyError};
use zero_migrate_backend::fault;
use zero_migrate_backend::journal::AppliedEntry;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::migration::Migration;

/// The crash-simulation seam the two-phase MySQL apply trips at.
///
/// `session.rs` trips `DML_AFTER_STMT_BEFORE_JOURNAL` and
/// `DML_AFTER_JOURNAL_BEFORE_COMMIT`; `backfill_sql.rs` trips
/// `BACKFILL_MID_BATCHES`. Reachability plus the one behaviour that matters: the
/// seam is inert until armed, and it does fire when it is.
#[test]
fn the_crash_seam_is_reachable_and_inert_until_armed() {
    for point in [
        fault::points::DML_AFTER_STMT_BEFORE_JOURNAL,
        fault::points::DML_AFTER_JOURNAL_BEFORE_COMMIT,
        fault::points::BACKFILL_MID_BATCHES,
        fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED,
    ] {
        assert!(
            fault::trip(point).is_ok(),
            "an UNARMED fault point fired at {point}, so the seam's inert default is \
             broken and every production apply would abort"
        );
    }

    // Armed on THIS thread it fires once, then disarms itself. Proven rather than
    // assumed: `trip`'s fast path is a single relaxed load, so a seam wired to
    // nothing would pass the loop above unchanged.
    fault::arm(fault::points::BACKFILL_MID_BATCHES, 0);
    assert!(
        fault::trip(fault::points::BACKFILL_MID_BATCHES).is_err(),
        "an ARMED fault point did not fire, so the crash seam is a no-op and the \
         MySQL backfill's mid-batch resume has no crash coverage at all"
    );
    assert!(
        fault::trip(fault::points::BACKFILL_MID_BATCHES).is_ok(),
        "the armed fault fired twice; it is one-shot by contract"
    );
    fault::disarm_all();
}

/// The lock-retry budget every backend's non-blocking project-lock acquisition
/// reads. Shared so the three cannot drift apart; `MysqlBackend` reads both.
#[test]
fn the_project_lock_retry_budget_is_reachable() {
    // A `const` block, because the value is a `const` and the check can therefore
    // fail at COMPILE time rather than on a test run.
    const {
        assert!(
            PROJECT_LOCK_TRY_ATTEMPTS > 0,
            "a zero-attempt budget makes every non-blocking acquisition report the \
             lock busy without ever asking for it"
        );
    }
    assert!(
        PROJECT_LOCK_TRY_BACKOFF > Duration::ZERO,
        "a zero backoff turns the bounded retry into a spin"
    );
}

/// The dialect-agnostic checksum/tamper comparison. MySQL supplies the journal read;
/// the rules over it are shared, which is what keeps the tamper verdict from
/// differing per dialect.
#[test]
fn the_checksum_drift_comparison_is_reachable() {
    let _: fn(&[AppliedEntry], &[Migration]) -> ChecksumDriftReport = compare_applied_to_set;
    assert!(
        compare_applied_to_set(&[], &[]).is_clean(),
        "an empty journal compared against an empty migration set reported drift, so \
         the shared comparison does not mean what every backend assumes it means"
    );
}

/// The existence-guard catalog-read authorization, which MySQL's session path calls
/// before reading a schema a guard named.
///
/// Named at its type rather than exercised: it takes an `ExecutorConfig`, and this
/// crate has no cheap way to compose an `EffectivePolicy` — the charter parser is in
/// the engine. The coercion still fails if the item is absent, private, or has
/// drifted, which is this file's subject. What it DOES when the schema is out of
/// scope stays covered where a policy is cheap: the engine's existence-guard suite.
#[test]
fn the_existence_guard_authorization_is_reachable() {
    let _: fn(&ExecutorConfig, &str, &str, &DialectId) -> Result<(), ApplyError> =
        authorize_existence_guard_schema;
}
