//! The one-dialect-literal rule of the backend module directories, enforced rather
//! than documented.
//!
//! There are FOUR renderer module families: DML, schema typing, DDL emission, and
//! value-format normalization.
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
//! The rule is invisible to every behaviour test. A PostgreSQL backend module that
//! reaches for the `MYSQL` dialect constant still emits correct PostgreSQL today; what it
//! costs is the extraction, and no assertion about emitted SQL can see that. So the
//! rule gets its own check or it has none, which is what it had.
//!
//! WHY THIS FILE AND NOT A `#[cfg(test)] mod tests` IN `backends/mod.rs`. The check
//! reads the RENDERER modules as TEXT — an enumerated list of `include_str!`s, one
//! per (basename, vendor) pair — which a unit test could do just as well. It lives
//! out here because it is a fact about the two layers' SHAPE rather than about their
//! behaviour, and because this theme binary is where the other "what does each
//! dialect declare" checks already are. It still reads the real files, so it tracks
//! them: `include_str!` is a compile-time dependency, and editing any of them
//! rebuilds this binary.
//!
//! (That sentence used to say "the nine modules". The list has been 17 for some time
//! — 4 anchored basenames plus 5 unanchored ones across three vendors — and a stale
//! count in prose reads as authoritative: it caused one reader to misreport this
//! census's scope. The list is the fact; there is no number here to go stale now.)
//!
//! # The second half: the EXECUTION modules
//!
//! The enumerated list above reaches ZERO `backend/` directories, and for most of
//! this census's life that was not a gap — the execution halves were inside the
//! engine, where a different census (`core_names_no_vendor_backend_module`) covered
//! them. All three left: `zero-migrate-mysql/src/backend/`,
//! `zero-migrate-sqlite/src/backend/`, `zero-migrate-postgres/src/backend/`. Nothing
//! covered them afterwards.
//!
//! [`no_backend_directory_module_names_a_foreign_dialect`] does, and it enforces the
//! SECOND clause only: a module names no OTHER vendor's dialect. Measured across all
//! three execution halves the day it was written, that clause is at zero, so it is
//! asserted as a real zero rather than ratcheted.
//!
//! The FIRST clause — "exactly once, as the `DIALECT` const" — is deliberately NOT
//! enforced over the whole walk. Each execution half carries its const in exactly one
//! file (`backend/mod.rs`) and every sibling reads `super::DIALECT`, which is the rule
//! working; asserting the const per FILE would redden 28 files that are correct.
//! The three `backend/mod.rs` files are pinned individually instead, and that pin
//! doubles as this walk's needle control.

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
/// This sees EXPLICIT coupling only - a foreign shipping dialect constant written
/// inside a backend module. It is blind to a backend that reaches another vendor's
/// spelling THROUGH a core helper that hard-codes a dialect, because the offending
/// literal then lives in core and no grep of the backend module can see it.
///
/// That is not hypothetical. When this test was written it was TRUE OF THIS TREE
/// while the test passed: `render::dml::quote_ident` and `quote_ident_checked` both
/// pinned the `POSTGRES` dialect constant (and `quote_bare_ident` delegated to the first), so
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
    fn code_identifier_hits(src: &str, identifier: &str) -> usize {
        src.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(|line| line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
            .filter(|token| *token == identifier)
            .count()
    }

    let cases = [
        (
            "zero-migrate-postgres/src/dml.rs",
            include_str!("../../../zero-migrate-postgres/src/dml.rs"),
            "POSTGRES",
        ),
        (
            "zero-migrate-sqlite/src/dml.rs",
            include_str!("../../../zero-migrate-sqlite/src/dml.rs"),
            "SQLITE",
        ),
        (
            "zero-migrate-mysql/src/dml.rs",
            include_str!("../../../zero-migrate-mysql/src/dml.rs"),
            "MYSQL",
        ),
        (
            "zero-migrate-postgres/src/schema.rs",
            include_str!("../../../zero-migrate-postgres/src/schema.rs"),
            "POSTGRES",
        ),
        (
            "zero-migrate-sqlite/src/schema.rs",
            include_str!("../../../zero-migrate-sqlite/src/schema.rs"),
            "SQLITE",
        ),
        (
            "zero-migrate-mysql/src/schema.rs",
            include_str!("../../../zero-migrate-mysql/src/schema.rs"),
            "MYSQL",
        ),
        (
            "zero-migrate-postgres/src/ddl.rs",
            include_str!("../../../zero-migrate-postgres/src/ddl.rs"),
            "POSTGRES",
        ),
        (
            "zero-migrate-sqlite/src/ddl.rs",
            include_str!("../../../zero-migrate-sqlite/src/ddl.rs"),
            "SQLITE",
        ),
        (
            "zero-migrate-mysql/src/ddl.rs",
            include_str!("../../../zero-migrate-mysql/src/ddl.rs"),
            "MYSQL",
        ),
        (
            "zero-migrate-postgres/src/value_format.rs",
            include_str!("../../../zero-migrate-postgres/src/value_format.rs"),
            "POSTGRES",
        ),
        (
            "zero-migrate-sqlite/src/value_format.rs",
            include_str!("../../../zero-migrate-sqlite/src/value_format.rs"),
            "SQLITE",
        ),
        (
            "zero-migrate-mysql/src/value_format.rs",
            include_str!("../../../zero-migrate-mysql/src/value_format.rs"),
            "MYSQL",
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
            "POSTGRES",
        ),
        (
            "zero-migrate-mysql/src/collation.rs",
            include_str!("../../../zero-migrate-mysql/src/collation.rs"),
            "MYSQL",
        ),
        (
            "zero-migrate-postgres/src/validation.rs",
            include_str!("../../../zero-migrate-postgres/src/validation.rs"),
            "POSTGRES",
        ),
        (
            "zero-migrate-sqlite/src/validation.rs",
            include_str!("../../../zero-migrate-sqlite/src/validation.rs"),
            "SQLITE",
        ),
        (
            "zero-migrate-mysql/src/validation.rs",
            include_str!("../../../zero-migrate-mysql/src/validation.rs"),
            "MYSQL",
        ),
    ];
    for (file, src, own) in unanchored {
        for other in ["POSTGRES", "SQLITE", "MYSQL"] {
            if other == own {
                continue;
            }
            assert_eq!(
                code_identifier_hits(src, other),
                0,
                "{file} names the foreign `{other}` dialect constant; it is a {own}-only module in a {own}-only \
                 crate and must not reach another vendor's spelling. See the \
                 one-dialect-literal rule in render/backends/mod.rs."
            );
        }
    }

    for (file, src, own) in cases {
        for other in ["POSTGRES", "SQLITE", "MYSQL"] {
            let hits = code_identifier_hits(src, other);
            // The module imports its own constant and assigns it to `DIALECT`.
            let expected = if other == own { 2 } else { 0 };
            assert_eq!(
                hits, expected,
                "{file} names the `{other}` dialect constant {hits} time(s); expected {expected} \
                 (its own constant once in the import and once in the DIALECT declaration; no other dialect). \
                 See the one-dialect-literal rule in backends/mod.rs."
            );
        }

        // ...and the use-site occurrence is the const, not merely some other line.
        let declaration = format!("const DIALECT: DialectId = {own};");
        assert!(
            src.lines().map(str::trim).any(|line| line == declaration),
            "{file} must carry its dialect identity in `{declaration}`; a module whose \
             own constant appears elsewhere has lost the const that makes the vendor deletable"
        );
    }
}

