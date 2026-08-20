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
//! # The coupling, measured rather than reasoned — and now FIXED
//!
//! `render::dml::quote_bare_ident` is the PostgreSQL-PINNED identifier wrapper: it
//! delegates to `quote_ident`, which is `quote_ident_for_dialect(.., SqlDialect::
//! Postgres)`, which resolves to `PostgresDmlRenderer::quote_ident`.
//! `render_sqlite_trigger_op` and its helpers called it SIX times, and
//! `backends/sqlite.rs` delegates its trigger rendering there. So every identifier in
//! a rendered SQLite trigger was quoted by the PostgreSQL renderer.
//!
//! THAT IS PAST TENSE AS OF THIS FILE'S `PINNED_CALLS_IN_THE_TRIGGER_PATH = 0`. Those
//! three functions now route every identifier through `quote_bare_ident_for_dialect(..,
//! DIALECT)`, a single `const` in the module that also absorbed the thirteen other
//! dialect literals they carried — so exactly one line in the SQLite trigger path
//! knows which vendor it is, which is the rule `backends/mod.rs` already states for a
//! backend module. The paragraphs below are kept in the past tense rather than
//! deleted: they are the measurement that made the fix arguable, and this file's job
//! now is to stop it coming back.
//!
//! # The subject MOVED, and this file followed it deliberately
//!
//! Those three functions lived in `render/lower.rs` when this test was written, and
//! this census read `lower.rs` by name. Step 3 of `docs/proposals/pluggable-backends.md`
//! relocated them into `render/backends/sqlite.rs`, where MySQL's trigger spelling
//! already lived. A census whose needle is a filename does not notice that: it would
//! have gone on scanning a file that no longer contains its subject and reporting a
//! comfortable zero forever. So the census names its subject FILE and its subject
//! FUNCTION, and its first act is `assert_subject_is_where_this_file_says_it_is`,
//! which fails if the trigger renderer is not in the file being scanned OR is still
//! in the file it left. Repointing this file was part of the move, not a follow-up —
//! and the anchor is what makes the next move fail loudly instead of silently.
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
//! `SqliteDmlRenderer::quote_ident` are both `super::ansi_double_quote_ident(ident)`
//! (they were `dml::escape_quote_ident(ident)` until that primitive moved into
//! `render::backends` and became private to it) - byte-identical, for every input,
//! by either spelling. Nothing an SQL-output test can feed them tells
//! the two apart, which is exactly why this coupling survived long enough to need a
//! source-editing spike to see. The second test below pins that fact, so the day the
//! two spellings diverge is the day this stops being latent and starts being a
//! defect - and you find out from the suite rather than from a user.

use std::path::{Path, PathBuf};

/// The number of PostgreSQL-pinned `quote_bare_ident` calls the SQLite trigger path
/// is currently allowed. It is a RATCHET, not a target: see the flip note below.
///
/// FLIPPED FROM 6 TO 0. `render_sqlite_trigger_op` and its two helpers now route
/// every identifier through `quote_bare_ident_for_dialect(.., DIALECT)`, a single
/// `const DIALECT` that also absorbed the thirteen other dialect literals those three
/// functions carried. Nothing about the emitted SQL moved — it could not, for the
/// reason the second test below pins — so this file is now the "must never come back"
/// guard its own flip note promised it would become, and the module header's
/// `zero-migrate-sqlite would need zero-migrate-postgres` finding is FALSE.
const PINNED_CALLS_IN_THE_TRIGGER_PATH: usize = 0;

/// The file the SQLite trigger spelling lives in, and the file it LEFT.
///
/// Both are named because the anchor is two-sided: "it is here" alone would pass on a
/// copy, and "it is not there" alone would pass on a deletion.
const SUBJECT_FILE: &str = "render/backends/sqlite.rs";
const FORMER_SUBJECT_FILE: &str = "render/lower.rs";

