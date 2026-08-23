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
//! Nothing. The list this section carried held one entry —
//! `zero_migrate::render::value_format`'s `catalog_id_default`,
//! `catalog_text_id_default`, `catalog_uuid_id_default`, `recover_format_check` and
//! `RecoveredFormatCheck`, all read by `drift_sql.rs` — and it came down exactly the
//! way the entry predicted: each took a `&DialectId` and resolved a renderer out of
//! the engine's registry, and each takes the renderers directly now.
//!
//! Add a line here if a new one appears, and take it off in the commit that moves it.

use std::time::Duration;

use zero_migrate_backend::backend::{PROJECT_LOCK_TRY_ATTEMPTS, PROJECT_LOCK_TRY_BACKOFF};
use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::constraint_definition::{constraintdef_cols, fk_constraint_snapshot};
use zero_migrate_backend::drift::{compare_applied_to_set, ChecksumDriftReport};
use zero_migrate_backend::executor::{authorize_existence_guard_schema, ApplyError};
use zero_migrate_backend::existence_probe::{decide, GuardVerdict};
use zero_migrate_backend::fault;
use zero_migrate_backend::journal::AppliedEntry;
use zero_migrate_backend::snapshot::{IdDefaultSnapshot, SchemaSnapshot};
use zero_migrate_backend::value_format::{
    catalog_id_default, column_metadata, recover_format_check, RecoveredFormatCheck,
};
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::ir::ValueFormat;
use zero_migrate_ir::migration::Migration;
use zero_migrate_ir::probe::{GuardDir, GuardProbe};

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

/// The existence-guard DECIDER, reached with MySQL's own vendor rather than by
/// asking the registry which backend handles MySQL.
///
/// That distinction is the whole reason `decide` takes a `&BackendVendor` now: this
/// crate knows which vendor it is, and `registry_resolution_stays_core_only` reads
/// this crate. The case exercised is the one that must never depend on a vendor at
/// all — an `IfNotExists` table probe against an EMPTY live catalog runs bare — so a
/// `decide` wired to nothing would still have to answer it correctly.
#[test]
fn the_existence_guard_decider_answers_for_this_vendor() {
    let probe = GuardProbe::Table {
        schema: "app".to_string(),
        table: "orders".to_string(),
        direction: GuardDir::IfNotExists,
        expect_columns: Vec::new(),
    };
    assert_eq!(
        decide(
            &probe,
            &SchemaSnapshot::default(),
            &zero_migrate_mysql::VENDOR
        ),
        GuardVerdict::RunBare,
        "an ifNotExists probe for a table absent from the live catalog did not run \
         bare, so a guarded MySQL createTable would be skipped or fail closed on an \
         empty database"
    );
}

/// The canonical constraint-`definition` codec, read by `drift_sql.rs` for every
/// PRIMARY KEY and FOREIGN KEY it lifts out of `information_schema`.
///
/// MySQL's catalog stores no rendered constraint body — there is no
/// `pg_get_constraintdef` there — so the drift path BUILDS the desired body itself
/// and must build the byte-identical one the engine's lower and fold build, or every
/// introspected key phantom-diffs against the snapshot it is compared to.
///
/// Reachability, plus the two bytes that would silently differ if either half were
/// wired to nothing: the CONDITIONAL quote, and an FK action folded by THIS vendor.
#[test]
fn the_constraint_definition_codec_is_reachable_and_spells_the_comparison_form() {
    let _: fn(&[String]) -> String = constraintdef_cols;

    // Conditional, not unconditional. A safe lowercase column stays BARE because
    // that is how a catalog renders it; a reserved word is quoted. Getting this
    // uniformly wrong in either direction phantom-diffs every key on the table.
    assert_eq!(
        constraintdef_cols(&["handle".to_string(), "order".to_string()]),
        r#"handle, "order""#,
        "the constraint-definition column list is not spelling the conditional \
         comparison form, so every MySQL key lifted from information_schema would \
         re-diff against a body the engine's lower never produces"
    );

    // The FK body, built with MySQL's OWN vendor rather than by asking the registry
    // which backend handles MySQL — the distinction `registry_resolution_stays_core_only`
    // reads this crate for.
    //
    // `RESTRICT` is the discriminator: InnoDB has no deferred checks, so MySQL folds
    // it into the omitted `NO ACTION` default, while PostgreSQL preserves it and
    // would render ` ON DELETE RESTRICT` here. A `fk_constraint_snapshot` that
    // ignored its vendor argument would emit the PostgreSQL body and still compile.
    let fk = fk_constraint_snapshot(
        "orders_author_id_fkey".to_string(),
        "app",
        &["author_id".to_string()],
        "authors",
        &["id".to_string()],
        Some("restrict"),
        None,
        false,
        false,
        false,
        &zero_migrate_mysql::VENDOR,
    );
    assert_eq!(
        fk.kind, "FOREIGN KEY",
        "the FK constraint snapshot did not come back as a FOREIGN KEY"
    );
    assert_eq!(
        fk.definition, "FOREIGN KEY (author_id) REFERENCES app.authors(id)",
        "the FK body is not MySQL's canonical form: InnoDB folds RESTRICT into the \
         omitted NO ACTION default, so a rendered ` ON DELETE RESTRICT` here means \
         the vendor argument was ignored and PostgreSQL's spelling was used"
    );
}

