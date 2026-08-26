//! Every registered backend states its own operational-advisory posture, and a
//! backend that has none cannot report as clean.
//!
//! # The defect this pins, which was live in this tree
//!
//! `DeclarativePlan::advisories` called the `libpg_query` analyzers directly, on
//! every dialect. MySQL renders identifiers with backticks and SQLite is not
//! PostgreSQL at all, so on those two backends every statement failed to parse, the
//! analyzers returned an empty vector, and the plan's advisory report came back
//! EMPTY — for SQL this engine emits and is about to run. "Nobody read any of this"
//! and "read all of it, found nothing" were the same value.
//!
//! The same defect had already been found and fixed ONCE, one layer up, in
//! `zero-migrate-node`'s `advisoriesFor` — as an `if dialect != POSTGRES` written in
//! the host addon, deciding on the backends' behalf and spelling out the reason in a
//! sentence that lived nowhere near the backends it was about. It was not fixed in
//! the differ, because nothing connected the two sites. This file is that
//! connection: the posture is now a required `BackendVendor` field, so both
//! consumers ask the same vendor and get the same answer.
//!
//! # Why this is a test and not a type-system guarantee
//!
//! Most of it IS a type-system guarantee. `BackendVendor::advisor` is required
//! (E0063), neither `OperationalAdvisor` method has a default body (E0046), and
//! `AdvisoryVerdict` has no `From<Vec<Advisory>>`, so a backend cannot acquire the
//! not-analyzed posture by omission and a caller cannot flatten a verdict without
//! meeting the absence. `crates/zero-migrate-backend/src/registry.rs` carries the
//! `compile_fail` doctests for those.
//!
//! What the compiler cannot check is CONSISTENCY: a vendor whose
//! `analyzer_absence` says "I analyze" while its `advise` returns `NotAnalyzed`
//! compiles fine and reports a set as checked while reporting every statement in it
//! as unchecked. That is what the census below is for.

use zeroship_migrate::{
    advisories_for_sql, analyzer_absence, shipping_backends, AdvisoryVerdict, Checksum,
    DeclarativePlan, Migration, MigrationFlags, MigrationId,
};
use zeroship_migrate_backend::advisory::rule;
use zeroship_migrate_ir::dialect::DialectId;

/// The census floor. A scan over a DISCOVERED set fails OPEN: narrow the discovery
/// and it iterates nothing, finds nothing, and reports clean. Raise it when a
/// backend is added; lower it only in the commit that removes one, and say which.
const REGISTERED_BACKEND_FLOOR: usize = 3;

/// The NEEDLE-LIVENESS floors, and they are two because there are two ways for this
/// census to pass while measuring nothing.
///
/// If EVERY registered backend refused to analyze, the consistency check below would
/// hold vacuously and the "unchecked is not clean" check would never see a real
/// advisory to contrast against. If every backend analyzed, the absence path — the
/// entire point of the seam — would never execute.
///
/// So both arms must be occupied. Measured on the tree that introduced the seam:
/// PostgreSQL analyzes, MySQL and SQLite do not.
const BACKENDS_THAT_ANALYZE_FLOOR: usize = 1;
const BACKENDS_WITHOUT_AN_ANALYZER_FLOOR: usize = 2;

