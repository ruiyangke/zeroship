//! The SECOND grep `backends/mod.rs` asks for, which until now had no test.
//!
//! `backend_modules_name_one_dialect.rs` next door enforces that no backend module
//! NAMES another vendor, and says plainly what it cannot see: a backend that reaches
//! another vendor's spelling THROUGH a core helper that hard-codes a dialect, because
//! the offending literal then lives in core. Its own words: "the second grep it asks
//! for - over the CORE helpers a moved branch calls - has no test, here or anywhere."
//!
//! This is that test.
//!
//! # The coupling, measured rather than reasoned
//!
//! `render::dml::quote_bare_ident` is the PostgreSQL-PINNED identifier wrapper: it
//! delegates to `quote_ident`, which is `quote_ident_for_dialect(.., SqlDialect::
//! Postgres)`, which resolves to `PostgresDmlRenderer::quote_ident`.
//! `render::lower::render_sqlite_trigger_op` and its helpers call it SIX times, and
//! `backends/sqlite.rs` delegates its trigger rendering there. So every identifier in
//! a rendered SQLite trigger is quoted by the PostgreSQL renderer.
//!
//! VERIFIED, not inferred. A crate-extraction spike moved `backends/sqlite.rs` and
//! `apply/backend/sqlite/` into a real `zero-migrate-sqlite` crate, replaced
//! `PostgresDmlRenderer::quote_ident` with a marker string, and rendered a
//! `createTrigger` op from inside the extracted crate. The marker came back in the
//! SQLite trigger SQL. Had `zero-migrate-postgres` been a crate too,
//! `zero-migrate-sqlite` would have needed it at runtime to quote a trigger
//! identifier — which is the premise of step 4 ("mechanical once step 3 is done")
//! failing on its own terms.
//!
//! The same spike found these six are the ONLY live callers of the pinned wrapper
//! left in the crate: with the SQLite modules removed, rustc reported both
//! `quote_ident` and `quote_bare_ident` as `never used`. Every other identifier seam
//! (`apply::role`, `apply::journal`, `apply::backend::postgres`, `render::vendor`,
//! `conn`, `plan::author`) goes through `quote_ident_checked`, a different wrapper.
//!
//! # Why this test is TEXTUAL and not a render assertion
//!
//! Because no render assertion can exist. `PostgresDmlRenderer::quote_ident` and
//! `SqliteDmlRenderer::quote_ident` are both `dml::escape_quote_ident(ident)` -
//! byte-identical, for every input. Nothing an SQL-output test can feed them tells
//! the two apart, which is exactly why this coupling survived long enough to need a
//! source-editing spike to see. The second test below pins that fact, so the day the
//! two spellings diverge is the day this stops being latent and starts being a
//! defect - and you find out from the suite rather than from a user.

use std::path::{Path, PathBuf};

/// The number of PostgreSQL-pinned `quote_bare_ident` calls the SQLite trigger path
/// is currently allowed. It is a RATCHET, not a target: see the flip note below.
const PINNED_CALLS_IN_LOWER: usize = 6;

