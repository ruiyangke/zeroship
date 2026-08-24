//! The engine RECEIVES the shipping registry; it does not REACH for it.
//!
//! # The invariant, stated without reference to where anything lives
//!
//! **Only an entry point may name the composition. Everything else takes it as a
//! value — a parameter, or a field on a struct it already carries state in.**
//!
//! Resolving "which backend handles dialect `D`" needs two things: the dialect, and
//! the set to resolve it against. The dialect has always travelled as an argument.
//! The SET used to be a `const` any module could name, so every resolution in the
//! engine reached past its own caller to the one place that knows which vendor
//! crates exist. That reach is the reason the engine crate has to depend on all
//! three vendors, and it is what this file forbids.
//!
//! The header is phrased in terms of the RULE and not the current file layout,
//! because the layout is about to change underneath it. At the time of writing the
//! composition is `zero_migrate::render::backends::VENDORS`; that is a fact about
//! today, not the thing being asserted.
//!
//! # Why the compiler will NOT catch this for you
//!
//! Today it cannot: the composition is a `pub(crate) const`, so a new
//! `crate::render::backends::VENDORS` written anywhere under `src` compiles, emits
//! byte-identical SQL, and passes every behaviour test in the workspace. The engine
//! and the composition are still one crate.
//!
//! After the composition root moves out they will not be, and the same line becomes
//! a compile error — which is exactly why the failure is dangerous BEFORE then. Each
//! reach re-added between now and the split is a site the split has to discover the
//! hard way, one compile error at a time, in a commit that is supposed to be
//! mechanical. This file is what notices them while they are still cheap.
//!
//! # Three needles, because there are three ways back to a global
//!
//! 1. **NAME the composition.** `VENDORS`, matched as a whole word. The direct
//!    route, and the one every site that used to exist took. The test handle's own
//!    spelling is excluded — see [`composition_sites`] for why that is a spelling
//!    exclusion and not a region one.
//! 2. **CALL an accessor for it.** [`zero_migrate::shipping_vendors`] and
//!    [`zero_migrate::shipping_backends`] hand the composition to a HOST. They are
//!    `pub`, so engine code can call them too, and a call from inside `src` is the
//!    same reach wearing a function's clothes. It survives the crate split — the
//!    accessors move to the composing crate, but core could re-export or re-declare
//!    one — so this needle outlives the compile error the first one becomes.
//! 3. **DECLARE a second one.** A `static`/`const` of type `VendorSet`, or one
//!    parked in a `OnceLock`/`LazyLock`/`Mutex`/`RwLock`, is the composition
//!    re-created under a new name. This is the quiet path the registry module
//!    already argues against for the shipping set itself: a growable or
//!    host-installed registry trades a compile error for a runtime one. A census
//!    that only watched the existing name would not see it arrive under another.
//!
//! # What this file does NOT cover, said out loud
//!
//! It does not check that the value threaded to a door is the RIGHT set — nothing
//! textual can, and there is only one set to pass. It reads `src` only: the engine's
//! own `tests/` binaries are hosts and are entitled to compose, which is why
//! [`zero_migrate::shipping_vendors`] exists at all. [`is_code`] is a line-oriented
//! comment filter, not a Rust parser: it cannot see inside a block comment that
//! opens mid-line and does not try. It over-counts prose into code, never the
//! reverse, which is the safe direction for a census asserting a ZERO.
//!
//! # The floors, because a scan over a DISCOVERED set fails OPEN
//!
//! Three ways to be green for the wrong reason, and they are different blindnesses.
//!
//! 1. Narrow the walk and it iterates nothing, matches nothing, reports clean.
//!    [`WALK_ANCHORS`] and [`ENGINE_FILE_FLOOR`] bound WHICH files were seen and how
//!    many. The anchors are the half that holds.
//! 2. Break a needle and it reads every file and still matches nothing.
//!    [`ALLOWED`] is the positive control: each allowed site must still MATCH, with
//!    the identical matcher, or the needle has gone blind where it is known to be
//!    live.
//! 3. Delete the threading and the property becomes vacuous — an engine that
//!    resolves nothing at all names no composition either. [`CARRIED_FLOOR`] is the
//!    control for that one: the registry must still be VISIBLY carried, as
//!    parameters and fields, across the engine.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The composition, matched as a whole word.
const COMPOSITION: &str = "VENDORS";

/// The path prefix of the TEST handle for the composition, which [`composition_sites`]
/// does not count. See that function for why the spelling is excluded rather than the
/// `#[cfg(test)]` regions.
const TEST_HANDLE_PREFIX: &str = "crate::test_fixtures::";

