//! The one-dialect-literal rule of the vendor crates, enforced rather than
//! documented.
//!
//! # The rule, and how it tightened
//!
//! It used to be per-MODULE: a backend module names its own dialect exactly ONCE,
//! as its `const DIALECT: DialectId = POSTGRES;`, and names no other dialect at all;
//! everything else in the module reads `DIALECT`. The name on the right of that
//! `=` came from `zeroship-migrate-ir`, the NEUTRAL vocabulary crate, which declared
//! `POSTGRES`, `SQLITE` and `MYSQL` for three vendors it does not own — while its own
//! module doc said a backend "declares its own — `DialectId::new(\"duckdb\")` —
//! without editing this crate".
//!
//! The ids moved into the vendors. Each crate now declares its id ONCE, in its
//! `lib.rs`, and every module — render half and execution half — reads
//! `crate::DIALECT`. So the rule is per-CRATE and it is strictly stronger: there is
//! no longer a module that names a dialect at all, and the count this file pins is a
//! count of DECLARATIONS rather than of imports.
//!
//! That also fixed this census's needle, which the move would otherwise have killed
//! silently. Its old matcher looked for the identifier tokens `POSTGRES` / `SQLITE` /
//! `MYSQL`. After the move there are none anywhere in any vendor crate, so every
//! assertion it made would have gone on passing over a tree it could no longer see —
//! the exact failure mode the floors below exist for. The needles are the
//! DECLARATION (`DialectId::new(`) and the vendor CRATE IDENT
//! (`zeroship_migrate_postgres`) now, and each has a positive control that is permanent
//! rather than one of the things being ratcheted to zero.
//!
//! # Why it needs a test
//!
//! The rule is invisible to every behaviour test. A PostgreSQL backend module that
//! reaches for another vendor's dialect still emits correct PostgreSQL today; what it
//! costs is the extraction, and no assertion about emitted SQL can see that. So the
//! rule gets its own check or it has none, which is what it had.
//!
//! WHY THIS FILE AND NOT A `#[cfg(test)] mod tests` IN a vendor crate. The check
//! reads the vendor crates as TEXT, and it must see all three at once to say
//! "foreign". A unit test inside `zeroship-migrate-postgres` cannot: the whole point of
//! the split is that it does not depend on its siblings. This binary is also where
//! the other "what does each dialect declare" checks already live.
//!
//! # What this does NOT catch, and it is the important half
//!
//! This sees EXPLICIT coupling only — a foreign vendor named inside a vendor crate.
//! It is blind to a backend that reaches another vendor's spelling THROUGH a core or
//! contract helper that hard-codes a dialect, because the offending literal then
//! lives in that helper and no grep of the backend can see it.
//!
//! That is not hypothetical. When the per-module version of this test was written it
//! was TRUE OF THIS TREE while the test passed: `render::dml::quote_ident` and
//! `quote_ident_checked` both pinned the PostgreSQL dialect constant (and
//! `quote_bare_ident` delegated to the first), so every identifier the SQLite backend
//! emitted was quoted by the POSTGRESQL renderer — correct only because both vendors
//! spell an identifier `"x"`.
//!
//! That instance is FIXED, through `SqliteDmlRenderer::quote_ident`, proven by
//! neutering the PostgreSQL method: the SQLite-only `sqlite_engine` binary went from
//! 148 passed / 7 failed to 155 / 0 over the same 155 tests, so the dependency is
//! gone rather than merely re-covered.
//!
//! AN EQUIVALENT INSTANCE SURVIVED ONE HOP AWAY, and the test was equally blind to
//! it: `render_sqlite_trigger_op`, then living in `render::lower`, called the
//! PostgreSQL-pinned `dml::quote_bare_ident` six times, and the SQLite renderer
//! delegated its trigger rendering there. That one is now fixed too — those six say
//! `quote_bare_ident_for_dialect(.., DIALECT)`, the pinned wrapper they used no longer
//! exists, and the three functions moved into `zeroship-migrate-sqlite/src/dml.rs`, which
//! is why this test can see them at all.
//!
//! HOW it was proven is the part worth keeping, because the obvious proof LIED. The
//! neuter above works by watching a suite go red; run against the trigger path it did
//! not. `sqlite_engine` stayed at 156 passed / 0 failed with `PostgresDmlRenderer::
//! quote_ident` neutered AT THE COMMIT WHERE THE REACH WAS STILL LIVE — that binary
//! never renders a trigger, so its green meant nothing, and only running the
//! before-case as a control exposed it. `sqlite_trigger_render_bytes.rs` was written
//! to be an instrument that can see the path, and with it the same neuter fails
//! before the fix and passes after.
//!
//! So both examples are WORKED ones now. The CLASS is not gone: it is invisible to
//! this test by construction, and both instances were found only because someone went
//! looking. Do not read either fix as evidence the class is gone — and do not trust a
//! neuter that stays green without first checking it can go red. The second grep it
//! asks for — over the CORE helpers a moved branch calls — has a test next door in
//! `sqlite_trigger_quoting_reaches_postgres.rs`, which walks every `.rs` under `src/`
//! and pins the pinned-wrapper call count at ZERO.

