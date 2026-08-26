//! The REPLACEMENT for the `pub(crate)` that today forbids a vendor from resolving a
//! vendor, written BEFORE the move that will dissolve it.
//!
//! # The invariant, stated without reference to where anything lives
//!
//! **The engine RESOLVES; a vendor ANSWERS FOR ITSELF.**
//!
//! Asking "which backend handles dialect `D`" is the registry's question and the
//! engine is the only thing entitled to ask it. A backend already knows which vendor
//! it is: it reaches its own spellings through `self`, through its own modules, and
//! through the `&'static` surfaces its own `BackendVendor` literal registers. A
//! vendor crate that looks a vendor up by dialect — even its OWN dialect — has
//! reached around the registry, and the registry is the entire reason the vendor
//! crates exist. A fourth backend is supposed to be a `[dependencies]` line plus one
//! entry in `SHIPPING`; that promise is worth nothing if vendors can resolve each
//! other behind it.
//!
//! The header is deliberately phrased in terms of the RULE and not the current file
//! layout, because the layout is about to change underneath it. At the time of
//! writing the subject items are `pub(crate)` in `zero_migrate::render::backends`.
//! That is a fact about today, not the thing being asserted.
//!
//! # What is about to be lost, exactly
//!
//! `crates/zero-migrate/src/apply/backend/{postgres,sqlite,mysql}/` is vendor code
//! that still lives in the engine crate, and it is being extracted into the vendor
//! crates. `pub(crate)` DOES NOT SURVIVE A CRATE BOUNDARY. There is no modifier that
//! means "visible to `zero-migrate` but not to `zero-migrate-postgres`" — `pub(crate)`
//! would hide these items from the engine, their intended caller, and `pub` lets
//! every vendor in. The type system genuinely cannot express this rule, which is why
//! it has to be written down as a test instead of being asserted in prose and
//! forgotten.
//!
//! Today, that same extracted code calls the resolvers directly. Measured at the
//! commit this file was written against, `apply/backend/` held
//! `crate::render::backends::guard_for` (4 sites, PostgreSQL),
//! `::schema_renderer` (3 sites, MySQL and SQLite), `::stored_ddl` (1 site, SQLite's
//! `mod.rs` forwarder that 6 more call sites read through), `::vendor` (2 sites,
//! PostgreSQL's `catalog_fold`) and `SqliteSequencePolicy` (2 imports, 9 uses). Every
//! one of them becomes a vendor resolving a vendor the instant the file moves.
//!
//! Two of the three vendors have since moved, and this file is what made them answer
//! the question rather than carry it. MySQL's `::schema_renderer` sites became
//! `crate::schema::RENDERER`; SQLite's became the same, and its `::stored_ddl`
//! forwarder — plus all six call sites reading through it — became
//! `crate::stored_ddl::PARSER`, the spelling prescribed further down this header.
//! `SqliteSequencePolicy` is now named only by the crate that owns it, which the
//! owner exemption below already permitted. What is left to move is
//! `apply/backend/postgres/`, and with it `guard_for` and `::vendor`.
//!
//! # Why the compiler will NOT catch this for you
//!
//! It is tempting to think it will, because no vendor crate depends on `zero-migrate`
//! and none can: the engine depends on all three, so the edge back would be a cycle.
//! A moved file that still writes `crate::render::backends::guard_for` therefore does
//! not compile.
//!
//! That is exactly why the failure is dangerous. The loud path is not the one anybody
//! takes. The quiet path is to PROMOTE the resolver into `zero-migrate-backend`,
//! which every vendor already depends on, and which already exposes
//! `VendorSet::get`, `VendorSet::as_slice` and `VendorSet::dialects` as `pub`. All
//! that is missing there is a `VendorSet` value to call them on, and the move
//! supplies a motive to publish one. The result compiles cleanly, emits byte-identical
//! SQL, passes every behaviour test in the workspace, and silently converts the crate
//! split into three crates that can all see each other. This file is what notices.
//!
//! # Not the same question as the two `_do_not_relookup_a_backend` files
//!
//! `schema_emitters_do_not_relookup_a_backend` and
//! `dml_emitters_do_not_relookup_a_backend` ask whether a CORE emitter that was
//! handed a backend goes back to the registry anyway. They read three named engine
//! files and key off the carrier parameter. This file asks whether a VENDOR resolves
//! at all, reads every vendor crate, and does not care about carriers. Both survive;
//! neither subsumes the other.
//!
//! # What the needle is, and why it is not the bare ident
//!
//! Four of the resolver names are also names a vendor crate legitimately writes, and
//! a census that fired on them would be red on day one for entirely correct code.
//! Measured on this tree: `stored_ddl` appears 18 times across the vendor crates and
//! `schema_renderer` 8 times, every one of them either the `SchemaRenderer` trait
//! method a vendor IMPLEMENTS (`fn stored_ddl(&self)`), a receiver call on a backend
//! the vendor already holds (`self.schema_renderer()`), SQLite's own
//! `crate::stored_ddl::PARSER`, or a local binding. None of those is a resolution.
//!
//! So the needle is the CALL SHAPE, which is what actually distinguishes the two:
//!
//! * `self.stored_ddl()` / `backend.stored_ddl()` — a receiver call. The vendor is
//!   asking something it already has. NOT a resolution.
//! * `fn stored_ddl(&self) -> …` — a definition. NOT a resolution.
//! * `crate::stored_ddl::PARSER`, `mod stored_ddl;` — a module path. NOT a call.
//! * `stored_ddl(&DIALECT)`, `backends::stored_ddl(d)`, `registry::vendor(id)` — a
//!   FREE FUNCTION applied to a dialect. That is the registry question, wherever the
//!   function lives, and that is what [`free_call_sites`] matches.
//!
//! A path-qualified free call counts as a violation even when the path is local
//! (`super::stored_ddl()`). That is deliberate rather than an accident of the matcher:
//! a vendor reaches its own parser as `crate::stored_ddl::PARSER` or through
//! `SchemaRenderer::stored_ddl(&self)`, so a free function of the registry's name,
//! sitting in a vendor crate, is precisely the shape that hides a resolution behind a
//! re-point. If the extraction wants SQLite's `mod.rs` forwarder to come along, it
//! must come along under a name that is not the registry's.
//!
//! [`SqliteSequencePolicy`] is a TYPE, not a call, so it gets the other matcher:
//! plain word occurrence, scoped to the vendor crates that do NOT own it.
//! `zero-migrate-sqlite` declaring and exporting its own policy enum is correct and
//! is this file's positive control for that needle. `zero-migrate-postgres` or
//! `zero-migrate-mysql` naming it would be one vendor reaching into another's plan
//! vocabulary, which is the same failure by a different route.
//!
//! # What this file does NOT cover, said out loud
//!
//! Only CALLS. A `use` line that imports a resolver and never calls it is invisible
//! here; the workspace's `-D warnings` clippy gate catches that as an unused import,
//! so it is covered, just not by this file. [`is_code`] is a line-oriented comment
//! filter, not a Rust parser: it cannot see inside a block comment that opens
//! mid-line and does not try. It over-counts prose into code, never the reverse,
//! which is the safe direction for a census asserting a ZERO.
//!
//! `render::backends` holds thirteen `pub(crate)` items. This file covers eleven of
//! them. The two it does not are named here rather than left to be discovered:
//!
//! * `generated_ident_max_bytes` is a byte budget, not a resolution. A vendor reading
//!   it would be leaking one backend's declared identifier limit into another, which
//!   is a real rule but a DIFFERENT one, and a census whose subject is two rules is a
//!   census nobody can read.
//! * `VENDOR` — the re-export of SQLite's own `BackendVendor` — is unmatchable, and
//!   that is a property of the name rather than a gap that can be closed. Every
//!   vendor crate declares its own `pub static VENDOR`, so the needle would fire on
//!   all three on day one for the one thing each of them is REQUIRED to do. It is the
//!   same call this file's template made about `ColumnSnapshot::new`: a needle nobody
//!   can keep green is worse than an absence that is written down. `VENDORS` — the
//!   registry SET, which no vendor crate has any business naming — is covered and is
//!   the part of that pair with teeth.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Two ways to be green for the wrong reason, and they are different blindnesses.
//! Narrow the walk and it iterates nothing, matches nothing, reports clean. Break the
//! needle and it reads every file and still matches nothing. So there are two floors,
//! and neither is a count alone:
//!
//! 1. [`WALK_ANCHORS`] — files the walk MUST reach, plus [`VENDOR_FILE_FLOOR`] under
//!    them. The anchors are the half that holds: a count only bounds HOW MANY files
//!    were found, and a walk narrowed to skip one directory can still clear a count.
//! 2. [`RESOLVER_CONTROL_FLOOR`] and [`OWNER_CONTROL_FLOOR`] — positive controls that
//!    run the IDENTICAL matchers where the answer must not be zero. The floor is
//!    stated PER RESOLVER rather than as a total, because a total lets one popular
//!    name carry seven dead ones: `renderer` alone would clear any aggregate floor
//!    this file could sensibly set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The registry resolvers: free functions that turn a dialect into a vendor's
/// surface. Core's, and only core's.
///
/// The first four are the briefed group — the items whose `pub(crate)` in
/// `render::backends` is doing the forbidding today. The rest are their siblings in
/// the same module, resolving the same way through the same `vendor()` lookup, and
/// they are included because covering four of eight identical items would leave a
/// hole nobody could see. It cost nothing: the call-shape matcher scores ZERO on all
/// eight across all three vendor crates today.
///
/// `VENDORS` is the `VendorSet` itself and is matched as a word, not a call — naming
/// it at all, in a vendor crate, is the purest form of the bypass.
const REGISTRY_RESOLVERS: &[&str] = &[
    // The briefed group.
    "vendor",
    "schema_renderer",
    "stored_ddl",
    "guard_for",
    // Same module, same lookup, same rule.
    "renderer",
    "value_format_renderer",
    "value_format_renderers",
    "ddl_emitter",
    "advisor",
    "index_coverage",
];