/// The vendor crates and the dialect constant each one IS, for the execution-half
/// walk below.
const VENDORS: &[(&str, &str)] = &[
    ("zero-migrate-postgres", "POSTGRES"),
    ("zero-migrate-sqlite", "SQLITE"),
    ("zero-migrate-mysql", "MYSQL"),
];

/// The three shipping dialect constants, so "foreign" is computed rather than listed
/// per vendor.
const DIALECT_CONSTANTS: &[&str] = &["POSTGRES", "SQLITE", "MYSQL"];

/// The walk's ANCHORS, relative to `crates/`: the file in each execution half that
/// carries its dialect const, the constant it IS, and the declaration verbatim.
///
/// A floor over a discovered set bounds HOW MANY files were read; these bound WHICH,
/// and they stay true at any size. They are also where the needle control runs — each
/// must show its own constant exactly twice (the import and the const) — so a matcher
/// that stopped recognizing a dialect constant fails here instead of reporting a
/// confident zero over every file it walked.
///
/// The declaration is spelled out per vendor rather than derived, because SQLite's
/// half names its const `SQLITE_DIALECT` (the module also carries `rusqlite`'s
/// `SQLITE_*` flag names, so a bare `DIALECT` would have read ambiguously to a human
/// scanning the file). Deriving the string would have quietly stopped matching there.
const EXECUTION_ANCHORS: &[(&str, &str, &str)] = &[
    (
        "zero-migrate-postgres/src/backend/mod.rs",
        "POSTGRES",
        "const DIALECT: DialectId = POSTGRES;",
    ),
    (
        "zero-migrate-sqlite/src/backend/mod.rs",
        "SQLITE",
        "const SQLITE_DIALECT: DialectId = SQLITE;",
    ),
    (
        "zero-migrate-mysql/src/backend/mod.rs",
        "MYSQL",
        "const DIALECT: DialectId = MYSQL;",
    ),
];