use std::path::{Path, PathBuf};

/// The vendor crates, the id each one IS, and the exact `lib.rs` lines that declare
/// it.
///
/// The declaration is spelled out rather than derived, because a derived string is
/// how a needle stops matching without anyone noticing: derive it from the id and a
/// change in the declaration's SHAPE (a rename of `NAME`, a move to a different
/// constructor) still produces a string that matches nothing, and the census then
/// reports a confident zero.
const VENDORS: &[Vendor] = &[
    Vendor {
        krate: "zeroship-migrate-postgres",
        ident: "zeroship_migrate_postgres",
        name_decl: "const NAME: &str = \"postgres\";",
    },
    Vendor {
        krate: "zeroship-migrate-sqlite",
        ident: "zeroship_migrate_sqlite",
        name_decl: "const NAME: &str = \"sqlite\";",
    },
    Vendor {
        krate: "zeroship-migrate-mysql",
        ident: "zeroship_migrate_mysql",
        name_decl: "const NAME: &str = \"mysql\";",
    },
];

/// A shipping vendor crate, as this census reads it.
struct Vendor {
    /// Directory name under `crates/`.
    krate: &'static str,
    /// The crate's Rust ident, which is what a FOREIGN reach would spell.
    ident: &'static str,
    /// The `lib.rs` line that spells the id string, verbatim.
    name_decl: &'static str,
}

/// The one line in the workspace that may build a `DialectId` from a vendor's own
/// name, spelled verbatim so a change to its shape is a red rather than a silent
/// zero.
const DIALECT_DECL: &str = "pub const DIALECT: DialectId = DialectId::new(NAME);";

/// The modules that must READ the crate's id rather than hold one, with the exact
/// import each carries.
///
/// Four renderer families — DML, schema typing, DDL emission, value-format
/// normalization — times three vendors, plus each execution half's `backend/mod.rs`.
/// SQLite's execution half aliases the import because that subtree also carries
/// `rusqlite`'s `SQLITE_*` flag names and a lone `DIALECT` reads ambiguously beside
/// them.
///
/// A module that legitimately stops needing the id, and an import that changes shape,
/// both fail here. Both are exactly the edits the rule exists to make an author
/// justify out loud.
const IDENTITY_READERS: &[(&str, &str)] = &[
    ("zeroship-migrate-postgres/src/dml.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-sqlite/src/dml.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-mysql/src/dml.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-postgres/src/schema.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-sqlite/src/schema.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-mysql/src/schema.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-postgres/src/ddl.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-sqlite/src/ddl.rs", "use crate::DIALECT;"),
    ("zeroship-migrate-mysql/src/ddl.rs", "use crate::DIALECT;"),
    (
        "zeroship-migrate-postgres/src/value_format.rs",
        "use crate::DIALECT;",
    ),
    (
        "zeroship-migrate-sqlite/src/value_format.rs",
        "use crate::DIALECT;",
    ),
    (
        "zeroship-migrate-mysql/src/value_format.rs",
        "use crate::DIALECT;",
    ),
    (
        "zeroship-migrate-postgres/src/backend/mod.rs",
        "pub(crate) use crate::DIALECT;",
    ),
    (
        "zeroship-migrate-sqlite/src/backend/mod.rs",
        "use crate::DIALECT as SQLITE_DIALECT;",
    ),
    (
        "zeroship-migrate-mysql/src/backend/mod.rs",
        "use crate::DIALECT;",
    ),
];

/// The NEEDLE control for [`declaration_hits`]: a file outside the vendor crates
/// where `DialectId::new(` is the point and the count cannot fall to zero.
///
/// `dialect_table.rs` is GENERATOR-OWNED — the drift test byte-compares it against
/// `gen:dialect-table` — and every row of it constructs three ids by literal. It is
/// not a violation of anything, so it is not a control that disappears the moment
/// this census succeeds.
const DECL_CONTROL: &str = "zeroship-migrate/tests/dialect_matrix/dialect_table.rs";