/// The registry SET, matched as a word rather than a call.
const REGISTRY_SET: &str = "VENDORS";

/// Vendor-owned types that core re-exports, and the one crate allowed to name each.
///
/// `SqliteSequencePolicy` is a SQLite plan-vocabulary enum that core re-exports from
/// its registry module so the engine never reaches into a vendor crate directly. Its
/// owner naming it is the design; any other vendor naming it is one backend reading
/// another's plan vocabulary.
const VENDOR_OWNED_TYPES: &[(&str, &str)] = &[("SqliteSequencePolicy", "zero-migrate-sqlite")];

/// The vendor crates, relative to the workspace `crates/` directory.
const VENDOR_CRATES: &[&str] = &[
    "zero-migrate-postgres",
    "zero-migrate-sqlite",
    "zero-migrate-mysql",
];

/// Files the walk MUST reach, relative to `crates/`. The real defence against a
/// census that fails open, because they bound WHICH files were seen rather than how
/// many.
///
/// Each vendor crate's `lib.rs` sits at that crate's walk root, so losing one means
/// the root itself was wrong — and a crate root is the one file in a crate that
/// cannot move. `analysis/mod.rs` is a directory down, so losing it means recursion
/// stopped descending; it is the libpg_query-backed analyzer, four files, permanently
/// PostgreSQL's.
///
/// Pick replacements only from files that cannot move. Do NOT anchor inside
/// `apply/backend/` or on anything arriving from it — that code is mid-extraction and
/// an anchor there would rot on landing.
const WALK_ANCHORS: &[&str] = &[
    "zero-migrate-postgres/src/lib.rs",
    "zero-migrate-postgres/src/analysis/mod.rs",
    "zero-migrate-sqlite/src/lib.rs",
    "zero-migrate-mysql/src/lib.rs",
];

