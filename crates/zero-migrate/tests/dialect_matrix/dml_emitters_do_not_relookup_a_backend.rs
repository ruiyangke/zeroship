//! The DML counterpart of `schema_emitters_do_not_relookup_a_backend`: a DML
//! emitter that has been HANDED a backend must never ask the registry for one.
//!
//! `render::backends::renderer(dialect) -> &'static dyn DmlRenderer` (re-exported as
//! `render::renderer::renderer`, so the two spellings are ONE function) is the DML
//! tree's vendor registry. `docs/proposals/pluggable-backends.md` step 4 wants
//! `render::backends::{postgres,sqlite,mysql}` in their own crates, and the blocker
//! is core resolving a vendor BY DIALECT from inside core. The taxonomy is the same
//! one the schema file spells out:
//!
//! * A **boundary** — a door whose callers hand in a `&zero_migrate::DialectId`, which turns it
//!   into a backend ONCE and hands that backend down. These survive extraction as
//!   the facade's `register(..)` table.
//! * A **caller-fixed target** — naming a vendor you already are, or already meant.
//!   Survives too.
//! * A **point-of-use** lookup — a core emitter deep in a call chain, holding a
//!   `dialect` parameter, reaching for the registry at the moment it needs one
//!   spelling. Does NOT survive.
//!
//! # The census this file was written against
//!
//! `git grep 'renderer::renderer(\|backends::renderer('` over `src/`, minus the
//! registry's own definition and re-export, is EIGHT sites, and after the commit
//! that added this file NONE of them is point-of-use:
//!
//! | site | kind |
//! |------|------|
//! | `dml::escape_quote_ident_for_dialect` | door for its `apply::` / `schema::` callers — but ALSO the far end of a live vendor -> core -> vendor round trip, see the blind-spot section below |
//! | `dml::inline_string_literal` | door (callers in `declarative`, `lower`) |
//! | `dml::inline_literal` | door, resolves then uses the backend |
//! | `dml::render_expr_inline_with_col` | door into the inline walk |
//! | `dml::BindCtx::new` | door, parks the backend in the struct |
//! | `lower::IrAuthor::new` | door, same |
//! | `lower::render_view_query` | door into the view-query walk (`apply::drift` calls it) |
//! | `value_format` | deliberate CORE-ASKS-VENDOR, see that module |
//!
//! Two point-of-use sites were removed to get there —
//! `dml::render_in_list_elem_portable` and `lower::render_table_ref` — and a third,
//! `dml::placeholder`, was deleted outright as a `pub` fn with zero callers anywhere
//! in `crates/`, `sdks/` or `packages/`. Two more `renderer(` sites exist in `src/`
//! and are excluded above because they are inside `#[cfg(test)] mod tests`: the
//! pointer-identity checks that assert `BindCtx` and `IrAuthor` carry the SAME
//! `&'static` the registry hands out. They are also invisible to the scan below,
//! which only reads column-zero items.
//!
//! # The instance this file was written for, which the schema side did not have
//!
//! The DML tree had a strictly worse variant of the third kind: a **vendor -> core
//! -> vendor cycle**. `backends/sqlite.rs::render_in_list` and
//! `backends/mysql.rs::render_in_list` each called
//! `dml::render_in_list_elem_portable(elem, DIALECT)`, handing core their own
//! dialect const; core then resolved that dialect back through the registry to reach
//! the very backend that had called in. After step 4 that is `zero-migrate-sqlite`
//! depending on core depending on `zero-migrate-sqlite` — a dependency cycle in
//! exactly the shape the crate split exists to remove.
//!
//! `backends/postgres.rs` was already free of it, but NOT because PostgreSQL is
//! special: it calls the lookup-free `render_in_list_elem_pg`, which can be
//! lookup-free only because every PG in-list spelling is fixed (`'x'::text`, a
//! verbatim decimal). SQLite quotes decimals and MySQL emits strings as hex, so the
//! portable helper genuinely needs a vendor — it just needs the CALLER's vendor,
//! which the caller already is. The fix was to pass it: the helper takes
//! `backend: &dyn DmlRenderer` and both backends hand it `self`.
//!
//! # Why this needs a test rather than a comment
//!
//! Identical to the schema case, and it is the reason the cycle survived while the
//! analogous schema problem was found and fixed: a reintroduced `renderer(dialect)`
//! inside an emitter emits BYTE-IDENTICAL SQL. It has to — it resolves the same
//! dialect through the same match to the same `&'static` vendor. That is what makes
//! the regression free to write and impossible to notice, and it is why the RED for
//! this test has to be demonstrated by hand rather than inferred.
//!
//! # It anchors on SHAPE, not on an occurrence count
//!
//! A raw count of `renderer(` in `render/dml.rs` would need editing every time
//! someone added a legitimate door, and a test that must be edited routinely gets
//! edited carelessly. The rule keys off the CARRIER instead: whatever set of
//! functions takes a resolved `&dyn DmlRenderer`, none of them may re-resolve one. A
//! new emitter adopting the carrier needs no edit to this file and is covered the
//! moment it compiles.
//!
//! # The carrier here is a DIFFERENT KIND from the schema side's, on purpose
//!
//! The schema tripwire keys on `backend: &'static dyn SchemaRenderer` — one exact
//! parameter spelling, because every schema carrier holds a `&'static`. The DML tree
//! has BOTH lifetimes and they mean different things: the function parameters in
//! `render/dml.rs` and `render/lower.rs` are `&dyn DmlRenderer` (a borrow threaded
//! down a walk), while `BindCtx::backend` and `IrAuthor::backend` are
//! `&'static dyn DmlRenderer` (a resolved vendor parked in a struct, because the
//! registry hands back borrows of statics). Pinning the schema side's exact string
//! here would silently miss every one of the function parameters.
//!
//! It went further than the lifetime. An earlier draft of this file used
//! `"dyn DmlRenderer"` as the type half of the needle and MISSED two carriers in
//! `render/lower.rs` that were spelled `&dyn crate::render::renderer::DmlRenderer` —
//! and the same blindness had made a `git grep 'dyn DmlRenderer'` report that
//! `render/lower.rs` contained no carriers at all, which is how they went unnoticed
//! while the file was being designed. The needle is now the pair `backend:` plus the
//! bare type NAME on one line, which tolerates any path prefix and both lifetimes.
//! The census floor is what surfaced the miss; that is the whole argument for having
//! one, arriving in the needle rather than in the region cutter.
//!
//! # What this does NOT catch
//!
//! It reads `render/dml.rs` and `render/lower.rs`, and it sees only the two states a
//! function can be in THERE.
//!
//! * **The identifier seam, which used to be the same cycle and is now CLOSED.**
//!   All three backend modules used to call `dml::quote_bare_ident_for_dialect(..,
//!   DIALECT)` and `dml::quote_ident_checked_for_dialect(.., DIALECT)`, which
//!   forwarded to `dml::escape_quote_ident_for_dialect`, i.e.
//!   `renderer(dialect).quote_ident(ident)` — vendor -> core -> the SAME vendor,
//!   structurally identical to the in-list cycle above, and PostgreSQL was not
//!   exempt. Those doors were converted to take a resolved backend, so today the
//!   vendors call `quote_bare_ident_for_backend` (23 sites),
//!   `quote_ident_checked_for_backend` (9) and `quote_ident_for_backend` (8), and
//!   no vendor crate contains a `*_for_dialect(` CALL at all. Grep for the bare name
//!   rather than the call form and you get six hits across the three crates; every
//!   one is prose describing the seam as it used to be, which is the difference
//!   between counting mentions and counting calls. The helpers now live in
//!   `zero_migrate_backend::dml`, which resolves no registry at all, so the round
//!   trip has no way back into core.
//!
//!   This bullet is kept rather than deleted because the conversion is what the
//!   census floor below is measuring, and a reader who finds the floor dropping
//!   needs to know these carriers are the reason it is as high as it is. It is NOT
//!   an outstanding decision any more; do not re-add a `_for_dialect` door here.
//! * A core emitter that still takes `dialect: &zero_migrate::DialectId` and looks a backend up.
//!   Such a function is not a carrier, so it is not scanned. The census floor is the
//!   partial guard — converting a carrier back drops the count and fails — but a
//!   brand-new dialect-taking emitter with a fresh lookup is invisible here.
//! * A lookup ONE HOP away. `render_view_op` carries a backend and calls
//!   `render_view_query`, which resolves one. That is legitimate — `render_view_query`
//!   is `pub(crate)` and `apply::drift` calls it holding only a `&zero_migrate::DialectId`, so it
//!   is a genuine door — but the scan cannot tell that from a laundered point-of-use
//!   lookup, because it never leaves the region it is reading.
//! * `value_format.rs:472` resolves a backend from a dialect and is meant to. Its
//!   module records why ("CORE-ASKS-VENDOR, and it stays that way"); it is drift
//!   comparison, not emission, and it is not scanned.
//!
//! So a green run means "no function holding a DML backend asked for another one, in
//! these two files". Read it as exactly that.