/// The floor for that control. Blunt on purpose: the number only has to prove the
/// matcher is alive. Measured at 92 rows when this landed, and the table only grows
/// as op kinds are added.
const DECL_CONTROL_FLOOR: usize = 80;

/// The NEEDLE control for [`code_identifier_hits`] over vendor crate idents: the
/// registry composition, which names all three shipping crates exactly once each and
/// is the one place designed to.
///
/// REPOINTED from `zeroship-migrate-core/src/render/backends/mod.rs` when the composition
/// root became its own crate. The engine no longer names a vendor crate ANYWHERE in
/// production source — it cannot, since it no longer declares one as a dependency —
/// so the file that used to vouch for this needle now returns zero for all three
/// idents, which is the census succeeding rather than the matcher failing. The
/// composition moved to `zero-migrate/src/lib.rs` and took the control with it.
const IDENT_CONTROL: &str = "zeroship-migrate/src/lib.rs";

/// The walk's floor across the three vendor `src` trees.
///
/// A scan over a DISCOVERED set fails OPEN: narrow the walk and it iterates nothing,
/// finds nothing, and reports clean. Measured at 82 `.rs` files the day this landed;
/// the floor sits under that with room for churn and would still notice losing an
/// entire ten-file directory. Raise it deliberately as the vendors grow. NEVER lower
/// it to get green — the anchors above are what tell a shrink from a broken walk.
const VENDOR_FILE_FLOOR: usize = 70;

/// How many times `src` names `identifier` on a CODE line, as a whole token.
///
/// One matcher for both halves of this file, so a census whose two halves used two
/// matchers cannot have one go blind while the other vouches for it.
fn code_identifier_hits(src: &str, identifier: &str) -> usize {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
        .filter(|token| *token == identifier)
        .count()
}

/// How many times `src` CONSTRUCTS a dialect id on a CODE line.
///
/// Substring rather than token, because `DialectId::new(` is the whole shape that
/// matters and splitting it into tokens would count every unrelated `new`.
fn declaration_hits(src: &str) -> usize {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("DialectId::new("))
        .count()
}

