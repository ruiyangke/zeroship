//! The census that replaces a VISIBILITY the crate split could not carry across a
//! crate boundary. Both `zero-migrate-backend/src/spelling.rs` and
//! `zero-migrate-core/src/render/backends/mod.rs` name this file by path as their
//! replacement; until this commit neither of those sentences was true.
//!
//! # What was lost, exactly
//!
//! `ansi_double_quote_ident` and `base64_standard` were
//! `pub(in crate::render::backends)` in `render/backends/mod.rs`. The engine was
//! PHYSICALLY UNABLE TO NAME THEM. That unreachability WAS the invariant — core
//! cannot spell an identifier itself, it must pick a named door and put a vendor on
//! the record — and the compiler enforced it at the DEFINITION, so no call site had
//! to be audited and no reviewer had to remember.
//!
//! They now live in `zero-migrate-backend/src/spelling.rs`. Across a crate boundary
//! `pub(in …)` cannot express "these vendor crates and no other": the vendor crates
//! genuinely need to reach them, so they are `pub`, so the engine can name them too.
//! Nothing enforces the rule any more.
//!
//! # The general hazard, stated once because the next extraction will meet it
//!
//! A PRIVACY-BASED INVARIANT DOES NOT SURVIVE A CRATE BOUNDARY. `pub(in path)` is
//! scoped to a module tree inside one crate; there is no `pub(in these three crates)`
//! and there is not going to be one. So the STRONGEST kind of invariant
//! (compiler-enforced, checked at the definition, impossible to delete without a
//! build error) degrades in one refactor to the WEAKEST kind (a comment) — and it
//! degrades precisely BECAUSE it was never also a test. A rule that is only a
//! visibility has no artifact to carry forward when the visibility stops being
//! expressible.
//!
//! The operational form of that: an extraction should audit for `pub(in …)`
//! invariants BEFORE moving code, not after, and land the replacement test in the
//! SAME commit as the move. `apply/backend` is 37,950 lines and is next in line.
//!
//! # Why this cannot be a behaviour test, and never could be
//!
//! Two of the three shipping vendors AGREE on the ANSI double-quote spelling, and two
//! of the three agree on standard base64. An engine caller that spells those bytes
//! itself is spelling them FOR A VENDOR IT NEVER NAMED — and the bytes come out
//! RIGHT. The routing is what is missing, and no assertion about emitted SQL can see
//! absent routing while the vendors agree. That is the whole defect class
//! `render/backends/mod.rs` was written to describe, and it is why the replacement
//! has to be textual: there is no observable behaviour to assert on.
//!
//! # This is a DOWNGRADE, not an equal substitute
//!
//! A privacy violation cannot be deleted; this file can. It is recorded as strictly
//! weaker in both source headers and it is recorded as strictly weaker here. What it
//! buys back is the part that actually rots — nobody notices a new caller — and it
//! carries the three defences this repo has needed, each of which is here because the
//! corresponding failure ACTUALLY HAPPENED in this tree (see
//! `sqlite_trigger_quoting_reaches_postgres.rs`, the worked example):
//!
//! - a TWO-SIDED SUBJECT ANCHOR. "The primitives are in the file I scan" alone passes
//!   on a leftover copy in the old home; "they are not in the old file" alone passes
//!   on a deletion. The pair says they moved, once, to here.
//! - a CENSUS FLOOR. A scan over a DISCOVERED set FAILS OPEN: narrow the discovery
//!   and it iterates nothing, finds nothing, reports clean. That was MEASURED blind
//!   next door, not predicted — a planted call in `zero-migrate-ir` went unreported
//!   by a crate-scoped walk. There are two floors here because there are two ways to
//!   discover nothing: the WALK can be narrowed, and the NEEDLE can stop matching.
//! - `include_str!` FOR THE ANCHOR. A compile-time dependency, so the pin cannot
//!   drift out from under an edit and a dangling path is a build error, which is the
//!   loud failure.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The raw spelling primitives: byte-logic that MORE THAN ONE vendor agrees on, and
/// therefore the exact things core must not reach un-named.
///
/// The needle is the bare function name plus `(`, so it matches the bare call a `use`
/// would produce AND the fully-qualified `zeroship_migrate_backend::spelling::…(` form
/// the vendors currently write. Anything that reaches the primitive has to spell its
/// name somewhere; a name with no `(` after it is prose, and prose is not a call.
/// That distinction is load-bearing and is measured, not assumed: `zero-migrate-core/src`
/// MENTIONS `ansi_double_quote_ident` six times today, across `render/backends/mod.rs`,
/// `render/dml.rs` and `schema/query.rs`, every one of them a doc comment explaining
/// why the engine must not call it. A census that counted mentions would be red on
/// day one for exactly the wrong reason, and the only way to green it would be
/// deleting the documentation that states the rule.
const SPELLING_PRIMITIVES: &[&str] = &["ansi_double_quote_ident", "base64_standard"];

