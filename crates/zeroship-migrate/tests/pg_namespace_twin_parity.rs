//! Twin-parity gate for the standalone `pg.*` vendor namespace (SA-4).
//!
//! The privileged vendor surface is authored twice, byte-structurally identical:
//!   - the SDK DSL   `sdks/migrate/src/pg.ts`            (`export const pg`)
//!   - the engine    `crates/zeroship-migrate/src/frontend/migrate_ops.js`
//!     (`export const pg`, the in-V8 recorder the engine `include_str!`s)
//!
//! A one-sided edit (a method added to one `pg` object but not the other) is a
//! lock-step contract violation: it makes a vendor op authorable via one path but
//! `pg.X is not a function` via the other. This test extracts the method-name set
//! from each `export const pg` object literal and asserts they are equal, so the
//! drift (e.g. the historical `pg.createPolicy` / `pg.dropPolicy` engine gap)
//! cannot regress silently.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Collect the top-level method names from the `export const pg = { … }` object
/// literal in a JS/TS source. Methods are 2-space-indented `name(` entries; the
/// block runs from the `export const pg` line to the first column-0 `};`.
fn pg_method_names(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_block = false;
    for line in src.lines() {
        if !in_block {
            if line.trim_start().starts_with("export const pg") {
                in_block = true;
            }
            continue;
        }
        // End of the object literal (column-0 `};`).
        if line.starts_with("};") {
            break;
        }
        // A method entry is exactly 2-space-indented `name(` (not deeper-nested
        // body lines, not comments).
        if let Some(rest) = line.strip_prefix("  ") {
            if rest.starts_with(' ') {
                continue; // deeper indentation = method body, skip
            }
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() && rest[name.len()..].starts_with('(') {
                names.insert(name);
            }
        }
    }
    names
}

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn pg_namespace_methods_are_lockstep_across_twins() {
    let engine = read("src/frontend/migrate_ops.js");
    let sdk = read("../../sdks/migrate/src/pg.ts");

    let engine_methods = pg_method_names(&engine);
    let sdk_methods = pg_method_names(&sdk);

    assert!(
        !engine_methods.is_empty() && !sdk_methods.is_empty(),
        "twin-parity extractor found no pg.* methods (engine={engine_methods:?}, sdk={sdk_methods:?})"
    );

    // The historically-drifted methods must be present in BOTH.
    for required in ["createPolicy", "dropPolicy", "dropExtension"] {
        assert!(engine_methods.contains(required), "engine pg.* missing {required}");
        assert!(sdk_methods.contains(required), "sdk pg.* missing {required}");
    }

    assert_eq!(
        engine_methods, sdk_methods,
        "pg.* namespace drifted between the engine recorder (migrate_ops.js) and the SDK DSL (pg.ts);\n  engine-only: {:?}\n  sdk-only:    {:?}",
        engine_methods.difference(&sdk_methods).collect::<Vec<_>>(),
        sdk_methods.difference(&engine_methods).collect::<Vec<_>>(),
    );
}