/// The census floor for the crate-wide half below.
///
/// That half must WALK the tree, because `include_str!` cannot express "no other
/// file". A walk fails OPEN: narrow its root and it iterates nothing, finds nothing,
/// and reports clean. Rooted at `CARGO_MANIFEST_DIR` it was MEASURED blind — a pinned
/// call planted in `zero-migrate-ir` did not register — and after the backend crates
/// are extracted it would go blind to the very modules this file exists to watch.
///
/// So the walk covers every crate, and this floor asserts the walk FOUND them. Raise
/// it when a crate is added. If one is genuinely removed, lower it deliberately and
/// say so — never to get green.
const WORKSPACE_CRATE_FLOOR: usize = 5;

/// The subject itself. `render_sqlite_trigger_op` is the entry point
/// `SqliteDmlRenderer::render_trigger_op` calls, and the two helpers under it are
/// where four of the original six pinned calls sat.
const SUBJECT_FN: &str = "fn render_sqlite_trigger_op(";

/// The census: ZERO PG-pinned identifier quotes, in the SQLite trigger path's own
/// module or anywhere else.
///
/// # This test was GREEN at 6 on purpose, and is now green at 0 for a better reason
///
/// While the coupling was real and unfixed, an honest "the SQLite path must not reach
/// PostgreSQL's renderer" assertion would have been RED on every commit. A
/// permanently-red test is not a guard - it is noise that trains people to ignore a
/// colour. So this pinned TODAY'S NUMBER instead, which bought three things a red
/// test or an `#[ignore]` would not:
///
/// - it failed on SPREAD. A seventh call site - a new SQLite render path reaching the
///   pinned wrapper - went red immediately. That direction was covered by nothing
///   else, and it is the direction this defect actually grew in.
/// - it failed on the FIX, loudly and with instructions. THAT IS WHAT HAPPENED: the
///   six became zero, this assertion broke, and its own message said to set the const
///   to 0. It did not have to be believed on trust; it computed the new number.
/// - it could not rot. `#[ignore]` produces no signal in either direction and nothing
///   forces anyone to un-ignore it afterwards; the stale-doc history in
///   `backends/mod.rs` is what that failure mode looks like in this tree.
///
/// At 0 the ratchet is at its stop and both halves say the same thing: no file in the
/// crate may call the PostgreSQL-pinned wrapper. Nothing here was relaxed to get
/// green - the assertion is strictly stronger than the one it replaced, because 0 is
/// the only value that admits no PostgreSQL reach at all.
///
/// # Two ways this goes red that are not defects
///
/// Neither survives the flip. The count can no longer move for a REFACTOR (a helper
/// split cannot create a call that is not there), and it can no longer move for a FIX
/// (there is nothing left to fix). Any increase from 0 is a new defect, full stop:
/// route the new site through `quote_bare_ident_for_dialect(.., DIALECT)`.
#[test]
fn no_sqlite_render_path_is_quoted_by_the_postgres_pinned_wrapper() {
    // `include_str!` is a compile-time dependency: editing either file rebuilds this
    // binary, so neither the count nor the anchor can silently drift out from under
    // the pin.
    let subject = include_str!("../../src/render/backends/sqlite.rs");
    let former = include_str!("../../src/render/lower.rs");
    assert_subject_is_where_this_file_says_it_is(subject, former);

    let in_subject = count_pinned_calls(subject);

    assert_eq!(
        in_subject, PINNED_CALLS_IN_THE_TRIGGER_PATH,
        "src/{SUBJECT_FILE} makes {in_subject} call(s) to the PostgreSQL-pinned \
         `dml::quote_bare_ident`; the pin says {PINNED_CALLS_IN_THE_TRIGGER_PATH}.\n\
         \n\
         The pin is at 0 and 0 is the stop: the SQLite trigger path was routed \
         through `quote_bare_ident_for_dialect(.., DIALECT)` and nothing in the \
         crate calls the pinned wrapper any more. So {in_subject} > 0 \
         means a render path has been quoted by the PostgreSQL renderer again - \
         route it through `quote_bare_ident_for_dialect(.., DIALECT)`, naming the \
         dialect ONCE per module as `lower.rs` and `backends/*.rs` both do.\n\
         \n\
         DO NOT RAISE THIS CONST TO MAKE THE BUILD GREEN. Raising it re-admits the \
         `zero-migrate-sqlite needs zero-migrate-postgres at runtime` dependency \
         that the module header measured and this commit removed, and it does so \
         silently, because both vendors still spell an identifier `\"x\"` and no \
         SQL-output test can tell you."
    );

    // ...and nowhere else in the WORKSPACE acquired one. `include_str!` cannot
    // express "no other file", so this half walks the real tree at run time.
    //
    // IT WALKS EVERY CRATE, NOT JUST THIS ONE'S `src`, AND THAT IS THE POINT. A walk
    // rooted at `CARGO_MANIFEST_DIR` is a census whose UNIVERSE SHRINKS when the
    // thing it guards moves away: once the backend crates are extracted,
    // `zero-migrate/src` simply stops containing the vendor modules, the walk finds
    // zero in a smaller tree, and this half passes while guarding nothing. That is
    // not a hypothetical - the crate-scoped version was MEASURED blind, reporting
    // clean with a pinned call planted in `zero-migrate-ir`.
    //
    // Generalised, because this file is the worked example: ANY assertion that scans
    // a DIRECTORY rather than a NAMED SET reports success when its subject leaves
    // the directory. The `include_str!` half above cannot fail that way - a dangling
    // path is a compile error - which is exactly why the two halves are kept
    // together.
    let crates_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .to_path_buf();
    let this_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .file_name()
        .expect("crate directory has a name")
        .to_string_lossy()
        .into_owned();
    let subject_rel = format!("{this_crate}/src/{SUBJECT_FILE}");

    let mut roots: Vec<PathBuf> = std::fs::read_dir(&crates_root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", crates_root.display()))
        .map(|e| e.expect("dir entry").path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    roots.sort();

    // The census floor. A scan over a DISCOVERED set fails OPEN: break the discovery
    // and it iterates nothing, finds nothing, and reports clean. This is the same
    // floor the schema and DML backend tripwires carry, for the same reason.
    assert!(
        roots.len() >= WORKSPACE_CRATE_FLOOR,
        "found only {} crate `src` root(s) under {}, expected at least \
         {WORKSPACE_CRATE_FLOOR}: {roots:?}.\n\
         \n\
         This census walks the tree, so a broken or narrowed discovery makes it \
         iterate NOTHING and pass. Raise the floor when a crate is ADDED; if a crate \
         was REMOVED, say so deliberately - do not lower it to get green.",
        roots.len(),
        crates_root.display()
    );

    let mut elsewhere: Vec<(String, usize)> = Vec::new();
    let mut total = 0usize;
    for root in &roots {
        for file in rust_sources(root) {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
            let hits = count_pinned_calls(&text);
            if hits == 0 {
                continue;
            }
            total += hits;
            let rel = file
                .strip_prefix(&crates_root)
                .unwrap_or(&file)
                .display()
                .to_string();
            if rel != subject_rel {
                elsewhere.push((rel, hits));
            }
        }
    }

    assert!(
        elsewhere.is_empty(),
        "the PostgreSQL-pinned `dml::quote_bare_ident` is called outside \
         {subject_rel}: {elsewhere:?}. Paths are workspace-relative because this \
         census walks EVERY crate, not just this one. Every identifier seam other \
         than the SQLite trigger path goes through `quote_ident_checked` (also \
         PG-pinned, but its callers ARE PostgreSQL). A new caller here is either a \
         PostgreSQL path that should say `quote_ident_checked`, or a non-PostgreSQL \
         path that should say `quote_bare_ident_for_dialect(.., DIALECT)`."
    );
    assert_eq!(
        total, in_subject,
        "workspace-wide pinned-call count ({total}) disagrees with the {subject_rel} \
         count ({in_subject}) even though no other file reported hits; the two counters \
         have diverged and one of them is lying."
    );
}

/// The anchor: the census must be reading the file the SQLite trigger spelling is
/// actually in.
///
/// # Why a census needs one
///
/// This file's first half is a TEXTUAL count over a NAMED file. That construction has
/// exactly one silent failure mode, and it is not a wrong count: it is a right count
/// of the wrong file. Move the subject out and the needle finds nothing, the count is
/// 0, the pin is 0, and the suite is green while the thing being guarded is
/// unguarded. That is strictly worse than deleting the test, because the green is
/// read as evidence.
///
/// It is not hypothetical here. Step 3 of `docs/proposals/pluggable-backends.md`
/// moved these three functions out of `render/lower.rs` and into
/// `render/backends/sqlite.rs`, next to MySQL's trigger spelling. This assertion is
/// what turned that move into a RED that had to be answered.
///
/// Both directions are checked on purpose. "The subject is in the file I scan" alone
/// passes if a copy is left behind in the old home and keeps quoting through the
/// pinned wrapper; "the subject is not in the old file" alone passes if it was simply
/// deleted. The pair says it moved, once, to here.
fn assert_subject_is_where_this_file_says_it_is(subject: &str, former: &str) {
    assert!(
        subject.contains(SUBJECT_FN),
        "src/{SUBJECT_FILE} does not contain `{SUBJECT_FN}`, so the census below is \
         counting a file that does not hold the SQLite trigger spelling and its zero \
         means nothing.\n\
         \n\
         Either the renderer moved again — repoint SUBJECT_FILE at its new home in \
         the SAME commit that moves it, exactly as the step-3 move did — or it was \
         renamed, in which case repoint SUBJECT_FN. Do not delete this assertion to \
         get green: it exists because a census that scans a file its subject has left \
         is the one failure this test cannot otherwise see."
    );
    assert!(
        !former.contains(SUBJECT_FN),
        "src/{FORMER_SUBJECT_FILE} still contains `{SUBJECT_FN}`. The SQLite trigger \
         spelling was moved OUT of the core lowerer into src/{SUBJECT_FILE}; a copy \
         left behind there is a second implementation the census does not scan, and \
         the one it does scan will happily report zero for it."
    );
}

/// PostgreSQL and SQLite spell an identifier identically, which is what makes the
/// coupling above invisible to behaviour.
///
/// This was the load-bearing companion to the census while the census stood at six:
/// it is WHY those six pinned calls were a latent extraction blocker that emitted
/// correct bytes rather than a live wrong-SQL defect, and therefore why the coupling
/// needed a source-editing spike to see at all.
///
/// # What it is for now that the census is at zero
///
/// It is the reason the fix is PROVABLY byte-neutral. Both `quote_ident`
/// implementations are the same `super::ansi_double_quote_ident(ident)` (they were
/// `dml::escape_quote_ident(ident)` until that primitive moved into
/// `render::backends` and became private to it; the move changed the name and the
/// visibility, not the bytes), so re-pointing six
/// calls from one to the other cannot move a byte of emitted SQL - which is the
/// claim the gate's identical pass counts corroborate but cannot, on their own,
/// distinguish from "the changed path is untested".
///
/// It is ALSO the tripwire for the day the two spellings diverge. That day is now
/// allowed: `render_sqlite_trigger_op` asks for SQLite's spelling by name, so a
/// bracket-quoting SQLite build would come out correct instead of coming out as
/// PostgreSQL. So read a red here as a QUESTION - "who still assumes these agree?" -
/// and answer it by re-running the census above. If the census is still 0, nothing
/// is broken and this test has done its job and can be retired with that noted; if
/// the census has drifted above 0, those calls are now emitting wrong SQL and this
/// red is the escalation.
#[test]
fn postgres_and_sqlite_still_spell_an_identifier_identically() {
    let pg = quote_ident_body(include_str!("../../src/render/backends/postgres.rs"));
    let sqlite = quote_ident_body(include_str!("../../src/render/backends/sqlite.rs"));

    assert_eq!(
        pg, sqlite,
        "PostgresDmlRenderer::quote_ident and SqliteDmlRenderer::quote_ident no \
         longer share an implementation.\n\
         \n\
         READ THIS AS A QUESTION, NOT A REGRESSION, and the census test above \
         answers it. If that test is GREEN at 0, no render path is asking \
         PostgreSQL to spell a SQLite identifier, the divergence is exactly what \
         `render_sqlite_trigger_op` naming its own dialect was FOR, and this test \
         has outlived its subject - retire it and say so.\n\
         \n\
         If that test is RED above 0, this is the escalation: those calls were \
         emitting correct SQLite only by the coincidence this assertion pinned, \
         and they are now emitting PostgreSQL's spelling into SQLite triggers. Fix \
         them first."
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
