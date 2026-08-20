//! An emitter that has been HANDED a backend must never ask the registry for one.
//!
//! `schema::backends::renderer(dialect) -> &dyn SchemaRenderer` is the schema tree's
//! vendor registry, and `docs/proposals/pluggable-backends.md` step 4 wants the three
//! vendors in their own crates. The blocker is core resolving a vendor BY DIALECT
//! from inside core, so the shape of each surviving call matters more than the count:
//!
//! * A **boundary** resolution — a `pub` surface whose callers hand in a
//!   `SqlDialect`, turning it into a backend exactly where it crosses in — survives
//!   the extraction. It becomes the facade's `register(..)` table.
//! * A **caller-fixed target** — `build_add_foreign_key` and `def_to_constraints`,
//!   `pg_quote_ident`-spelled throughout, and the tests that name the vendor under
//!   test — is not a decision at all. Naming a fixed target survives too.
//! * A **point-of-use** lookup — a core emitter deep in a call chain, holding a
//!   `dialect` parameter, reaching for the registry at the moment it needs one
//!   spelling — does NOT survive. It is core reaching for a vendor list core will no
//!   longer have.
//!
//! `schema/query.rs` had eight of the third kind, all reachable from the single entry
//! point `build_create_table_with_fks_for_dialect_scoped_statements`: one CREATE
//! TABLE emit went through the registry five separate times for the same dialect.
//! They are gone. The emitters take `backend: &'static dyn SchemaRenderer` where they
//! took `dialect: SqlDialect`, and the resolution happens once.
//!
//! # Why this needs a test rather than a comment
//!
//! The rule is invisible to every behaviour test, in the strongest sense: a
//! reintroduced `renderer(dialect)` inside an emitter emits BYTE-IDENTICAL SQL. It
//! has to — it resolves the same dialect through the same match to the same
//! `&'static` vendor. That is what makes the regression free to write and impossible
//! to notice.
//!
//! That is measured, not assumed. The commit that removed the eight lookups was run
//! against live PostgreSQL 18 and MySQL 8 with `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` and
//! zero skip banners, before and after: 37 suites, 3393 passed, 0 failed, 11 ignored,
//! both times. Every other test in the workspace is indifferent to which shape this
//! file is in. So the rule gets its own check or it has none — and note that the
//! indifference cuts both ways, which is why the RED for this test has to be
//! demonstrated by hand rather than inferred (reintroduce one lookup, watch it fire,
//! put it back).
//!
//! # It anchors on SHAPE, not on an occurrence count
//!
//! A raw count of `renderer(` in `schema/query.rs` would need editing every time
//! someone added a legitimate boundary, and a test that must be edited routinely gets
//! edited carelessly. The rule here keys off the CARRIER instead: whatever set of
//! functions takes a resolved backend, none of them may re-resolve one. A seventh
//! emitter adopting the carrier needs no edit to this file and is covered the moment
//! it compiles.
//!
//! # What this does NOT catch
//!
//! It reads `schema/query.rs` only, and it sees only the two states a function can be
//! in THERE. It is blind to:
//!
//! * A core emitter that still takes `dialect: SqlDialect` and looks a backend up.
//!   Such a function is not a carrier, so it is not scanned. The census clause below
//!   is the partial guard — converting a carrier back drops the count and fails —
//!   but a brand-new dialect-taking emitter with a fresh lookup is invisible here.
//! * The cross-stack edge. These emitters spell identifiers through
//!   `quote_ident_for_dialect`, which forwards to
//!   `render::dml::escape_quote_ident_for_dialect` and resolves through the OTHER
//!   registry, `render::backends`. A carrier reaching a DML vendor that way is one
//!   forwarder from anything this file can see. `schema/backends/mod.rs` carries the
//!   reason that forwarder must stay single.
//! * The five `backend.dialect()` derivations that remain in these bodies. They are
//!   deliberate and are NOT lookups in disguise — identifier quoting, FK-action
//!   folding and the SQLite scope test are core NORMALIZATION keyed by dialect, which
//!   the backend-boundary rule keeps in core, parameterized. Shrinking that number is
//!   different work, and this test says nothing about it.
//!
//! So a green run means "no function holding a backend asked for another one". Read
//! it as exactly that.