/// A migration carrying one statement, enough to ask a backend about.
fn migration(up: &str) -> Migration {
    let flags = MigrationFlags::default();
    Migration {
        version: MigrationId::generate(),
        name: "advisory_probe".into(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_acme",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: "app_acme".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

fn plan_for(dialect: &DialectId, up: &str) -> DeclarativePlan {
    DeclarativePlan {
        migrations: vec![migration(up)],
        renames: Vec::new(),
        rebuilds: Vec::new(),
        accepted_index_aliases: Vec::new(),
        created_tables: Vec::new(),
        dialect: dialect.clone(),
    }
}

/// Every registered backend answers both halves of the contract, and answers them
/// the same way.
#[test]
fn a_backends_two_advisory_answers_cannot_disagree() {
    let registry = shipping_backends();
    assert!(
        registry.len() >= REGISTERED_BACKEND_FLOOR,
        "the registry holds {} backend(s), expected at least {REGISTERED_BACKEND_FLOOR} — \
         a census over a discovered set that iterates nothing reports clean",
        registry.len()
    );

    let mut analyzing = 0usize;
    let mut refusing = 0usize;

    for descriptor in registry.iter() {
        let dialect = &descriptor.id;
        // A statement every dialect can be ASKED about. Whether it parses is the
        // backend's business; that it is asked is this test's.
        let verdict = advisories_for_sql(zeroship_migrate::shipping_vendors(), dialect, "DROP TABLE t");
        match (
            analyzer_absence(zeroship_migrate::shipping_vendors(), dialect),
            verdict,
        ) {
            (None, AdvisoryVerdict::Analyzed(_)) => analyzing += 1,
            (Some(by_absence), AdvisoryVerdict::NotAnalyzed(by_advise)) => {
                assert_eq!(
                    by_absence, by_advise,
                    "{dialect} states its absent analyzer differently through \
                     analyzer_absence than through advise"
                );
                assert_eq!(
                    &by_absence.dialect, dialect,
                    "{dialect}'s absence must name {dialect}, not another backend"
                );
                refusing += 1;
            }
            (absence, verdict) => panic!(
                "{dialect} disagrees with itself: analyzer_absence said {absence:?} \
                 while advise said {verdict:?}"
            ),
        }
    }

    assert!(
        analyzing >= BACKENDS_THAT_ANALYZE_FLOOR,
        "no registered backend analyzes anything: {analyzing} of {} — this census \
         would pass vacuously",
        registry.len()
    );
    assert!(
        refusing >= BACKENDS_WITHOUT_AN_ANALYZER_FLOOR,
        "no registered backend exercises the absence path: {refusing} of {} — the \
         seam's whole purpose would be untested",
        registry.len()
    );
}

/// A backend with no analyzer produces a NON-EMPTY report, and it says UNCHECKED.
///
/// This is the assertion the old direct call could not have passed. An empty
/// `Vec<Advisory>` was indistinguishable from a clean one, so there was no value
/// this test could have looked at.
#[test]
fn an_unchecked_backend_never_reports_an_empty_advisory_list() {
    for descriptor in shipping_backends().iter() {
        let dialect = &descriptor.id;
        let Some(absent) = analyzer_absence(zeroship_migrate::shipping_vendors(), dialect) else {
            continue;
        };

        let report = advisories_for_sql(zeroship_migrate::shipping_vendors(), dialect, "DROP TABLE t")
            .into_report();
        assert!(
            !report.is_empty(),
            "{dialect} has no analyzer, so its report must SAY so rather than be empty"
        );
        assert!(
            report
                .iter()
                .any(|a| a.rule == rule::ANALYZER_DIALECT_UNSUPPORTED),
            "{dialect}'s report must carry {}, got {report:?}",
            rule::ANALYZER_DIALECT_UNSUPPORTED
        );
        assert!(
            absent.advisory().message.contains(dialect.as_str()),
            "{dialect}'s notice must name the backend it is about"
        );
    }
}

/// The differ's plan seam routes through the registry, so the same property holds
/// where the defect actually lived.
#[test]
fn a_plan_on_a_backend_without_an_analyzer_reports_the_absence() {
    for descriptor in shipping_backends().iter() {
        let dialect = &descriptor.id;
        let plan = plan_for(dialect, "DROP TABLE t");
        let advisories = plan.advisories(zeroship_migrate::shipping_vendors());

        if analyzer_absence(zeroship_migrate::shipping_vendors(), dialect).is_some() {
            assert_eq!(
                advisories.len(),
                1,
                "{dialect}: every migration in the plan must carry the absence notice"
            );
            assert!(
                advisories[0]
                    .1
                    .iter()
                    .any(|a| a.rule == rule::ANALYZER_DIALECT_UNSUPPORTED),
                "{dialect}: the plan's advisories must say the plan was not analyzed, \
                 got {:?}",
                advisories[0].1
            );
        } else {
            // The positive control. A backend that DOES analyze must still find the
            // real footgun, or the seam has quietly disabled the analyzers rather
            // than routed them.
            assert!(
                advisories
                    .iter()
                    .any(|(_, a)| a.iter().any(|adv| adv.rule == rule::DESTRUCTIVE_DROP)),
                "{dialect}: a DROP TABLE must still raise DESTRUCTIVE_DROP, got \
                 {advisories:?}"
            );
        }
    }
}