/// The census: exactly six PG-pinned identifier quotes, all of them in `lower.rs`.
///
/// # This test is GREEN today ON PURPOSE, and here is the argument
///
/// The coupling is real and unfixed, so an honest "the SQLite path must not reach
/// PostgreSQL's renderer" assertion would be RED on this commit. A permanently-red
/// test is not a guard - it is noise that trains people to ignore a colour. So this
/// pins TODAY'S NUMBER instead, which buys three things a red test or an `#[ignore]`
/// would not:
///
/// - it fails on SPREAD. A seventh call site - a new SQLite render path reaching the
///   pinned wrapper - goes red immediately. That direction is covered by nothing
///   else, and it is the direction this defect actually grows in.
/// - it fails on the FIX, loudly and with instructions. When the six become zero the
///   assertion below breaks and tells the author to set the const to 0, at which
///   point this file becomes the permanent "must never come back" guard.
/// - it cannot rot. `#[ignore]` produces no signal in either direction and nothing
///   forces anyone to un-ignore it afterwards; the stale-doc history in
///   `backends/mod.rs` is what that failure mode looks like in this tree.
///
/// # Two ways this goes red that are not defects
///
/// The count changing because `render_sqlite_trigger_op` was REFACTORED (helpers
/// split or merged) without the coupling changing, and the count going to zero
/// because the coupling was FIXED. Both should be read as a question about the
/// number, not as a bug in this file. Only an INCREASE is a new defect.
#[test]
fn sqlite_trigger_identifiers_are_still_quoted_by_the_postgres_pinned_wrapper() {
    // `include_str!` is a compile-time dependency: editing lower.rs rebuilds this
    // binary, so the count cannot silently drift out from under the pin.
    let lower = include_str!("../../src/render/lower.rs");
    let in_lower = count_pinned_calls(lower);

    assert_eq!(
        in_lower, PINNED_CALLS_IN_LOWER,
        "render/lower.rs makes {in_lower} call(s) to the PostgreSQL-pinned \
         `dml::quote_bare_ident`; the pin says {PINNED_CALLS_IN_LOWER}.\n\
         \n\
         MORE than the pin: a new SQLite render path is being quoted by the \
         PostgreSQL renderer. That is the defect this test exists for - route it \
         through `quote_bare_ident_for_dialect(.., DIALECT)` instead.\n\
         \n\
         FEWER, and ZERO especially: the coupling is being fixed. Set \
         PINNED_CALLS_IN_LOWER to {in_lower}. At 0, delete nothing - this file \
         becomes the guard that stops it coming back, and the module header's \
         `zero-migrate-sqlite would need zero-migrate-postgres` finding becomes \
         false, which is the point."
    );

    // ...and nowhere else in the crate acquired one. `include_str!` cannot express
    // "no other file", so this half walks the real tree at run time.
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut elsewhere: Vec<(String, usize)> = Vec::new();
    let mut total = 0usize;
    for file in rust_sources(&src) {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        let hits = count_pinned_calls(&text);
        if hits == 0 {
            continue;
        }
        total += hits;
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(&file)
            .display()
            .to_string();
        if rel != "render/lower.rs" {
            elsewhere.push((rel, hits));
        }
    }

    assert!(
        elsewhere.is_empty(),
        "the PostgreSQL-pinned `dml::quote_bare_ident` is called outside \
         render/lower.rs: {elsewhere:?}. Every identifier seam in the crate other \
         than the SQLite trigger path goes through `quote_ident_checked` (also \
         PG-pinned, but its callers ARE PostgreSQL). A new caller here is either a \
         PostgreSQL path that should say `quote_ident_checked`, or a non-PostgreSQL \
         path that should say `quote_bare_ident_for_dialect(.., DIALECT)`."
    );
    assert_eq!(
        total, in_lower,
        "crate-wide pinned-call count ({total}) disagrees with the lower.rs count \
         ({in_lower}) even though no other file reported hits; the two counters \
         have diverged and one of them is lying."
    );
}

/// PostgreSQL and SQLite spell an identifier identically, which is what makes the
/// coupling above invisible to behaviour.
///
/// This is the load-bearing companion to the census. While it is green, the six
/// pinned calls are a LATENT extraction blocker that emits correct bytes. The moment
/// it goes red - a SQLite build that quotes with brackets, a PostgreSQL build that
/// folds case differently - those same six calls become a live wrong-SQL defect, and
/// the census pin above stops being a tidiness argument.
///
/// Going red here is therefore not a failure of this test. It is a promotion of the
/// other one to urgent.
#[test]
fn postgres_and_sqlite_spell_an_identifier_identically_which_hides_the_coupling() {
    let pg = quote_ident_body(include_str!("../../src/render/backends/postgres.rs"));
    let sqlite = quote_ident_body(include_str!("../../src/render/backends/sqlite.rs"));

    assert_eq!(
        pg, sqlite,
        "PostgresDmlRenderer::quote_ident and SqliteDmlRenderer::quote_ident no \
         longer share an implementation.\n\
         \n\
         READ THIS AS AN ESCALATION, NOT A REGRESSION. While the two agreed, the \
         six PostgreSQL-pinned quotes in `render::lower::render_sqlite_trigger_op` \
         (pinned by the test above) emitted correct SQLite anyway. They no longer \
         do: SQLite triggers are now quoted by PostgreSQL's rules and the SQL is \
         wrong. Fix those six FIRST, then update or delete this test."
    );
}

/// Count calls to the PostgreSQL-pinned `quote_bare_ident`, excluding its own
/// declaration, its dialect-parameterized sibling, and prose.
///
/// The open paren is what separates the pinned form from
/// `quote_bare_ident_for_dialect(` - the sibling never matches, because
/// `_for_dialect` sits between the name and the paren.
fn count_pinned_calls(src: &str) -> usize {
    src.lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("//") && !t.contains("fn quote_bare_ident")
        })
        .map(|line| line.matches("quote_bare_ident(").count())
        .sum()
}

/// The body of the `DmlRenderer::quote_ident` METHOD in a backend module.
///
/// Anchored on `&self` so it cannot be confused with a free `fn quote_ident(ident:
/// &str)` that a step-3 move might land in the same file, and terminated on the
/// 4-space `}` that closes a trait-impl method.
fn quote_ident_body(src: &str) -> String {
    let mut lines = src
        .lines()
        .skip_while(|l| !l.contains("fn quote_ident(&self, ident: &str) -> String {"));
    assert!(
        lines.next().is_some(),
        "backend module has no `DmlRenderer::quote_ident` method; this test's anchor \
         is stale (or the trait method was renamed)"
    );
    lines
        .take_while(|l| *l != "    }")
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries =
            std::fs::read_dir(&d).unwrap_or_else(|e| panic!("reading {}: {e}", d.display()));
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