/// One found call: the workspace-relative file, which primitive, and how many times.
///
/// Named so the failure message prints a shape a reader can act on — the file is what
/// you open, the primitive is what you replace, the count is whether it is one slip or
/// a pattern.
type SpellingCall = (String, &'static str, usize);

/// The primitives' one physical home, and the file they LEFT.
///
/// Both are named because the anchor is two-sided. The former home is where they were
/// `pub(in crate::render::backends)`; a copy left behind there would be a second
/// physical home for byte-logic whose entire discipline is having exactly one.
const SPELLING_HOME: &str = "zero-migrate-backend/src/spelling.rs";
const FORMER_SPELLING_HOME: &str = "zero-migrate-core/src/render/backends/mod.rs";

/// The crates that MAY call a raw spelling primitive, and the crates that MUST NOT.
///
/// This is an ALLOW-LIST, which is the whole point: a crate that is on neither list
/// is a RED, not a silent pass. The visibility this replaces failed closed — an
/// unlisted module simply could not name the function — so its replacement has to
/// fail closed too. A deny-list would let the tenth crate walk straight through.
///
/// `zero-migrate-backend` is on the allow list because it DEFINES them. The other
/// three are the shipping vendors, and each is spelling bytes for ITSELF, which is
/// the only thing these functions are for.
const CRATES_THAT_MAY_SPELL: &[&str] = &[
    "zero-migrate-backend",
    "zero-migrate-mysql",
    "zero-migrate-postgres",
    "zero-migrate-sqlite",
];

/// The engine and the non-vendor libraries. Named rather than inferred, so that a
/// RENAME goes red instead of quietly moving a crate from "denied" to "unknown".
///
/// `zero-migrate-core` is the one the lost visibility was actually about — it is the
/// ENGINE, and the primitives were `pub(in crate::render::backends)` inside it. The
/// others are here because the same argument applies verbatim: none of them is a
/// vendor, so none of them has a vendor to name, so a raw spelling in any of them is
/// bytes emitted on behalf of nobody.
///
/// `zero-migrate` is on this list too, and after the crate split that is a stronger
/// claim rather than a formality. It is the COMPOSITION: it names all three vendors on
/// purpose, so it is the one non-vendor crate with a plausible-looking excuse to spell
/// a vendor's bytes. It has none. Composing a registry is not emitting SQL.
///
/// `zero-migrate-guard` came OFF this list when it was dissolved. It is not a rename:
/// its contents moved into `zero-migrate-postgres`, which sits on
/// [`CRATES_THAT_MAY_SPELL`] above — so the code did not become unclassified, it
/// changed classification, from "must not spell" to "may spell for itself". That is
/// correct rather than a loosening: every line of that crate parsed PostgreSQL with
/// `libpg_query`, so it was always one vendor's bytes filed under a neutral-sounding
/// name.
const CRATES_THAT_MUST_NOT_SPELL: &[&str] = &[
    "zero-migrate",
    "zero-migrate-core",
    "zero-migrate-ir",
    "zero-migrate-node",
    "zero-migrate-policy",
];

/// The census floor for the walk, and it is the SAME eight roots
/// `sqlite_trigger_quoting_reaches_postgres.rs` walks, for the same reason.
///
/// Raise it when a crate is ADDED. If one is genuinely removed, lower it deliberately
/// and say so — never to get green.
///
/// LOWERED 9 → 8: `zero-migrate-guard` was dissolved. Every line of it needed
/// `libpg_query` to parse PostgreSQL, so all of it was one vendor's code; it moved
/// into `zero-migrate-postgres` (`guard/sql.rs`, `guard/denylist.rs`, `analysis/`) and
/// the crate was deleted from the workspace. The walk still reaches the same source —
/// it is under a different root — so this is the count following a real removal, not
/// a narrowed discovery. The `crates/` listing itself is the check: eight entries,
/// nine before.
///
/// RAISED 8 -> 9: `zero-migrate-core` was ADDED. The engine and the composition that
/// names the vendors are two crates now, so `crates/` holds nine entries and the walk
/// must find all of them. This is the direction the doc above prescribes for an added
/// crate, and the new root is the one this census is most about: the engine.
const WORKSPACE_CRATE_FLOOR: usize = 9;

/// The NEEDLE-LIVENESS floor: the calls the vendors are known to make today.
///
/// The crate floor above defends the WALK. This defends the MATCH, and they are
/// different failure modes with the same symptom. Change `SPELLING_PRIMITIVES` to
/// something that no longer matches — a rename, a `_` typo, dropping the `(` — and
/// the walk still visits all nine roots, still reads every file, and still finds zero
/// violations, because it finds zero of everything. A census with no positive control
/// cannot tell "clean" from "blind".
///
/// Measured on the unmodified tree: `ansi_double_quote_ident` twice
/// (`zero-migrate-postgres`, `zero-migrate-sqlite`), `base64_standard` three times
/// (`zero-migrate-mysql` once, `zero-migrate-postgres` twice). Per-primitive rather
/// than a single total, so that breaking ONE needle cannot be masked by the other
/// still matching.
///
/// A drop below these is a QUESTION, not automatically a defect: either the needle
/// stopped matching (fix the needle) or a vendor stopped spelling those bytes (lower
/// the floor deliberately, in the commit that removes the call, and say which).
const VENDOR_CALL_FLOOR: &[(&str, usize)] =
    &[("ansi_double_quote_ident", 2), ("base64_standard", 3)];

/// The census: no crate outside the vendor set names a raw spelling primitive.
///
/// # This is a true zero, and that was MEASURED before it was asserted
///
/// The honest first step for a census like this is a RATCHET at today's count, not a
/// zero that is red on day one — a permanently-red test is not a guard, it is noise
/// that trains people to ignore a colour. So the count was taken first. Every call to
/// either primitive in the workspace today is inside a vendor crate: two
/// `ansi_double_quote_ident`, three `base64_standard`, five total, zero of them
/// outside `CRATES_THAT_MAY_SPELL`.
///
/// So there is nothing to ratchet. The assertion is a real zero because the tree is
/// really at zero, and no production code was touched to make that so. Had core been
/// calling one of them, this const would be that number with a note saying so.
#[test]
fn core_does_not_spell_a_vendors_bytes() {
    // `include_str!` is a compile-time dependency: editing either file rebuilds this
    // binary, so the anchor cannot silently drift out from under the census, and a
    // dangling path is a build error rather than a quiet zero.
    let home = include_str!("../../../zero-migrate-backend/src/spelling.rs");
    let former = include_str!("../../../zero-migrate-core/src/render/backends/mod.rs");
    assert_primitives_are_where_this_file_says_they_are(home, former);

    let crates_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .to_path_buf();

    let mut roots: Vec<PathBuf> = std::fs::read_dir(&crates_root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", crates_root.display()))
        .map(|e| e.expect("dir entry").path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    roots.sort();

    // FLOOR ONE: the walk. A scan over a DISCOVERED set fails OPEN — narrow the
    // discovery and it iterates nothing, finds nothing, and reports clean. This is
    // the floor the sibling census carries, and it is here for the same measured
    // reason: a crate-scoped walk in this tree went blind to a planted call in
    // `zero-migrate-ir`.
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

    // FOUND-SET READBACK, made permanent rather than done once by hand. A bare count
    // passes on N-1 crates plus a coincidence; naming the set says WHICH nine, so a
    // rename cannot move a crate from "denied" to "unnoticed" and a new crate cannot
    // arrive unclassified.
    let found: Vec<String> = roots
        .iter()
        .map(|p| {
            p.parent()
                .expect("<crates>/<name>/src")
                .file_name()
                .expect("crate dir name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut classified: Vec<&str> = CRATES_THAT_MAY_SPELL
        .iter()
        .chain(CRATES_THAT_MUST_NOT_SPELL)
        .copied()
        .collect();
    classified.sort_unstable();
    let unclassified: Vec<&String> = found
        .iter()
        .filter(|c| !classified.contains(&c.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "crate(s) {unclassified:?} exist under {} but appear on neither \
         CRATES_THAT_MAY_SPELL nor CRATES_THAT_MUST_NOT_SPELL.\n\
         \n\
         This list fails CLOSED on purpose, because the visibility it replaces did. \
         Decide which one it is and say why in the same commit that adds the crate: \
         a new VENDOR may spell its own bytes; anything else may not.",
        crates_root.display()
    );
    let missing: Vec<&&str> = classified
        .iter()
        .filter(|c| !found.contains(&(*c).to_string()))
        .collect();
    assert!(
        missing.is_empty(),
        "crate(s) {missing:?} are classified by this file but no longer exist under \
         {}. A classification for a crate that is gone is dead weight, and if it was \
         RENAMED then the new name is currently unclassified - fix both ends.",
        crates_root.display()
    );

    let mut by_crate: BTreeMap<String, Vec<SpellingCall>> = BTreeMap::new();
    let mut per_primitive: BTreeMap<&str, usize> = BTreeMap::new();
    for (root, krate) in roots.iter().zip(&found) {
        for file in crate::sqlite_trigger_quoting_reaches_postgres::rust_sources(root) {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
            for primitive in SPELLING_PRIMITIVES {
                let hits = count_spelling_calls(&text, primitive);
                if hits == 0 {
                    continue;
                }
                *per_primitive.entry(primitive).or_default() += hits;
                let rel = file
                    .strip_prefix(&crates_root)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                by_crate
                    .entry(krate.clone())
                    .or_default()
                    .push((rel, primitive, hits));
            }
        }
    }

    // FLOOR TWO: the needle. Break the match and every count above is zero, including
    // the violation counts - a blind census and a clean one are the same green.
    for (primitive, floor) in VENDOR_CALL_FLOOR {
        let seen = per_primitive.get(primitive).copied().unwrap_or(0);
        assert!(
            seen >= *floor,
            "the census found {seen} call(s) to `{primitive}` across all {} crate \
             `src` root(s), but at least {floor} are known to exist in the vendor \
             crates.\n\
             \n\
             This is the POSITIVE CONTROL and it has just failed, which means the \
             census is probably BLIND rather than the tree being clean: a needle that \
             matches nothing reports zero violations for the same reason it reports \
             zero calls. Check SPELLING_PRIMITIVES against \
             {SPELLING_HOME} first.\n\
             \n\
             If the needle is fine and a vendor genuinely stopped spelling those \
             bytes, lower this floor in the commit that removes the call and say \
             which - do not lower it to get green.",
            roots.len()
        );
    }

    let violations: Vec<(&String, &Vec<SpellingCall>)> = by_crate
        .iter()
        .filter(|(krate, _)| !CRATES_THAT_MAY_SPELL.contains(&krate.as_str()))
        .collect();
    assert!(
        violations.is_empty(),
        "a crate outside the vendor set calls a raw spelling primitive: \
         {violations:#?}\n\
         (paths are workspace-relative; each entry is file, primitive, count)\n\
         \n\
         These functions used to be `pub(in crate::render::backends)` and this would \
         have been a PRIVACY ERROR, not a test failure. The crate split made them \
         `pub` because `pub(in …)` cannot say \"these vendor crates and no other\", \
         and this census is what stands in for the compiler now.\n\
         \n\
         Two of the three vendors AGREE on both spellings, so the bytes this \
         produces are almost certainly CORRECT and no SQL-output test will ever \
         disagree with you. Correct bytes are not the property being defended - \
         NAMING A VENDOR is. Use a door instead:\n\
         \n\
           - EMIT for a named dialect -> \
         `zeroship_migrate_backend::dml::escape_quote_ident_for_backend(x, backend)`\n\
           - the PG-shaped NORMAL FORM -> \
         `zeroship_migrate_backend::dml::pg_canonical_ident(x)`\n\
         \n\
         Both resolve back to {SPELLING_HOME} through the registry, so the bytes are \
         unchanged and the vendor is on the record."
    );
}

/// The two-sided anchor: the census must be reasoning about the file the primitives
/// are actually in.
///
/// # Why a census needs one
///
/// Everything below the anchor is a NEEDLE over a DISCOVERED set, and that
/// construction has one silent failure mode that is not a wrong count: it is a right
/// count of an irrelevant tree. Rename the primitives, or move them to a fourth home,
/// and `SPELLING_PRIMITIVES` matches nothing anywhere — zero violations, zero calls,
/// green forever, guarding nothing. The needle floor catches the second half of that;
/// this catches the first, and catches it with a message that says where to look.
///
/// Both directions are checked on purpose, and the reasons are different. "They are
/// in `spelling.rs`" alone passes if a copy was left behind in `render/backends/mod.rs`
/// still spelling bytes for the engine — which is the exact shape of the thing being
/// guarded, a second physical home. "They are not in `render/backends/mod.rs`" alone
/// passes if they were simply deleted. The pair says they moved, once, to here.
fn assert_primitives_are_where_this_file_says_they_are(home: &str, former: &str) {
    for primitive in SPELLING_PRIMITIVES {
        assert!(
            home.contains(&format!("pub fn {primitive}(")),
            "{SPELLING_HOME} does not define `pub fn {primitive}(`, so this census is \
             hunting a name that is no longer the primitive's and its zero means \
             nothing.\n\
             \n\
             Either the primitive was RENAMED - update SPELLING_PRIMITIVES - or it \
             MOVED again, in which case repoint SPELLING_HOME and the `include_str!` \
             beside it, in the SAME commit that moves it. Do not delete this \
             assertion to get green: a census that hunts a name its subject has shed \
             is the one failure the rest of this file cannot see."
        );
        assert!(
            !former.contains(&format!("fn {primitive}(")),
            "{FORMER_SPELLING_HOME} still defines `fn {primitive}(`. These primitives \
             were moved OUT of the engine and into {SPELLING_HOME}; a copy left \
             behind is a SECOND physical home for byte-logic whose entire discipline \
             is having exactly one, it is reachable by every module in the engine, \
             and the census above will report a comfortable zero for it because it \
             only classifies CALLS - a definition in core is worse than a call from \
             core."
        );
    }
}

/// Count CALLS to `primitive` in `src`, excluding its own declaration and prose.
///
/// The `(` is what separates a call from a mention. It matters here more than in most
/// censuses: the engine's doc comments explain at length why the engine must not call
/// these, so the name appears in `zero-migrate-core/src` six times WITHOUT ever being
/// called. Counting mentions would make this file red on an unmodified tree and the
/// only way to green would be deleting the documentation that explains the rule.
///
/// Comment lines are dropped by prefix - `//` covers `///` and `//!`, and `*` covers
/// the continuation lines of the `/* … */` block in `zero-migrate-backend/src/dml.rs`
/// that narrates this exact history. A call cannot hide behind either prefix without
/// being commented out, and a commented-out call is not a call.
fn count_spelling_calls(src: &str, primitive: &str) -> usize {
    let needle = format!("{primitive}(");
    let declaration = format!("fn {primitive}(");
    src.lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("//") && !t.starts_with('*') && !t.contains(&declaration)
        })
        .map(|line| line.matches(&needle).count())
        .sum()
}