/// The accessors that hand the composition to a host. A free call to either, from
/// inside the engine's own `src`, is the engine composing itself.
const ACCESSORS: &[&str] = &["shipping_vendors", "shipping_backends"];

/// The crate the accessor needle is vouched for against, relative to `crates/`.
///
/// A HOST, which is what the accessors exist for: it composes the engine and must
/// therefore call one. The control cannot live in `src`, because "nothing in `src`
/// calls one" is the property itself.
const ACCESSOR_CONTROL_CRATE: &str = "zero-migrate-node";

/// How many accessor calls the host must still make, so a broken needle is loud.
///
/// Blunt on purpose and set far below what the host holds: it only has to prove the
/// matcher is not blind. If the host legitimately stops composing through them —
/// because the composing crate handed it a set instead — repoint
/// [`ACCESSOR_CONTROL_CRATE`] at whatever does, deliberately, in that commit.
const ACCESSOR_CONTROL_FLOOR: usize = 5;

/// Where the composition may be named OR declared, relative to
/// `crates/zero-migrate/src`, and what each site is for.
///
/// These are the ENTRY POINTS in the invariant's sense: the place the set is
/// composed, and the places it is handed out. Every one of them is also a positive
/// control — see [`ALLOWED`]'s use below.
///
/// A site earns its place by being unable to take the set as an argument:
///
/// * `render/backends/mod.rs` composes it. Its own budget fold and its own unit
///   tests read it because they are the composition's tests.
/// * `lib.rs` is the crate's public surface, where a host asks what it got.
/// * `test_fixtures.rs` is `#[cfg(test)]` and answers the same question for core's
///   own unit tests, in one place, for the reason its header already gives about the
///   three dialect ids.
const ALLOWED: &[(&str, &str)] = &[
    (
        "render/backends/mod.rs",
        "composes the set, folds the identifier budget from it, and tests both",
    ),
    ("lib.rs", "hands the set to a host"),
    (
        "test_fixtures.rs",
        "hands the set to core's own `#[cfg(test)]` modules",
    ),
];

/// Files the walk MUST reach, relative to `crates/zero-migrate/src`.
///
/// The real defence against a census that fails open, because they bound WHICH
/// files were seen rather than how many. `lib.rs` is the walk root and cannot move;
/// `render/backends/mod.rs` is three levels down, so losing it means recursion
/// stopped descending. The other two are the largest consumers of the registry and
/// the first place a re-added reach would appear.
const WALK_ANCHORS: &[&str] = &[
    "lib.rs",
    "render/backends/mod.rs",
    "render/lower.rs",
    "model/validate.rs",
];

/// The walk's floor over the engine's `src` tree.
///
/// NEVER raise it to match a growing tree without saying why, and never lower it to
/// get green: unlike the vendor crates, this tree SHRINKS as extraction continues,
/// so a falling count can be the project working. Check [`WALK_ANCHORS`] first and
/// trust it over this number.
const ENGINE_FILE_FLOOR: usize = 40;

/// How many parameters and fields must still CARRY the registry.
///
/// The control for the third blindness. "Nothing names the composition" is trivially
/// true of an engine that resolves nothing, so the assertion is only worth having
/// alongside evidence that the resolution still happens and still travels by value.
/// Both spellings are counted because they are the two shapes the invariant permits
/// — a parameter, and a field on a struct that already carries state.
///
/// Blunt on purpose, and set well below what the tree holds: it only has to prove
/// the threading did not evaporate.
const CARRIED_FLOOR: usize = 200;

/// Declarations that would re-create the composition under a new name.
///
/// Matched as a `static`/`const` whose type names `VendorSet`, or any binding that
/// parks one behind interior mutability. The second half is what a host-installed
/// registry looks like, and it is the shape that trades a compile error for a
/// runtime one.
fn declares_a_second_composition(line: &str) -> bool {
    let t = line.trim_start();
    let after_keyword = ["static ", "const "].iter().find_map(|kw| {
        let at = t.find(kw)?;
        let head = t[..at].trim_end();
        // Only a leading visibility may precede the keyword, so `pub const fn` is
        // reached but a `const` buried mid-expression is not.
        (head.is_empty() || head == "pub" || (head.starts_with("pub(") && head.ends_with(')')))
            .then(|| t[at + kw.len()..].trim_start())
    });
    // `pub const fn shipping_vendors() -> VendorSet` is a DOOR, not a binding: it
    // hands out the one composition rather than declaring a second.
    if let Some(rest) = after_keyword {
        if !rest.starts_with("fn ") && line.contains("VendorSet") {
            return true;
        }
    }
    // The cell half is deliberately loose: any line that names both an interior
    // mutability wrapper and the registry type is a host-installed registry taking
    // shape, however the path to `VendorSet` happens to be spelled.
    line.contains("VendorSet")
        && [
            "OnceLock", "OnceCell", "LazyLock", "Lazy", "Mutex", "RwLock",
        ]
        .iter()
        .any(|cell| line.contains(cell))
}

