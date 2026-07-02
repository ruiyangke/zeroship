//! Twin-parity gate for the standalone `/pg` vendor named exports (SA-4).
//!
//! The privileged vendor surface is authored twice, byte-structurally identical:
//!   - the SDK DSL   `sdks/migrate/src/pg.ts`
//!   - the engine    `crates/zeroship-migrate/src/frontend/migrate_ops.js`
//!     (the in-V8 recorder the engine `include_str!`s)
//!
//! A one-sided edit (an export added to one copy but not the other) is a
//! lock-step contract violation: it makes a vendor op authorable via one path but
//! `X is not a function` via the other. This test extracts the vendor export-name
//! set from each implementation and asserts they are equal, so the drift (e.g.
//! the historical `createPolicy` / `dropPolicy` engine gap)
//! cannot regress silently.

use std::collections::BTreeSet;
use std::path::PathBuf;

const VENDOR_EXPORTS: &[&str] = &[
    "schema",
    "dropSchema",
    "extension",
    "dropExtension",
    "role",
    "alterRole",
    "dropRole",
    "dropOwnedBy",
    "grant",
    "revoke",
    "createPolicy",
    "dropPolicy",
    "createFunction",
    "dropFunction",
    "raw",
    "domain",
    "sequence",
];

/// Collect the direct vendor exports from the implementation file. The public
/// functions are `export function name(` declarations; moved PG-only handles are
/// `export const name = __pg...` aliases. Internal `__pg*` hooks are ignored.
fn pg_export_names(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        let name = if let Some(rest) = trimmed.strip_prefix("export function ") {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        } else if let Some(rest) = trimmed.strip_prefix("export const ") {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        } else {
            String::new()
        };
        let public_name = match name.as_str() {
            "__pgDomain" => "domain",
            "__pgSequence" => "sequence",
            _ => name.as_str(),
        };
        if VENDOR_EXPORTS.contains(&public_name) {
            names.insert(public_name.to_string());
        }
    }
    names
}

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn pg_named_exports_are_lockstep_across_twins() {
    let engine = read("src/frontend/migrate_ops.js");
    let sdk = read("../../sdks/migrate/src/pg.ts");

    let engine_methods = pg_export_names(&engine);
    let sdk_methods = pg_export_names(&sdk);

    assert!(
        !engine_methods.is_empty() && !sdk_methods.is_empty(),
        "twin-parity extractor found no /pg exports (engine={engine_methods:?}, sdk={sdk_methods:?})"
    );

    // The historically-drifted methods must be present in BOTH.
    for required in ["createPolicy", "dropPolicy", "dropExtension"] {
        assert!(engine_methods.contains(required), "engine /pg exports missing {required}");
        assert!(sdk_methods.contains(required), "sdk /pg exports missing {required}");
    }

    assert_eq!(
        engine_methods, sdk_methods,
        "/pg named exports drifted between the engine recorder (migrate_ops.js) and the SDK DSL (pg.ts);\n  engine-only: {:?}\n  sdk-only:    {:?}",
        engine_methods.difference(&sdk_methods).collect::<Vec<_>>(),
        sdk_methods.difference(&engine_methods).collect::<Vec<_>>(),
    );
}
