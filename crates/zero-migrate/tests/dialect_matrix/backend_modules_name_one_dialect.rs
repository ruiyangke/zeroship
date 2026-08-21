//! The one-dialect-literal rule of the backend module directories, enforced rather
//! than documented.
//!
//! There are THREE renderer module families: DML, schema typing, and DDL emission.
//! All are covered here. The schema family came later
//! — `SchemaRenderer`'s three vendors lived as bare structs and `impl` blocks in
//! the middle of `schema/query.rs` long after the DML vendors had modules — and it
//! is guarded from its first commit precisely because the rule is invisible to
//! behaviour tests, so an unguarded module drifts silently.
//!
//! `backends/mod.rs` states the rule in prose: a backend module names its own
//! dialect exactly ONCE, as its `DIALECT` const, and names no other dialect at all.
//! Everything else in the module reads `DIALECT`. That is what makes the crate
//! extraction of `docs/proposals/pluggable-backends.md` step 4 mechanical - deleting
//! the const is the only edit each module needs, because nothing else in it can
//! observe which vendor it is.
//!
//! The rule is invisible to every behaviour test. A `backends/postgres.rs` that
//! reaches for `SqlDialect::Mysql` still emits correct PostgreSQL today; what it
//! costs is the extraction, and no assertion about emitted SQL can see that. So the
//! rule gets its own check or it has none, which is what it had.
//!
//! WHY THIS FILE AND NOT A `#[cfg(test)] mod tests` IN `backends/mod.rs`. The check
//! reads the nine modules as TEXT, which a unit test could do with `include_str!`
//! just as well. It lives out here because it is a fact about the two layers'
//! SHAPE rather than about their behaviour, and because this theme binary is where
//! the other "what does each dialect declare" checks already are. It still reads the
//! real files, so it tracks them: `include_str!` is a compile-time dependency, and
//! editing any of the nine rebuilds this binary.