/// Files the POSITIVE-CONTROL walk must reach, relative to `crates/zero-migrate-core/src`.
///
/// The same pair the sibling census `core_names_no_vendor_crate.rs` uses and for the
/// same reasons: `lib.rs` is the walk root, and `render/backends/mod.rs` is three
/// levels down and is the registry composition that census calls PERMANENT.
const ENGINE_WALK_ANCHORS: &[&str] = &["lib.rs", "render/backends/mod.rs"];

/// The walk's floor across all three vendor crates. They hold 79 `.rs` files under
/// `src` (31 + 26 + 22), up from 67: the PostgreSQL execution half landed in
/// `zero-migrate-postgres/src/backend/` — ten files, plus that crate's own
/// `recording.rs` and `test_fixtures.rs` — the same way the SQLite and MySQL halves
/// landed before it. That was the last one; there is no vendor code left in the
/// engine to arrive here.
///
/// Raise it deliberately as the vendors grow, and this is one of those times: 55 was
/// set against 67 and would no longer notice losing an entire ten-file directory. 70
/// would: 79 minus that directory is 69.
///
/// NEVER lower it to get green: unlike the engine, these crates are the destination of
/// the extraction, so a falling vendor file count is not the project working — it is
/// the walk losing a root. Check [`WALK_ANCHORS`] first and trust it over this number.
const VENDOR_FILE_FLOOR: usize = 70;

