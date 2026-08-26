//! A backend's spelling stays behind its `BackendVendor`, and privacy is what enforces it.
//!
//! [`BackendVendor`](zero_migrate_backend::registry::BackendVendor)'s own documentation
//! makes this claim: "Nothing else of a vendor crate's surface is public API — the
//! renderer structs themselves stay crate-private, so a caller cannot reach past this
//! descriptor to a vendor's spelling without going through a registry."
//!
//! That claim is TRUE today, and until this file existed nothing checked it. One
//! character — `mod dml;` becoming `pub mod dml;` — would open a door for core, the
//! launcher or a host to call a vendor's renderer directly, and the neutrality censuses
//! would not notice, because they police who NAMES a vendor rather than what a vendor
//! EXPOSES.
//!
//! # Why a source census and not a compile check
//!
//! Privacy is invisible from outside the crate: a test cannot ask "is
//! `zero_migrate_postgres::dml` private?", it can only fail to compile if it tries to use
//! it — and a `compile_fail` doctest per module per vendor would pass for any reason at
//! all, including a typo'd path. Reading the declaration is the only way to distinguish
//! "private" from "absent", and the difference matters: see the liveness floor below.
//!
//! # The lesson this is a standing instance of
//!
//! A PRIVACY-BASED INVARIANT DOES NOT SURVIVE A CRATE BOUNDARY. When the vendor backends
//! moved out of core, `pub(crate)` items became `pub` because callers in the new crate
//! needed them, and each compiler-enforced invariant that dissolved needed a replacement
//! test. This is that replacement for the nine contract-implementation modules.

use std::fs;
use std::path::PathBuf;

/// The nine modules that IMPLEMENT the `BackendVendor` contract fields. Every one exists
/// in all three vendor crates, and none may be public: they are reached through the
/// descriptor's `&'static dyn` fields or not at all.
const CONTRACT_MODULES: &[&str] = &[
    "advisory",
    "ddl",
    "descriptor",
    "dml",
    "existence_probe",
    "fold",
    "schema",
    "validation",
    "value_format",
];

/// What a vendor crate is ALLOWED to publish, beyond `DIALECT` and `VENDOR`.
///
/// An allow-list rather than a deny-list, because the failure direction is asymmetric: a
/// deny-list silently permits every module nobody thought to name, which is precisely the
/// door this file exists to keep shut. Each entry below is a deliberate public surface,
/// and adding one should be a decision someone makes here, in the open.
const ALLOWED_PUBLIC_MODULES: &[&str] = &[
    // Backend entry points a host composes with.
    "backend",
    // Each vendor's declared attribute vocabulary — public because its own npm package's
    // export test reads it.
    "attribute",
    // The line-1 guard, public so a host can name the type it is configuring.
    "guard",
    // PostgreSQL: parse-time analysis, cross-schema confinement, and the role model.
    "analysis",
    "confinement",
    "role",
    // MySQL: the collation table and the physical-type parser, both consulted by tests
    // and by the host's diagnostics.
    "collation",
    "physical_type",
];

/// Line floor. A truncated or unreadable `lib.rs` yields no `mod` declarations at all,
/// and every assertion below would then range over an empty set and pass.
const LIB_RS_LINE_FLOOR: usize = 80;

fn vendor_lib_rs(crate_name: &str) -> (PathBuf, String) {
    // CARGO_MANIFEST_DIR is `crates/zero-migrate`; the vendor crates are its siblings.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_name)
        .join("src")
        .join("lib.rs");
    let src =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    (path, src)
}

/// Every declared module in a `lib.rs`, as (name, is_public).
///
/// Only top-of-line declarations count. A `mod` nested inside a function or an inline
/// `mod foo { … }` block is not a crate-root module and is none of this file's business.
fn declared_modules(src: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let (is_pub, rest) = if let Some(r) = line.strip_prefix("pub mod ") {
            (true, r)
        } else if let Some(r) = line.strip_prefix("mod ") {
            (false, r)
        } else {
            continue;
        };
        // `mod name;` only — an inline `mod name {` declares a body, not a file module.
        if let Some(name) = rest.strip_suffix(';') {
            out.push((name.trim().to_string(), is_pub));
        }
    }
    out
}

