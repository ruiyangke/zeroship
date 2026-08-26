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
//! because the layout changed underneath it exactly as it warned it would. The
//! composition is `zeroship_migrate::shipping_vendors()` now, in the crate that names the
//! vendors; the engine is `zero-migrate-core` and can no longer see it.
//!
//! # THE SPLIT HAPPENED, AND IT COST NOTHING — WHICH IS THIS FILE'S RESULT
//!
//! When this file was written the composition was a `pub(crate) const` in the engine's
//! own crate, so a new `crate::render::backends::VENDORS` written anywhere under `src`
//! compiled, emitted byte-identical SQL, and passed every behaviour test in the
//! workspace. The prediction recorded here was that each such reach re-added before
//! the split would become a compile error the split had to discover the hard way.
//!
//! MEASURED at the split: the engine's manifest dropped all three vendor
//! `[dependencies]` and the workspace compiled with ZERO errors. Not one resolution
//! site had grown back. That is what this census was for, and it is the only evidence
//! that it worked.
//!
//! # Why the compiler still will NOT catch this for you
//!
//! For PRODUCTION code it now does, and that half of the rule has been handed over:
//! `zero-migrate-core` declares no vendor dependency, so it cannot compose a set.
//!
//! What is left is the `#[cfg(test)]` half. The vendor crates ARE dev-dependencies of
//! the engine — several hundred unit tests need a real `VendorSet` to hand the
//! resolution doors — so a second composition written in any `#[cfg(test)]` module
//! under `src` compiles clean. [`ALLOWED`] holds that to ONE file, and the third
//! needle below is what sees a new one arrive under another name.
//!
//! # Three needles, because there are three ways back to a global
//!
//! 1. **NAME the composition.** `VENDORS`, matched as a whole word. The direct
//!    route, and the one every site that used to exist took. The test handle's own
//!    spelling is excluded — see [`composition_sites`] for why that is a spelling
//!    exclusion and not a region one.
//! 2. **CALL an accessor for it.** [`zeroship_migrate::shipping_vendors`] and
//!    [`zeroship_migrate::shipping_backends`] hand the composition to a HOST. They are
//!    `pub`, so a `#[cfg(test)]` module in the engine could call one through the
//!    dev-dependency edge, and a call from inside `src` is the same reach wearing a
//!    function's clothes. It SURVIVED the crate split, exactly as this entry predicted
//!    it would: the accessors moved to the composing crate, but core could re-export or
//!    re-declare one and nothing but this needle would say so.
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
//! textual can. There are TWO sets in the workspace now, and that is new: the shipping
//! one this crate composes, and the `#[cfg(test)]` one `test_fixtures.rs` composes for
//! the engine's own unit tests. [`the_two_compositions_list_the_same_vendors`] is what
//! keeps them from disagreeing, and it is a source comparison because the test set is
//! `#[cfg(test)] pub(crate)` and no integration test can hold both values at once.
//!
//! It reads `src` only: this crate's `tests/` binaries are HOSTS and are entitled to
//! compose, which is why [`zeroship_migrate::shipping_vendors`] exists at all. [`is_code`]
//! is a line-oriented comment filter, not a Rust parser: it cannot see inside a block
//! comment that opens mid-line and does not try. It over-counts prose into code, never
//! the reverse, which is the safe direction for a census asserting a ZERO.
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
/// `crates/zero-migrate-core/src`, and what each site is for.
///
/// ONE ENTRY, and the fall from three to one is what the crate split bought.
///
/// The list used to hold `render/backends/mod.rs` (which composed the set and folded
/// the identifier budget from it) and `lib.rs` (which handed it to a host). Both left
/// the engine with the composition: the shipping list is
/// `crates/zero-migrate/src/lib.rs` now, and so are `shipping_vendors` and
/// `shipping_backends`. Neither entry was lowered — both became unrepresentable,
/// because the engine cannot name a vendor crate it does not depend on.
///
/// What remains is `test_fixtures.rs`, which is `#[cfg(test)]` and composes a set for
/// core's own unit tests from the three vendor crates reached through
/// `[dev-dependencies]`. That is the one edge Cargo does not close, it is deliberate,
/// and it is the reason this file did not retire with the two entries above it.
///
/// It is also a positive control — see [`ALLOWED`]'s use below.
const ALLOWED: &[(&str, &str)] = &[(
    "test_fixtures.rs",
    "composes the `#[cfg(test)]` set and hands it to core's own unit tests",
)];

/// Files the walk MUST reach, relative to `crates/zero-migrate-core/src`.
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

/// The ENGINE's source root, which is `zero-migrate-core/src` and no longer this
/// crate's own `src`.
///
/// This crate is the COMPOSITION now: its `src` is one file that names the three
/// vendors on purpose. Walking it would give this census a one-file tree in which
/// every finding is by design — a green that means nothing. The floors below caught
/// exactly that when the split landed, which is why they exist.
fn engine_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .join("zero-migrate-core")
        .join("src")
}