/// The catalog value-format comparison `drift_sql.rs` reads every MySQL column
/// default and format `CHECK` through, reached with THIS vendor's renderers rather
/// than by asking the registry which backend handles MySQL.
///
/// Reachability, plus the two answers that would be wrong if either renderer
/// argument were ignored, one per renderer:
///
///   * MySQL is the one shipping vendor whose `information_schema` reports a literal
///     default WITHOUT SQL quotes and marks the expression/literal distinction out of
///     band, so a bare `abc` under the literal marker is a LITERAL here and an
///     expression under any other vendor's `ValueFormatRenderer`; and
///   * the UUIDv4 catalog form below is what MySQL's `DmlRenderer` emits and what its
///     catalog echoes back, `_latin1` introducers and all. Recognizing it needs BOTH
///     halves — the DML renderer to render the generator to compare against, and the
///     value-format renderer to strip the introducers — so a call wired to another
///     vendor's pair reports a plain expression and every UUID-defaulted MySQL column
///     drifts on the first introspection.
#[test]
fn the_catalog_id_default_comparison_answers_with_this_vendors_renderers() {
    /// MySQL's UUIDv4 default exactly as `information_schema` echoes it back.
    const MYSQL_CATALOG_UUID_V4_DEFAULT: &str = "lower(concat(hex(random_bytes(4)),_latin1'-',hex(random_bytes(2)),_latin1'-',hex(((ord(random_bytes(1)) & 15) | 64)),hex(random_bytes(1)),_latin1'-',hex(((ord(random_bytes(1)) & 63) | 128)),hex(random_bytes(1)),_latin1'-',hex(random_bytes(6))))";

    let vendor = &zero_migrate_mysql::VENDOR;

    assert_eq!(
        catalog_id_default(Some("abc"), vendor.value_format, vendor.dml, Some(false)),
        IdDefaultSnapshot::Literal("\"abc\"".to_string()),
        "MySQL reports an unquoted COLUMN_DEFAULT plus an out-of-band literal \
         marker; reading that as anything but a literal makes every defaulted MySQL \
         column drift on the first introspection"
    );

    assert_eq!(
        catalog_id_default(
            Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
            vendor.value_format,
            vendor.dml,
            Some(true)
        ),
        IdDefaultSnapshot::UuidV4,
        "MySQL's own UUIDv4 catalog form was not recognized as the UUIDv4 default, so \
         at least one of the two renderers handed in was not this vendor's"
    );
}

/// The format-`CHECK` recovery `drift_sql.rs` runs over every catalog CHECK clause,
/// reached with this vendor's renderers.
///
/// The discriminator is the MySQL-only charset introducer: its catalog echoes a
/// CHECK back with `_utf8mb4'…'` in front of every string literal, and only MySQL's
/// own normalization strips it. Recovery therefore fails on the exact clause MySQL
/// stores unless the renderers handed in are MySQL's.
#[test]
fn the_format_check_recovery_answers_with_this_vendors_renderers() {
    let vendor = &zero_migrate_mysql::VENDOR;
    let authored = column_metadata(
        "public_id",
        &ValueFormat::TypeId {
            prefix: "user".to_string(),
        },
        vendor.value_format,
        vendor.dml,
    )
    .expect("a valid TypeID prefix lowers to column metadata");

    assert_eq!(
        recover_format_check(
            "public_id",
            &authored.inline_check,
            vendor.value_format,
            vendor.dml
        ),
        Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
            prefix: "user".to_string(),
        })),
        "this vendor's own rendered format CHECK did not recover to the format it \
         renders, so the recovery and the renderer disagree and every TypeID column \
         would drift against itself"
    );

    assert_eq!(
        recover_format_check(
            "public_id",
            "CHECK (public_id IS NOT NULL)",
            vendor.value_format,
            vendor.dml
        ),
        None,
        "an unrelated CHECK was recovered as a format contract, so recovery is not \
         comparing the whole clause"
    );
}