/// The rule, as a test: one dialect literal per backend module, its own, and it is
/// the `DIALECT` const.
///
/// The second clause is not decoration. A bare occurrence COUNT would stay at one if
/// the const were deleted and some other line in the module reached for the same
/// variant - which is the shape the rule actually forbids, since the point is that
/// exactly one line in the module knows the vendor and everything else reads it.
///
/// # What this does NOT catch, and it is the important half
///
/// This sees EXPLICIT coupling only - a foreign `SqlDialect::` literal written
/// inside a backend module. It is blind to a backend that reaches another vendor's
/// spelling THROUGH a core helper that hard-codes a dialect, because the offending
/// literal then lives in core and no grep of the backend module can see it.
///
/// That is not hypothetical. When this test was written it was TRUE OF THIS TREE
/// while the test passed: `render::dml::quote_ident` and `quote_ident_checked` both
/// pinned `SqlDialect::Postgres` (and `quote_bare_ident` delegated to the first), so
/// every identifier `backends/sqlite.rs` emitted was quoted by the POSTGRESQL
/// renderer - correct only because both vendors spell an identifier `"x"`.
///
/// That instance is FIXED. `backends/sqlite.rs` now routes through
/// `SqliteDmlRenderer::quote_ident`, proven by neutering the PostgreSQL method: the
/// SQLite-only `sqlite_engine` binary went from 148 passed / 7 failed to 155 / 0 over
/// the same 155 tests, so the dependency is gone rather than merely re-covered.
///
/// AN EQUIVALENT INSTANCE SURVIVED ONE HOP AWAY, and this test was equally blind to
/// it: `render_sqlite_trigger_op`, then living in `render::lower`, called the
/// PostgreSQL-pinned `dml::quote_bare_ident` six times, and `backends/sqlite.rs`
/// delegated its trigger rendering there. That one is now fixed too — those six say
/// `quote_bare_ident_for_dialect(.., DIALECT)`, the pinned wrapper they used no longer
/// exists, and step 3 moved the three functions into `backends/sqlite.rs`, which is
/// why this test can now see them at all.
///
/// HOW it was proven is the part worth keeping, because the obvious proof LIED. The
/// neuter above works by watching a suite go red; run against the trigger path it did
/// not. `sqlite_engine` stayed at 156 passed / 0 failed with `PostgresDmlRenderer::
/// quote_ident` neutered AT THE COMMIT WHERE THE REACH WAS STILL LIVE — that binary
/// never renders a trigger, so its green meant nothing, and only running the
/// before-case as a control exposed it. `sqlite_trigger_render_bytes.rs` was written
/// to be an instrument that can see the path, and with it the same neuter fails
/// before the fix and passes after.
///
/// So both examples are WORKED ones now. The CLASS is not gone: it is invisible to
/// this test by construction, and both instances were found only because someone went
/// looking. Do not read either fix as evidence the class is gone — and do not trust a
/// neuter that stays green without first checking it can go red.
///
/// So a green run here means "no backend module NAMES another vendor". It does NOT
/// mean the backend boundary is clean, and a reader who takes it for that has been
/// given a proof of the wrong proposition. `backends/mod.rs` carries the same
/// warning at more length, with the measurement behind it; the second grep it asks
/// for - over the CORE helpers a moved branch calls - now HAS a test, next door in
/// `sqlite_trigger_quoting_reaches_postgres.rs`, which walks every `.rs` under `src/`
/// and pins the pinned-wrapper call count at ZERO.
///
/// # Two ways this goes red that are not defects
///
/// A module that legitimately needs its own dialect on a second line, and a
/// `DIALECT` const that moves out of the module, both fail here. Both are exactly
/// the edits the rule exists to make an author justify out loud, so the pin is
/// working; it is written down so the failure is read as a question and not as a bug
/// in this file.
#[test]
fn a_backend_module_names_only_its_own_dialect_and_only_once() {
    let cases = [
        (
            "zero-migrate-postgres/src/dml.rs",
            include_str!("../../../zero-migrate-postgres/src/dml.rs"),
            "Postgres",
        ),
        (
            "zero-migrate-sqlite/src/dml.rs",
            include_str!("../../../zero-migrate-sqlite/src/dml.rs"),
            "Sqlite",
        ),
        (
            "zero-migrate-mysql/src/dml.rs",
            include_str!("../../../zero-migrate-mysql/src/dml.rs"),
            "Mysql",
        ),
        (
            "zero-migrate-postgres/src/schema.rs",
            include_str!("../../../zero-migrate-postgres/src/schema.rs"),
            "Postgres",
        ),
        (
            "zero-migrate-sqlite/src/schema.rs",
            include_str!("../../../zero-migrate-sqlite/src/schema.rs"),
            "Sqlite",
        ),
        (
            "zero-migrate-mysql/src/schema.rs",
            include_str!("../../../zero-migrate-mysql/src/schema.rs"),
            "Mysql",
        ),
        (
            "zero-migrate-postgres/src/ddl.rs",
            include_str!("../../../zero-migrate-postgres/src/ddl.rs"),
            "Postgres",
        ),
        (
            "zero-migrate-sqlite/src/ddl.rs",
            include_str!("../../../zero-migrate-sqlite/src/ddl.rs"),
            "Sqlite",
        ),
        (
            "zero-migrate-mysql/src/ddl.rs",
            include_str!("../../../zero-migrate-mysql/src/ddl.rs"),
            "Mysql",
        ),
    ];

    // The two vendor-crate files that carry spelling but NO renderer, and therefore
    // no `DIALECT` const: PostgreSQL's vendor-op renderer is PostgreSQL by
    // construction (every vendor op is `dialect_scope = PgOnly`) and MySQL's
    // collation module is MySQL by construction. The "exactly once, as the const"
    // half of the rule has nothing to bind to in either, so only the half that DOES
    // apply is asserted: no FOREIGN dialect, at all.
    //
    // They are asserted rather than skipped because they are exactly where a
    // cross-vendor reach would be invisible — `vendor.rs` moved out of the engine in
    // the crate-extraction commit and `collation.rs` moved out of the declarative
    // differ, and in both former homes naming another dialect was ordinary.
    let unanchored = [
        (
            "zero-migrate-postgres/src/vendor.rs",
            include_str!("../../../zero-migrate-postgres/src/vendor.rs"),
            "Postgres",
        ),
        (
            "zero-migrate-mysql/src/collation.rs",
            include_str!("../../../zero-migrate-mysql/src/collation.rs"),
            "Mysql",
        ),
    ];
    for (file, src, own) in unanchored {
        for other in ["Postgres", "Sqlite", "Mysql"] {
            if other == own {
                continue;
            }
            let needle = format!("SqlDialect::{other}");
            assert_eq!(
                src.matches(needle.as_str()).count(),
                0,
                "{file} names {needle}; it is a {own}-only module in a {own}-only \
                 crate and must not reach another vendor's spelling. See the \
                 one-dialect-literal rule in render/backends/mod.rs."
            );
        }
    }

    for (file, src, own) in cases {
        for other in ["Postgres", "Sqlite", "Mysql"] {
            let needle = format!("SqlDialect::{other}");
            let hits = src.matches(needle.as_str()).count();
            let expected = usize::from(other == own);
            assert_eq!(
                hits, expected,
                "{file} names {needle} {hits} time(s); expected {expected} \
                 (its own dialect exactly once, as the DIALECT const; no other dialect). \
                 See the one-dialect-literal rule in backends/mod.rs."
            );
        }

        // ...and the one occurrence is the const, not merely some single line.
        let carriers: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("SqlDialect::"))
            .collect();
        let declaration = format!("const DIALECT: SqlDialect = SqlDialect::{own};");
        assert!(
            carriers.len() == 1 && carriers[0].contains(declaration.as_str()),
            "{file} must carry its dialect literal on exactly one line and that \
             line must declare `{declaration}`; a module whose single mention is somewhere \
             else has lost the const that makes the vendor deletable. Found: {carriers:?}"
        );
    }
}