/// The needle's floor, PER RESOLVER: how many free-call sites the identical matcher
/// must still find in the engine.
///
/// Stated per name on purpose. An aggregate would let `renderer` — 77 sites on its
/// own — mask every other needle going blind at once. Measured when written:
/// `vendor` 39, `renderer` 77, `schema_renderer` 33, `value_format_renderer` 16,
/// `value_format_renderers` 9, `stored_ddl` 7, `ddl_emitter` 3, `guard_for` 11,
/// `advisor` 3, `index_coverage` 2, `VENDORS` 24. The floors are set below those with
/// room to churn; they only have to prove the matcher is not blind, so they are blunt
/// on purpose.
///
/// A resolver whose engine count reaches zero has genuinely left core — retire its
/// entry deliberately in that commit, rather than dropping the floor to get green.
///
/// `stored_ddl` was retired exactly that way, at 4. Its only engine caller was
/// SQLite's execution half, asking this registry which parser handles SQLite from
/// inside the SQLite backend; that half is `zero-migrate-sqlite` now and names
/// `crate::stored_ddl::PARSER` directly, so `render::backends::stored_ddl` had zero
/// callers and was DELETED. The name stays in [`REGISTRY_RESOLVERS`] above — the rule
/// it states is still the rule, and it now also guards against the resolver being
/// reintroduced — but there is no engine site left for it to control, and the ten
/// remaining controls are what vouch for the matcher.
const RESOLVER_CONTROL_FLOOR: &[(&str, usize)] = &[
    ("vendor", 20),
    ("schema_renderer", 15),
    ("guard_for", 5),
    ("renderer", 40),
    ("value_format_renderer", 8),
    ("value_format_renderers", 4),
    ("ddl_emitter", 2),
    ("advisor", 2),
    ("index_coverage", 2),
    (REGISTRY_SET, 12),
];

/// The type needle's positive control: how many code lines in the OWNING crate must
/// still name each vendor-owned type. `zero-migrate-sqlite` declares
/// `SqliteSequencePolicy` and re-exports it, which is two lines and is correct.
const OWNER_CONTROL_FLOOR: usize = 2;