/// Whether a source line is CODE rather than a comment.
///
/// The same line-oriented filter its sibling censuses use, with the same stated
/// limits: a `//`, `///`, `//!` line is prose, a line whose first non-space
/// character is `*` is a block-comment continuation, and everything else is code. A
/// trailing `// …` on a code line still counts, which is the safe direction here.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
}

/// Whether `c` can appear inside a Rust identifier.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How many times `line` names THE COMPOSITION.
///
/// The whole word, minus one spelling: `crate::test_fixtures::VENDORS`. That module
/// is `#[cfg(test)]` and its handle is `pub(crate)`, so PRODUCTION code cannot name
/// it — the compiler forbids exactly what this census would otherwise have to
/// forbid, and a `#[cfg(test)]` module reading it is the declared entry point doing
/// its job. Excluding the spelling rather than the test REGIONS is deliberate: a
/// `#[cfg(test)]` block is brace-matched, braces appear inside raw string literals
/// all over this tree, and a region scanner that loses its place fails silently in
/// whichever direction the mismatch happens to fall.
fn composition_sites(line: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(offset) = line[from..].find(COMPOSITION) {
        let start = from + offset;
        let end = start + COMPOSITION.len();
        from = end;
        let before = &line[..start];
        let clear_before = before.chars().last().is_none_or(|c| !is_ident_char(c));
        let clear_after = line[end..].chars().next().is_none_or(|c| !is_ident_char(c));
        if clear_before && clear_after && !before.ends_with(TEST_HANDLE_PREFIX) {
            count += 1;
        }
    }
    count
}

/// How many times `line` calls `name` as a FREE FUNCTION.
///
/// A hit needs `name` immediately followed by `(`, not preceded by an identifier
/// character and not preceded by `.`, so a same-named method on a carrier is not a
/// call to the accessor. A path-qualified `::name(` counts, which is the spelling a
/// re-added reach would actually use.
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