/// The walk's floor across the three execution halves. They hold 31 `.rs` files under
/// `src/backend` the day this was written; the floor sits under that with room for
/// ordinary churn and would still notice losing an entire ten-file directory
/// (31 minus PostgreSQL's ten is 21).
///
/// A scan over a DISCOVERED set fails OPEN: narrow the walk and it iterates nothing,
/// finds nothing, and reports clean. Raise it deliberately as the halves grow. NEVER
/// lower it to get green — check [`EXECUTION_ANCHORS`] first and trust them over this
/// number.
const EXECUTION_FILE_FLOOR: usize = 24;

/// How many times `src` names `identifier` on a CODE line, as a whole token.
///
/// The same matcher the enumerated check above uses, lifted to a free function so
/// both halves of this file provably run ONE needle. A census whose two halves used
/// two matchers could have one of them go blind while the other vouched for it.
fn code_identifier_hits(src: &str, identifier: &str) -> usize {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
        .filter(|token| *token == identifier)
        .count()
}

/// Every `.rs` file under `root`, sorted.
fn rs_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// CLAUSE TWO over the three EXECUTION halves: no file under a vendor's
/// `src/backend/` names another vendor's dialect constant.
///
/// # Why only clause two
///
/// See the module header. Clause one binds a `DIALECT` const to a MODULE; an
/// execution half is a directory whose siblings all read `super::DIALECT`, which is
/// the rule being obeyed rather than broken. Enforcing "exactly once per file" here
/// would redden 28 correct files in three vendors, which is a census demanding a
/// defect. The const's home is pinned per anchor instead.
///
/// # What a red here means
///
/// A vendor's execution half wrote another vendor's dialect constant. That is either
/// a cross-vendor reach — the thing this rule exists to make loud — or a test in that
/// half asserting something about a foreign dialect, which belongs in the engine's
/// test tree where all three are visible. Both are questions for the author; neither
/// is fixed by adding an exemption here.
#[test]
fn no_backend_directory_module_names_a_foreign_dialect() {
    let crates = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf();

    // ---- THE ANCHORS, which double as the NEEDLE POSITIVE CONTROL. -----------
    for (rel, own, declaration) in EXECUTION_ANCHORS {
        let path = crates.join(rel);
        assert!(
            path.is_file(),
            "this census must read {}, and it does not exist. If the execution half \
             legitimately moved, repoint the anchor and say so in the commit; do NOT \
             delete it to get green.",
            path.display()
        );
        let src = std::fs::read_to_string(&path).expect("anchor reads");
        let hits = code_identifier_hits(&src, own);
        assert_eq!(
            hits, 2,
            "{rel} names its own `{own}` constant {hits} time(s); expected 2 (the \
             import and the `DIALECT` declaration). Either the const moved — say so \
             deliberately — or `code_identifier_hits` stopped matching, in which case \
             the zero this test reports over every other file is blind."
        );
        assert!(
            src.lines()
                .map(str::trim)
                .any(|line| line.ends_with(declaration)),
            "{rel} must carry its execution half's dialect identity in \
             `{declaration}`; the siblings all read it through `super::` and a half \
             whose const moved has lost the one line that knows the vendor"
        );
    }

    // ---- THE WALK, with its floor. ------------------------------------------
    let mut walked = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for (vendor_crate, own) in VENDORS {
        let root = crates.join(vendor_crate).join("src").join("backend");
        assert!(
            root.is_dir(),
            "{} does not exist, so this census would walk nothing for {vendor_crate} \
             and report clean",
            root.display()
        );
        for path in rs_files(&root) {
            walked += 1;
            let rel = path
                .strip_prefix(&crates)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            for other in DIALECT_CONSTANTS {
                if other == own {
                    continue;
                }
                let hits = code_identifier_hits(&src, other);
                if hits > 0 {
                    violations.push(format!("  {rel}: names `{other}` {hits} time(s)"));
                }
            }
        }
    }

    assert!(
        walked >= EXECUTION_FILE_FLOOR,
        "the census walked only {walked} execution-half files, below the floor of \
         {EXECUTION_FILE_FLOOR}. A narrowed walk finds nothing and reports clean; fix \
         the walk, do not lower the floor to get green."
    );

    assert!(
        violations.is_empty(),
        "a vendor execution half names a FOREIGN dialect constant:\n{}\n\nA backend \
         module names its own dialect and no other. See the one-dialect-literal rule \
         in render/backends/mod.rs.",
        violations.join("\n")
    );
}