/// Whether a source line is CODE rather than a comment.
///
/// The same line-oriented filter its sibling censuses use, with the same stated
/// limits: a `//`, `///`, `//!` line is prose, a line whose first non-space character
/// is `*` is a block-comment continuation, and everything else is code. A trailing
/// `// …` on a code line still counts, which is the safe direction here.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Whether `c` can appear inside a Rust identifier.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How many times `line` calls `name` as a FREE FUNCTION.
///
/// A hit needs `name` immediately followed by `(`, not preceded by an identifier
/// character (so `render_vendor_op(` is not `vendor(`), not preceded by `.` (so
/// `self.schema_renderer()` is the vendor asking itself, not the registry), and not
/// preceded by the keyword `fn` (so implementing a same-named trait method is not a
/// call to it). Everything else — including a path-qualified `::name(` — counts.
///
/// It reads one line at a time and does not know about strings or macros. A receiver
/// split across lines still lands its `.` immediately before the name, which is the
/// case that matters.
fn free_call_sites(line: &str, name: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(offset) = line[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        from = end;
        if !line[end..].starts_with('(') {
            continue;
        }
        let before = &line[..start];
        if before
            .chars()
            .last()
            .is_some_and(|c| c == '.' || is_ident_char(c))
        {
            continue;
        }
        let head = before.trim_end();
        let defines = head.ends_with("fn")
            && head[..head.len() - "fn".len()]
                .chars()
                .last()
                .is_none_or(|c| !is_ident_char(c));
        if !defines {
            count += 1;
        }
    }
    count
}

/// How many times `line` names `word` as a whole identifier.
fn word_sites(line: &str, word: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(offset) = line[from..].find(word) {
        let start = from + offset;
        let end = start + word.len();
        from = end;
        let clear_before = line[..start]
            .chars()
            .last()
            .is_none_or(|c| !is_ident_char(c));
        let clear_after = line[end..].chars().next().is_none_or(|c| !is_ident_char(c));
        if clear_before && clear_after {
            count += 1;
        }
    }
    count
}

/// Every `.rs` file under `root`, sorted.
fn src_files(root: &Path) -> Vec<PathBuf> {
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

/// Code lines of `files` that resolve through the registry, keyed by needle name.
///
/// `owner` is the vendor crate being scanned, or `None` for the engine control; it is
/// what lets `zero-migrate-sqlite` name its own `SqliteSequencePolicy` without being
/// a finding.
fn resolutions(
    files: &[PathBuf],
    base: &Path,
    owner: Option<&str>,
) -> BTreeMap<String, Vec<String>> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in files {
        let rel = path
            .strip_prefix(base)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for (number, line) in text.lines().enumerate().filter(|(_, l)| is_code(l)) {
            let mut hit = |needle: &str, n: usize| {
                if n > 0 {
                    found.entry(needle.to_string()).or_default().push(format!(
                        "{rel}:{}: {}",
                        number + 1,
                        line.trim()
                    ));
                }
            };
            for needle in REGISTRY_RESOLVERS {
                hit(needle, free_call_sites(line, needle));
            }
            hit(REGISTRY_SET, word_sites(line, REGISTRY_SET));
            for (ty, ty_owner) in VENDOR_OWNED_TYPES {
                if owner != Some(ty_owner) {
                    hit(ty, word_sites(line, ty));
                }
            }
        }
    }
    found
}