/// How many times `line` CARRIES the registry, as a parameter or a field.
fn carried_sites(line: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    for needle in ["vendors: VendorSet", "self.vendors"] {
        from = 0;
        while let Some(offset) = line[from..].find(needle) {
            from += offset + needle.len();
            count += 1;
        }
    }
    let _ = from;
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

#[test]
fn the_engine_receives_the_registry_instead_of_reaching_for_it() {
    let engine_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    assert!(
        engine_src.is_dir(),
        "the engine source root {} does not exist, so the census would walk nothing \
         and report clean",
        engine_src.display()
    );

    // ---- FLOOR ONE: the WALK. -------------------------------------------------
    //
    // The anchors run FIRST because they answer "did the walk reach the tree at
    // all", which is the failure the count below is too blunt to see.
    let files = src_files(&engine_src);
    let reached: BTreeSet<String> = files
        .iter()
        .filter_map(|p| p.strip_prefix(&engine_src).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for anchor in WALK_ANCHORS {
        assert!(
            reached.contains(*anchor),
            "the census walked {} engine files but never reached `{anchor}`, so the \
             walk is not seeing the tree it claims to census. Fix the walk. Do NOT \
             delete the anchor to get green — if `{anchor}` legitimately moved, point \
             the anchor at another file that cannot move and say which in the commit.",
            files.len()
        );
    }
    assert!(
        files.len() >= ENGINE_FILE_FLOOR,
        "the census walked only {} engine files, below the floor of \
         {ENGINE_FILE_FLOOR}. The anchors above PASSED, so the walk did reach the \
         root; a drop this large still means it stopped descending.",
        files.len()
    );

    // ---- Read once; every matcher below runs over the same lines. --------------
    let mut named_at: Vec<(String, usize, String)> = Vec::new();
    let mut accessor_at: Vec<(String, usize, String)> = Vec::new();
    let mut second_composition_at: Vec<(String, usize, String)> = Vec::new();
    let mut carried = 0usize;
    for path in &files {
        let rel = path
            .strip_prefix(&engine_src)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for (number, line) in text.lines().enumerate().filter(|(_, l)| is_code(l)) {
            carried += carried_sites(line);
            let site = || (rel.clone(), number + 1, line.trim().to_string());
            if composition_sites(line) > 0 {
                named_at.push(site());
            }
            if ACCESSORS.iter().any(|name| free_call_sites(line, name) > 0) {
                accessor_at.push(site());
            }
            if declares_a_second_composition(line) {
                second_composition_at.push(site());
            }
        }
    }

    // ---- FLOOR TWO: the NEEDLES, as positive controls. -------------------------
    //
    // The identical matchers, run where the answer must not be zero. A broken walk
    // or a broken matcher returns a confident ZERO, and every finding below would
    // then mean nothing.
    let mut blind: Vec<String> = Vec::new();
    for (file, purpose) in ALLOWED {
        if !named_at.iter().any(|(rel, _, _)| rel == file) {
            blind.push(format!(
                "  {COMPOSITION}: `{file}` — the site that {purpose} — no longer \
                 matches"
            ));
        }
    }
    // The accessor control cannot run inside `src` — the property IS that nothing
    // there calls one, and `lib.rs` only DEFINES them, which the matcher
    // deliberately does not count. So it runs where a call must exist: the Node host
    // composes through them on every verb, and that is the whole point of their
    // being `pub`.
    let host_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zero-migrate has a parent")
        .join(ACCESSOR_CONTROL_CRATE)
        .join("src");
    assert!(
        host_src.is_dir(),
        "the accessor control's source root {} does not exist, so the control would \
         walk nothing and vouch for nothing",
        host_src.display()
    );
    let host_calls: usize = src_files(&host_src)
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            text.lines()
                .filter(|l| is_code(l))
                .map(|line| {
                    ACCESSORS
                        .iter()
                        .map(|name| free_call_sites(line, name))
                        .sum::<usize>()
                })
                .sum::<usize>()
        })
        .sum();
    if host_calls < ACCESSOR_CONTROL_FLOOR {
        blind.push(format!(
            "  accessors: the host `{ACCESSOR_CONTROL_CRATE}` calls one on {host_calls} \
             code line(s), floor {ACCESSOR_CONTROL_FLOOR}"
        ));
    }
    if !second_composition_at
        .iter()
        .any(|(rel, _, _)| rel == "render/backends/mod.rs")
    {
        blind.push(
            "  second-composition: `render/backends/mod.rs` declares the ONE \
             composition and the matcher no longer sees it"
                .to_string(),
        );
    }
    assert!(
        blind.is_empty(),
        "the census has gone BLIND — these needles no longer match where they must:\n\
         {}\n\nEither the item genuinely moved (retire or repoint its entry \
         deliberately, in the commit that did it) or the matcher stopped working, in \
         which case every zero this file reports is meaningless.",
        blind.join("\n")
    );

    // ---- FLOOR THREE: the threading is still THERE. ----------------------------
    assert!(
        carried >= CARRIED_FLOOR,
        "the registry is carried at only {carried} parameter/field sites, below the \
         floor of {CARRIED_FLOOR}. An engine that resolves nothing names no \
         composition either, so the property below would be vacuously true. Do not \
         lower this floor: find out where the threading went."
    );

    // ---- THE PROPERTY. ---------------------------------------------------------
    let allowed: BTreeSet<&str> = ALLOWED.iter().map(|(file, _)| *file).collect();
    let mut violations: Vec<String> = Vec::new();
    for (rel, line, text) in &named_at {
        if !allowed.contains(rel.as_str()) {
            violations.push(format!("  [names the composition] {rel}:{line}: {text}"));
        }
    }
    for (rel, line, text) in &accessor_at {
        if rel != "lib.rs" {
            violations.push(format!(
                "  [calls a composition accessor] {rel}:{line}: {text}"
            ));
        }
    }
    for (rel, line, text) in &second_composition_at {
        if !allowed.contains(rel.as_str()) {
            violations.push(format!(
                "  [declares a second composition] {rel}:{line}: {text}"
            ));
        }
    }
    violations.sort();
    assert!(
        violations.is_empty(),
        "engine code reaches for the shipping registry instead of receiving it:\n{}\n\n\
         Only an entry point may name the composition. Everything else takes it as a \
         value — add a `vendors: VendorSet` parameter, or a field on the struct the \
         code already carries state in, and let the caller pass what it was given. \
         The reach compiles today because the engine and the composition are still \
         one crate; it is a compile error the moment they are not, and finding it \
         then costs a mechanical commit its mechanical-ness.",
        violations.join("\n")
    );
}