const VENDOR_CRATES: &[&str] = &[
    "zero-migrate-postgres",
    "zero-migrate-mysql",
    "zero-migrate-sqlite",
];

#[test]
fn no_vendor_publishes_the_modules_that_implement_its_contract() {
    let mut checked = 0_usize;
    for crate_name in VENDOR_CRATES {
        let (path, src) = vendor_lib_rs(crate_name);
        assert!(
            src.lines().count() >= LIB_RS_LINE_FLOOR,
            "{} has {} lines, below the floor of {LIB_RS_LINE_FLOOR} — a truncated read \
             would make every check here vacuous",
            path.display(),
            src.lines().count()
        );
        let modules = declared_modules(&src);

        for wanted in CONTRACT_MODULES {
            // NEEDLE LIVENESS, and the whole reason this loop is written this way.
            // "there is no `pub mod dml;`" is ALSO true when there is no `dml` at all, so
            // a rename or a deletion would turn this test green while removing what it
            // guards. Require the module to be PRESENT, then require it to be private.
            let found = modules
                .iter()
                .find(|(name, _)| name == wanted)
                .unwrap_or_else(|| {
                    panic!(
                        "{} declares no `{wanted}` module. Either it was renamed — in \
                         which case update CONTRACT_MODULES, since this test is now \
                         blind to it — or the contract implementation moved and this \
                         census needs re-scoping.",
                        path.display()
                    )
                });
            assert!(
                !found.1,
                "{} declares `pub mod {wanted};`. That opens a door past the \
                 `BackendVendor` descriptor: core, the launcher or a host could call this \
                 backend's spelling directly instead of through the registry, which is \
                 the coupling the descriptor exists to prevent. Keep it `mod {wanted};`.",
                path.display()
            );
            checked += 1;
        }
    }

    // The count is the product of the two loops, so it catches an empty vendor list as
    // well as an empty module list.
    let expected = VENDOR_CRATES.len() * CONTRACT_MODULES.len();
    assert_eq!(
        checked, expected,
        "the census examined {checked} (vendor, module) pairs, expected {expected}"
    );
}

#[test]
fn a_vendors_public_module_surface_stays_on_its_allow_list() {
    let mut checked = 0_usize;
    for crate_name in VENDOR_CRATES {
        let (path, src) = vendor_lib_rs(crate_name);
        for (name, is_pub) in declared_modules(&src) {
            if !is_pub {
                continue;
            }
            assert!(
                ALLOWED_PUBLIC_MODULES.contains(&name.as_str()),
                "{} declares `pub mod {name};`, which is not on the allow-list. A new \
                 public module on a vendor crate is a new way to reach that backend \
                 without the registry. If it is deliberate, add it to \
                 ALLOWED_PUBLIC_MODULES with a line saying what it is for.",
                path.display()
            );
            checked += 1;
        }
    }
    // Every vendor publishes at least `backend`, `attribute` and `guard`, so a walk that
    // saw fewer than three per crate read something that is not a vendor `lib.rs`.
    assert!(
        checked >= VENDOR_CRATES.len() * 3,
        "only {checked} public module(s) seen across {} vendor crates — too few for the \
         walk to have found the real files",
        VENDOR_CRATES.len()
    );
}

/// The allow-list must not rot into a list of names nothing uses.
///
/// An entry that no vendor declares is dead permission: it grants nothing today, and the
/// next person reading the list cannot tell whether it is obsolete or aspirational. This
/// keeps the list honest in the direction the other two tests do not cover.
#[test]
fn every_allow_list_entry_is_actually_published_by_some_vendor() {
    let published: Vec<String> = VENDOR_CRATES
        .iter()
        .flat_map(|c| declared_modules(&vendor_lib_rs(c).1))
        .filter(|(_, is_pub)| *is_pub)
        .map(|(name, _)| name)
        .collect();

    assert!(
        !published.is_empty(),
        "no vendor publishes any module — the walk found nothing"
    );

    for allowed in ALLOWED_PUBLIC_MODULES {
        assert!(
            published.iter().any(|p| p == allowed),
            "`{allowed}` is on the allow-list but no vendor declares `pub mod {allowed};`. \
             Remove it: a permission nobody exercises reads as policy and is not."
        );
    }
}