#[test]
fn no_vendor_crate_resolves_a_vendor() {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .to_path_buf();

    // ---- FLOOR ONE: the WALK. -------------------------------------------------
    //
    // The anchors run FIRST because they answer "did the walk reach the tree at
    // all", which is the failure the count below is too blunt to see.
    let mut vendor_files: Vec<PathBuf> = Vec::new();
    let mut per_crate: Vec<(&str, Vec<PathBuf>)> = Vec::new();
    for vendor in VENDOR_CRATES {
        let src = crates.join(vendor).join("src");
        assert!(
            src.is_dir(),
            "vendor source root {} does not exist, so the census would walk nothing \
             and report clean",
            src.display()
        );
        let files = src_files(&src);
        vendor_files.extend(files.iter().cloned());
        per_crate.push((vendor, files));
    }

    let reached: std::collections::BTreeSet<String> = vendor_files
        .iter()
        .filter_map(|p| p.strip_prefix(&crates).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in WALK_ANCHORS {
        assert!(
            reached.contains(*anchor),
            "the census walked {} vendor files but never reached `{anchor}`, so the \
             walk is not seeing the tree it claims to census. Fix the walk. Do NOT \
             delete the anchor to get green — if `{anchor}` legitimately moved, point \
             the anchor at another file that cannot move and say which in the commit.",
            vendor_files.len()
        );
    }
    assert!(
        vendor_files.len() >= VENDOR_FILE_FLOOR,
        "the census walked only {} vendor files, below the floor of \
         {VENDOR_FILE_FLOOR}. The anchors above PASSED, so the walk did reach each \
         crate root; a drop this large in crates that the extraction only ADDS to \
         still means the walk stopped descending. Fix the walk, do not lower the floor.",
        vendor_files.len()
    );

    // ---- FLOOR TWO: the NEEDLE, as positive controls. -------------------------
    //
    // The identical matchers, run where the answer must not be zero. A broken
    // pathspec or a broken matcher returns a confident ZERO, and every vendor zero
    // below would then mean nothing.
    let engine_src = crates.join("zero-migrate-core").join("src");
    let engine_files = src_files(&engine_src);
    let engine_reached: std::collections::BTreeSet<String> = engine_files
        .iter()
        .filter_map(|p| p.strip_prefix(&engine_src).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in ENGINE_WALK_ANCHORS {
        assert!(
            engine_reached.contains(*anchor),
            "the positive control walked {} engine files but never reached \
             `{anchor}`, so the control itself is blind and cannot vouch for the \
             needles.",
            engine_files.len()
        );
    }

    let engine = resolutions(&engine_files, &engine_src, None);
    let mut blind: Vec<String> = Vec::new();
    for (needle, floor) in RESOLVER_CONTROL_FLOOR {
        let n = engine.get(*needle).map_or(0, Vec::len);
        if n < *floor {
            blind.push(format!(
                "  {needle}: the positive control found {n} engine site(s), floor {floor}"
            ));
        }
    }
    for (ty, owner) in VENDOR_OWNED_TYPES {
        let src = crates.join(owner).join("src");
        let files = src_files(&src);
        let n = resolutions(&files, &src, None).get(*ty).map_or(0, Vec::len);
        if n < OWNER_CONTROL_FLOOR {
            blind.push(format!(
                "  {ty}: its OWNER {owner} names it on {n} code line(s), floor \
                 {OWNER_CONTROL_FLOOR}"
            ));
        }
    }
    assert!(
        blind.is_empty(),
        "the census has gone BLIND — these needles no longer match where they must:\n\
         {}\n\nEither the item genuinely left core (retire its entry deliberately, in \
         the commit that did it) or the matcher stopped working, in which case every \
         vendor zero this file reports is meaningless. Do not lower a floor to get \
         green.",
        blind.join("\n")
    );

    // ---- THE PROPERTY. --------------------------------------------------------
    let mut violations: Vec<String> = Vec::new();
    for (vendor, files) in &per_crate {
        for (needle, sites) in resolutions(files, &crates, Some(vendor)) {
            for site in sites {
                violations.push(format!("  [{needle}] {site}"));
            }
        }
    }
    violations.sort();
    assert!(
        violations.is_empty(),
        "a vendor crate resolves a vendor:\n{}\n\nThe engine RESOLVES; a vendor \
         ANSWERS FOR ITSELF. A backend reaches its own spellings through `self`, its \
         own modules, and the `&'static` surfaces its own `BackendVendor` registers — \
         never by asking the registry which backend handles a dialect, not even its \
         own. These resolvers were `pub(crate)` in `zero_migrate::render::backends` \
         before `apply/backend/` was extracted into the vendor crates; the crate \
         boundary is what dissolved that modifier, and this census is what stands in \
         for it.",
        violations.join("\n")
    );
}