/// Every `.rs` file under `root`, sorted.
fn rs_files(root: &Path) -> Vec<PathBuf> {
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

/// `crates/`, the root both halves walk from.
fn crates_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/zeroship-migrate has a parent")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// CLAUSE ONE: a vendor crate declares its dialect EXACTLY ONCE, in its `lib.rs`.
///
/// # What a red here means
///
/// A second `DialectId::new(` appeared somewhere in a vendor crate. Either a module
/// re-declared the id it should be reading from `crate::DIALECT` — which puts the
/// vendor's name back into a module and undoes the rule — or it built a FOREIGN id
/// by literal, which is a cross-vendor reach wearing a different spelling than the
/// one clause two looks for. Neither is fixed by adding an exemption here.
#[test]
fn a_vendor_crate_declares_its_dialect_exactly_once() {
    let crates = crates_root();

    // ---- FLOOR TWO, the NEEDLE, first: a dead matcher makes every count below a
    // ---- meaningless zero, and it is not one of the things being ratcheted.
    let control = read(&crates.join(DECL_CONTROL));
    let control_hits = declaration_hits(&control);
    assert!(
        control_hits >= DECL_CONTROL_FLOOR,
        "the needle found {control_hits} `DialectId::new(` in {DECL_CONTROL}, below \
         the control floor of {DECL_CONTROL_FLOOR}. That table constructs an id on \
         every row and cannot legitimately fall this far, so `declaration_hits` has \
         stopped matching and every zero this census reports is blind. Fix the \
         matcher; do NOT lower the floor."
    );

    for vendor in VENDORS {
        let src = crates.join(vendor.krate).join("src");
        let lib = src.join("lib.rs");
        let lib_text = read(&lib);

        assert!(
            lib_text
                .lines()
                .map(str::trim)
                .any(|l| l == vendor.name_decl),
            "{}/src/lib.rs must spell its id in `{}`. That line is the workspace's \
             ONLY spelling of this vendor's id string; if it legitimately moved or \
             changed shape, repoint this census deliberately and say so in the \
             commit. Do not delete the assertion.",
            vendor.krate,
            vendor.name_decl
        );
        assert!(
            lib_text.lines().map(str::trim).any(|l| l == DIALECT_DECL),
            "{}/src/lib.rs must declare its identity in `{DIALECT_DECL}`; a vendor \
             whose declaration moved or changed shape has lost the one line the rest \
             of the workspace reads its id from",
            vendor.krate
        );

        let mut declarations: Vec<String> = Vec::new();
        for path in rs_files(&src) {
            let hits = declaration_hits(&read(&path));
            if hits > 0 {
                let rel = path
                    .strip_prefix(&crates)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                declarations.push(format!("  {rel}: {hits}"));
            }
        }
        let expected = format!("  {}/src/lib.rs: 1", vendor.krate);
        assert_eq!(
            declarations,
            vec![expected.clone()],
            "{} must construct a `DialectId` exactly once, in its `lib.rs`. Found:\n{}\
             \n\nEverything else in the crate reads `crate::DIALECT`.",
            vendor.krate,
            declarations.join("\n")
        );
    }
}

/// CLAUSE TWO: no vendor crate names another vendor crate.
///
/// # What a red here means
///
/// A vendor reached a sibling by name. That is either a cross-vendor coupling — the
/// thing this rule exists to make loud, and the reason the three crates can be linked
/// independently — or a test in that crate asserting something about a foreign
/// dialect, which belongs in the engine's test tree where all three are visible.
/// Both are questions for the author; neither is fixed by adding an exemption here.
#[test]
fn no_vendor_crate_names_a_foreign_vendor() {
    let crates = crates_root();

    // ---- FLOOR TWO, the NEEDLE. The registry composition names all three shipping
    // ---- crates and is PERMANENT — `render/backends/mod.rs` is the one place
    // ---- designed to hold them — so it cannot go to zero the way a violation can.
    let control = read(&crates.join(IDENT_CONTROL));
    for vendor in VENDORS {
        let hits = code_identifier_hits(&control, vendor.ident);
        assert_eq!(
            hits, 1,
            "the needle found `{}` {hits} time(s) in {IDENT_CONTROL}, expected 1 (its \
             `SHIPPING` entry). Either the registry was restructured — update this \
             control deliberately — or `code_identifier_hits` stopped matching, in \
             which case every zero below it is blind.",
            vendor.ident
        );
    }

    // ---- FLOOR ONE, the WALK. -----------------------------------------------
    let mut walked = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for vendor in VENDORS {
        let root = crates.join(vendor.krate).join("src");
        assert!(
            root.is_dir(),
            "{} does not exist, so this census would walk nothing for {} and report \
             clean",
            root.display(),
            vendor.krate
        );
        for path in rs_files(&root) {
            walked += 1;
            let rel = path
                .strip_prefix(&crates)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let text = read(&path);
            for other in VENDORS {
                if other.ident == vendor.ident {
                    continue;
                }
                let hits = code_identifier_hits(&text, other.ident);
                if hits > 0 {
                    violations.push(format!("  {rel}: names `{}` {hits} time(s)", other.ident));
                }
            }
        }
    }

    assert!(
        walked >= VENDOR_FILE_FLOOR,
        "the census walked only {walked} vendor-crate files, below the floor of \
         {VENDOR_FILE_FLOOR}. A narrowed walk finds nothing and reports clean; fix \
         the walk, do not lower the floor to get green."
    );

    assert!(
        violations.is_empty(),
        "a vendor crate names a FOREIGN vendor crate:\n{}\n\nA backend names its own \
         dialect and no other. See the one-dialect-literal rule in \
         render/backends/mod.rs.",
        violations.join("\n")
    );
}

/// CLAUSE THREE: every module that needs the identity READS it, and the import is the
/// one shape the rule allows.
///
/// # Why the list is enumerated rather than walked
///
/// A walk would have to decide which modules OUGHT to hold the identity, and it
/// cannot: a module that legitimately never names a dialect is indistinguishable from
/// one that lost its import. The list is the fact. `include_str!` is not used because
/// these are read from three sibling crates at run time by the same walker the other
/// two clauses use, which is what keeps all three provably reading one tree.
#[test]
fn every_identity_reader_reads_the_crates_own_id() {
    let crates = crates_root();
    for (rel, import) in IDENTITY_READERS {
        let path = crates.join(rel);
        assert!(
            path.is_file(),
            "this census must read {}, and it does not exist. If the module \
             legitimately moved, repoint the entry and say so in the commit; do NOT \
             delete it to get green.",
            path.display()
        );
        let text = read(&path);
        assert!(
            text.lines().map(str::trim).any(|line| line == *import),
            "{rel} must read its vendor identity as `{import}`. A module that spells \
             a dialect any other way has either re-declared one — which puts the \
             vendor's name back in the module — or stopped reading the crate's, in \
             which case nothing here knows which vendor it is."
        );
    }
}