/// Every function in `render/dml.rs` and `render/lower.rs` that receives a resolved
/// DML backend, and the question asked of each: does its body reach for the registry
/// anyway?
///
/// # How the regions are cut, and why not by brace matching
///
/// A naive brace counter would be wrong on these files. Both are several thousand
/// lines of `format!("({expr} {op} ({}))")` and `format!("{} JOIN {} ON {}")`, so
/// braces inside string literals are everywhere; a counter that cannot tell a
/// literal from code walks off the end of the function it meant to read. The
/// cut here is rustfmt's own invariant instead: a top-level item's closing brace is
/// the line `}` at column zero, and nothing nested can produce one. So a region runs
/// from a column-zero `fn` line to the next bare `}` line.
///
/// That cut also excludes the doc comment, which is load-bearing here in a way it was
/// not even on the schema side: the module headers of both files discuss `renderer(..)`
/// by name at length, and `BindCtx::backend`'s doc names the very doors this test is
/// about. Counting those would make the test red for a paragraph, which is the classic
/// way a source-scanning check gets weakened until it means nothing. Column-zero `fn`
/// also excludes the `#[cfg(test)] mod` at the bottom of each file, whose two
/// pointer-identity tests deliberately call `renderer(dialect)` to compare against.
///
/// # The census clause is the boundary self-check, not decoration
///
/// The scan above fails OPEN. If the region cutter broke — a rustfmt change, a
/// signature respelled, a typo in the needle — it would find zero carriers, iterate
/// over nothing, and report green, which is the failure direction that matters. So
/// the count of carriers found is asserted too. The assertion is a FLOOR, so a new
/// emitter adopting the carrier passes silently while a broken scanner and a carrier
/// reverted to `dialect: &zero_migrate::DialectId` both go red.
///
/// # Two ways this goes red that are not defects
///
/// Deleting a carrier emitter as dead code, and folding two of them together, both
/// drop the census below the floor. Both are fine edits; the red is a question — "is
/// this emitter gone, or did it stop carrying?" — and answering it out loud is the
/// whole point of the floor. Lower it, in the same commit, with the reason.
#[test]
fn a_dml_emitter_holding_a_backend_never_resolves_another() {
    // THE SUBJECT MOVED. Fourteen of the nineteen carriers below were in
    // `zero-migrate/src/render/dml.rs`; the crate extraction took that module into
    // `zero-migrate-backend`, leaving a thin engine-side shim at the old path whose
    // functions take a `dialect` and resolve ONE backend at the door. This census
    // scans all three, so it still sees every carrier AND it now watches the shim
    // for a carrier that starts re-resolving.
    const BACKEND_DML: &str = include_str!("../../../zero-migrate-backend/src/dml.rs");
    const ENGINE_DML: &str = include_str!("../../src/render/dml.rs");
    const LOWER: &str = include_str!("../../src/render/lower.rs");
    const CARRIER_NAME: &str = "backend:";
    // Deliberately NOT `"dyn DmlRenderer"`. Writing this test found two carriers in
    // `render/lower.rs` spelled `&dyn crate::render::renderer::DmlRenderer` that a
    // `dyn DmlRenderer` needle silently skipped — the exact fail-open the census
    // floor exists for, arriving in the needle rather than in the region cutter.
    // Both were normalized to the short spelling, but the needle must not depend on
    // that: matching the TYPE NAME alone tolerates any path prefix and both
    // lifetimes, and still cannot match a `use` line (no `backend:`) or a return
    // type (no `backend:`).
    const CARRIER_TYPE: &str = "DmlRenderer";
    const LOOKUP: &str = "renderer(";

    // Named for the failure message only — the scan finds carriers structurally, so
    // this list never gates which ones are checked. It is here so that a census drop
    // can be diffed against what the conversion produced rather than guessed at.
    const KNOWN: &[&str] = &[
        // `render/dml.rs`, threading a backend down the bound and inline walks.
        "zero-migrate-backend/src/dml.rs::qualify_table",
        "zero-migrate-backend/src/dml.rs::in_list_text_literal",
        "zero-migrate-backend/src/dml.rs::render_in_list_elem_portable",
        "zero-migrate-backend/src/dml.rs::render_in_list",
        "zero-migrate-backend/src/dml.rs::render_regex_match_node",
        "zero-migrate-backend/src/dml.rs::render_extract",
        "zero-migrate-backend/src/dml.rs::render_binop",
        "zero-migrate-backend/src/dml.rs::render_distinct_from",
        "zero-migrate-backend/src/dml.rs::render_scalar_fn_call",
        "zero-migrate-backend/src/dml.rs::cast_target_sql",
        "zero-migrate-backend/src/dml.rs::render_concat_ws",
        "zero-migrate-backend/src/dml.rs::render_split_part",
        "zero-migrate-backend/src/dml.rs::render_unary",
        "zero-migrate-backend/src/dml.rs::render_expr_inline_walk_for_backend",
        // `render/lower.rs`: the two that already carried, plus the view-query
        // subtree below its `render_view_query` door.
        "zero-migrate/src/render/lower.rs::trigger_inverse_from_history",
        // Added when the vendor-op surface went behind
        // `DmlRenderer::render_vendor_op`: this one used to call
        // `crate::render::vendor::render_vendor_op` — a re-export of one vendor
        // crate — where its trigger sibling directly above had always taken the
        // resolved backend. It takes one now too.
        "zero-migrate/src/render/lower.rs::vendor_inverse_from_history",
        "zero-migrate/src/render/lower.rs::render_view_op",
        "zero-migrate/src/render/lower.rs::render_select_ast",
        "zero-migrate/src/render/lower.rs::render_join",
        "zero-migrate/src/render/lower.rs::render_table_ref",
    ];

    let mut carriers: Vec<(&str, &str, Vec<&str>)> = Vec::new();

    for (file, src) in [
        ("zero-migrate-backend/src/dml.rs", BACKEND_DML),
        ("zero-migrate/src/render/dml.rs", ENGINE_DML),
        ("zero-migrate/src/render/lower.rs", LOWER),
    ] {
        let lines: Vec<&str> = src.lines().collect();

        for (start, line) in lines.iter().enumerate() {
            // A top-level item, i.e. column zero. Anything indented is inside an
            // `impl` or inside the `#[cfg(test)] mod` at the bottom of the file.
            let Some(after_fn) = line
                .strip_prefix("fn ")
                .or_else(|| line.strip_prefix("pub fn "))
                .or_else(|| line.strip_prefix("pub(crate) fn "))
                .or_else(|| line.strip_prefix("pub(super) fn "))
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

            // The needle is the PAIR, on one line: a parameter named `backend`
            // whose type is a `dyn DmlRenderer` of either lifetime. `use` lines,
            // doc lines and return types cannot satisfy both halves.
            let carries = region
                .iter()
                .any(|l| l.contains(CARRIER_NAME) && l.contains(CARRIER_TYPE));
            if !carries {
                continue;
            }

            let offenders: Vec<&str> = region
                .iter()
                .filter(|l| l.contains(LOOKUP))
                .map(|l| l.trim())
                .collect();
            carriers.push((file, name, offenders));
        }
    }

    for (file, name, offenders) in &carriers {
        assert!(
            offenders.is_empty(),
            "`{file}::{name}` is handed a resolved DML backend and then resolves \
             another one:\n  {}\n\
             \n\
             That is a POINT-OF-USE lookup, the one shape that does not survive the \
             per-vendor crate extraction. In the DML tree it is worse than in the \
             schema tree, because the emitters here are reachable FROM the backend \
             modules: a backend that hands core its own `DIALECT` and has core \
             resolve it back is `zero-migrate-sqlite` -> core -> \
             `zero-migrate-sqlite`, a dependency cycle in the exact shape step 4 \
             exists to remove. It also emits byte-identical SQL, so no behaviour \
             test can see it — this is the only check that can.\n\
             \n\
             Use the `backend` this function was given. `DmlRenderer` NOW has a \
             `dialect()` accessor — the crate split added it, because the spelling \
             helpers had to stop taking a `&zero_migrate::DialectId` and the capability and \
             leg-selection questions they ask still need one. So a genuine DIALECT \
             need (drift comparison, capability folding, normalization keyed by \
             dialect: core decisions, not vendor spelling) is `backend.dialect()`, \
             not a second registry lookup. This message used to say the accessor did \
             not exist and that the dialect had to be threaded alongside; that is \
             past tense. If a \
             genuinely DIFFERENT vendor is needed, that is a boundary and it belongs \
             at the door, not in this body. See `render/backends/mod.rs` for who is \
             allowed to call the registry.",
            offenders.join("\n  ")
        );
    }

    let found: Vec<String> = carriers
        .iter()
        .map(|(file, name, _)| format!("{file}::{name}"))
        .collect();
    assert!(
        found.len() >= KNOWN.len(),
        "found {} backend-carrying DML emitter(s), expected at least {}: {found:#?}\n\
         \n\
         This is the boundary self-check on the scan above, and it fires in two very \
         different situations. Either the scan itself broke — the region cutter, or \
         the `{CARRIER_NAME}` / `{CARRIER_TYPE}` needle — in which case the loop \
         above iterated over nothing and its green meant nothing; or an emitter \
         stopped carrying a backend and went back to taking a `dialect: &zero_migrate::DialectId`, \
         which is the regression this file exists to catch. Check WHICH before \
         editing the floor. The set at the time of writing was {KNOWN:#?}; a \
         legitimate drop (an emitter deleted or two folded together) lowers the \
         floor in the same commit, with the reason.",
        found.len(),
        KNOWN.len(),
    );
}