#[test]
fn the_engine_receives_the_registry_instead_of_reaching_for_it() {
    let engine_src = engine_src();
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
        .any(|(rel, _, _)| rel == "test_fixtures.rs")
    {
        blind.push(
            "  second-composition: `test_fixtures.rs` declares the engine's only \
             `VendorSet` binding and the matcher no longer sees it"
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
    // No exemption: the accessors are `zero-migrate`'s now, and the engine cannot even
    // name that crate. The rule used to be "only `lib.rs`, which DEFINES them"; both
    // definitions left with the composition, so the target is a flat zero.
    for (rel, line, text) in &accessor_at {
        violations.push(format!(
            "  [calls a composition accessor] {rel}:{line}: {text}"
        ));
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

/// The engine's `#[cfg(test)]` vendor set lists the SAME vendors, in the same order, as
/// the shipping composition.
///
/// # Why this test exists, and why it is textual
///
/// The crate split left the workspace with TWO `VendorSet` compositions where it had
/// one. The shipping list is `crates/zero-migrate/src/lib.rs`. The other is
/// `crates/zero-migrate-core/src/test_fixtures.rs`, which composes a set for the
/// engine's own unit tests from the same three crates reached through
/// `[dev-dependencies]` — because the engine cannot see the shipping one, and a
/// hand-rolled fake would make several hundred unit tests assert against a double
/// instead of against the backends that ship.
///
/// Two copies of a list drift silently. That is the standing hazard the sibling
/// censuses record about recorders and normal forms, and it now applies here.
///
/// It cannot be a value comparison: `test_fixtures::VENDORS` is `#[cfg(test)]
/// pub(crate)`, so no integration test can hold both. Making it `pub` to enable the
/// comparison would publish a test double on the engine's API — a worse trade than a
/// source scan, and one this file's whole subject argues against.
///
/// So it compares the two composition sites as TEXT: the ordered sequence of vendor
/// crate idents each one names. A backend added to one list and not the other changes
/// that sequence, which is the drift worth catching.
///
/// WHAT IT DOES NOT SEE, said rather than implied: the shipping site names each vendor
/// crate at its `const <NAME>_VENDOR` line and then builds `SHIPPING` out of those
/// CONSTS, so this reads the declaration order, not the array's. Reordering the array
/// alone is invisible here. That is a smaller fact than it sounds — resolution is a
/// lookup by dialect id, so array order changes only the order a few union answers
/// (`reserved_catalog_prefixes`, `targets_declaring`) are spelled in — but it is a real
/// gap and not a claim this test can make.
#[test]
fn the_two_compositions_list_the_same_vendors() {
    /// The shipping composition, and the engine's test composition.
    const SHIPPING_SITE: &str = "zero-migrate/src/lib.rs";
    const FIXTURE_SITE: &str = "zero-migrate-core/src/test_fixtures.rs";

    /// The idents to look for. The ORDER is not asserted from this list — it is read
    /// out of each file, so a reordering in one and not the other is a red.
    const VENDOR_CRATES: &[&str] = &[
        "zeroship_migrate_mysql",
        "zeroship_migrate_postgres",
        "zeroship_migrate_sqlite",
    ];

    /// The vendor crate idents `text` names, in the order its CODE lines name them.
    fn vendors_in_order(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines().filter(|l| is_code(l)) {
            let mut hits: Vec<(usize, &str)> = VENDOR_CRATES
                .iter()
                .filter_map(|c| line.find(c).map(|at| (at, *c)))
                .collect();
            hits.sort_unstable();
            out.extend(hits.into_iter().map(|(_, c)| c.to_string()));
        }
        out
    }

    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives at <workspace>/crates/<name>")
        .to_path_buf();
    let read = |rel: &str| {
        let path = crates.join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    };

    let shipping = vendors_in_order(&read(SHIPPING_SITE));
    let fixture = vendors_in_order(&read(FIXTURE_SITE));

    // ---- The FLOOR. Two empty lists are equal, and would mean the matcher went blind
    // ---- on both sites at once rather than that the compositions agree.
    assert_eq!(
        shipping.len(),
        VENDOR_CRATES.len(),
        "the shipping composition {SHIPPING_SITE} names {} vendor crate(s); the \
         workspace ships {}. Either a backend was added or removed — update this floor \
         in that commit — or the matcher stopped seeing the composition, in which case \
         the comparison below is vacuous.",
        shipping.len(),
        VENDOR_CRATES.len()
    );

    assert_eq!(
        shipping, fixture,
        "the shipping composition ({SHIPPING_SITE}) and the engine's `#[cfg(test)]` \
         composition ({FIXTURE_SITE}) list different vendors, or list them in a \
         different order. The engine's unit tests would then be resolving against a set \
         that is not the one that ships, and every dialect answer they assert would be \
         about a build nobody deploys. Add the backend to BOTH, in the same order, in \
         the same commit."
    );
}