/// Every function in `schema/query.rs` that receives a resolved backend, and the
/// question asked of each: does its body reach for the registry anyway?
///
/// # How the regions are cut, and why not by brace matching
///
/// A naive brace counter would be wrong on this file. These bodies are full of
/// `format!("CREATE TABLE IF NOT EXISTS {} (\n  {}\n)")` and `format!("CHECK ({col} >=
/// {min})")`, so braces inside string literals are everywhere; a counter that cannot
/// tell a literal from code walks off the end of the function it meant to read. The
/// cut here is rustfmt's own invariant instead: a top-level item's closing brace is
/// the line `}` at column zero, and nothing nested can produce one. So a region runs
/// from a column-zero `fn` line to the next bare `}` line.
///
/// That cut also excludes the doc comment, which is load-bearing. Doc comments in
/// this file discuss `renderer(..)` by name in prose; counting those would make the
/// test red for a paragraph, which is the classic way a source-scanning check gets
/// weakened until it means nothing.
///
/// # The census clause is the boundary self-check, not decoration
///
/// The scan above fails OPEN. If the region cutter broke — a rustfmt change, a
/// signature respelled, a typo in the needle — it would find zero carriers, iterate
/// over nothing, and report green, which is the failure direction that matters. So
/// the count of carriers found is asserted too. Six is what the conversion produced;
/// the assertion is a FLOOR, so a seventh emitter adopting the carrier passes
/// silently while a broken scanner and a carrier reverted to `dialect: SqlDialect`
/// both go red.
///
/// # Two ways this goes red that are not defects
///
/// Deleting a carrier emitter as dead code, and folding two of them together, both
/// drop the census below six. Both are fine edits; the red is a question — "is this
/// emitter gone, or did it stop carrying?" — and answering it out loud is the whole
/// point of the floor. Lower it, in the same commit, with the reason.
#[test]
fn a_schema_emitter_holding_a_backend_never_resolves_another() {
    const SRC: &str = include_str!("../../src/schema/query.rs");
    const CARRIER: &str = "backend: &'static dyn SchemaRenderer";
    const LOOKUP: &str = "renderer(";

    // The six the conversion produced. Named for the failure message only — the scan
    // finds carriers structurally, so this list never gates which ones are checked.
    const KNOWN: [&str; 6] = [
        "build_injected_columns",
        "injected_column_type",
        "render_injected_default",
        "build_fk_clause",
        "field_to_column_for_dialect",
        "def_to_constraints_for_dialect",
    ];

    let lines: Vec<&str> = SRC.lines().collect();
    let mut carriers: Vec<(&str, Vec<&str>)> = Vec::new();

    for (start, line) in lines.iter().enumerate() {
        // A top-level item, i.e. column zero. Anything indented is inside the
        // `#[cfg(test)] mod` at the bottom of the file, whose tests name the dialect
        // they are testing and are caller-fixed targets by construction.
        let Some(after_fn) = line
            .strip_prefix("fn ")
            .or_else(|| line.strip_prefix("pub fn "))
            .or_else(|| line.strip_prefix("pub(crate) fn "))
        else {
            continue;
        };
        let name = after_fn
            .split(['(', '<'])
            .next()
            .unwrap_or(after_fn)
            .trim_end();

        let end = lines[start..]
            .iter()
            .position(|l| *l == "}")
            .map_or(lines.len(), |offset| start + offset);
        let region = &lines[start..=end.min(lines.len() - 1)];

        if region.iter().any(|l| l.contains(CARRIER)) {
            let offenders: Vec<&str> = region
                .iter()
                .filter(|l| l.contains(LOOKUP))
                .map(|l| l.trim())
                .collect();
            carriers.push((name, offenders));
        }
    }

    for (name, offenders) in &carriers {
        assert!(
            offenders.is_empty(),
            "`schema::query::{name}` is handed a resolved backend and then resolves \
             another one:\n  {}\n\
             \n\
             That is a POINT-OF-USE lookup, the one shape that does not survive the \
             per-vendor crate extraction: it is core reaching for a vendor list core \
             will no longer have. It also emits byte-identical SQL, so no behaviour \
             test can see it — this is the only check that can.\n\
             \n\
             Use the `backend` this function was given. If a DIALECT is what is \
             actually needed (identifier quoting, FK-action folding, the SQLite scope \
             test — core normalization, not vendor spelling), derive it with \
             `backend.dialect()` instead. If a genuinely different vendor is needed \
             here, that is a boundary and it belongs at the entry point, not in this \
             body. See `schema/backends/mod.rs` for who is allowed to call the \
             registry.",
            offenders.join("\n  ")
        );
    }

    let found: Vec<&str> = carriers.iter().map(|(name, _)| *name).collect();
    assert!(
        found.len() >= KNOWN.len(),
        "found {} backend-carrying emitter(s) in schema/query.rs, expected at least \
         {}: {found:?}\n\
         \n\
         This is the boundary self-check on the scan above, and it fires in two very \
         different situations. Either the scan itself broke — the region cutter, or \
         the `{CARRIER}` needle — in which case the loop above iterated over nothing \
         and its green meant nothing; or an emitter stopped carrying a backend and \
         went back to taking a `dialect: SqlDialect`, which is the regression this \
         file exists to catch. Check which before editing the floor. The six the \
         conversion produced were {KNOWN:?}; a legitimate drop (an emitter deleted or \
         two folded together) lowers the floor in the same commit, with the reason.",
        found.len(),
        KNOWN.len(),
    );
}
