//! The one-dialect-literal rule of `src/render/backends/`, enforced rather than
//! documented.
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
//! reads the three modules as TEXT, which a unit test could do with `include_str!`
//! just as well. It lives out here because it is a fact about the render layer's
//! SHAPE rather than about its behaviour, and because this theme binary is where the
//! other "what does each dialect declare" checks already are. It still reads the
//! real files, so it tracks them: `include_str!` is a compile-time dependency, and
//! editing any of the three rebuilds this binary.

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
/// AN EQUIVALENT INSTANCE SURVIVES ONE HOP AWAY, and this test is equally blind to
/// it: `render::lower::render_sqlite_trigger_op` calls the PostgreSQL-pinned
/// `dml::quote_bare_ident` six times, and `backends/sqlite.rs` delegates its trigger
/// rendering there. So the example below is now a WORKED one rather than a live
/// defect, and the class it illustrates is still present in the tree. Do not read the
/// fix as evidence the class is gone.
///
/// So a green run here means "no backend module NAMES another vendor". It does NOT
/// mean the backend boundary is clean, and a reader who takes it for that has been
/// given a proof of the wrong proposition. `backends/mod.rs` carries the same
/// warning at more length, with the measurement behind it; the second grep it asks
/// for - over the CORE helpers a moved branch calls - has no test, here or anywhere.
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
            "postgres.rs",
            include_str!("../../src/render/backends/postgres.rs"),
            "Postgres",
        ),
        (
            "sqlite.rs",
            include_str!("../../src/render/backends/sqlite.rs"),
            "Sqlite",
        ),
        (
            "mysql.rs",
            include_str!("../../src/render/backends/mysql.rs"),
            "Mysql",
        ),
    ];

    for (file, src, own) in cases {
        for other in ["Postgres", "Sqlite", "Mysql"] {
            let needle = format!("SqlDialect::{other}");
            let hits = src.matches(needle.as_str()).count();
            let expected = usize::from(other == own);
            assert_eq!(
                hits, expected,
                "backends/{file} names {needle} {hits} time(s); expected {expected} \
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
            "backends/{file} must carry its dialect literal on exactly one line and that \
             line must declare `{declaration}`; a module whose single mention is somewhere \
             else has lost the const that makes the vendor deletable. Found: {carriers:?}"
        );
    }
}
