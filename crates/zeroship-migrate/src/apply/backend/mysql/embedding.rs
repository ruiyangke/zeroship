//! V8 embedding for the platform-owned Trusted MySQL JS-driver isolate.
//!
//! This module hosts the `mysql2/promise` driver-isolate module graph + the
//! vendored `mysql2` bundle. It lives beside its sole caller
//! (`transport.rs`), so the MySQL backend has no code edge into the
//! schema-authoring `frontend/` module. Both this module and `frontend/` ride
//! the same `zsv8` feature, so they compile-or-neither together, but the code
//! dependency is severed by construction (a MySQL driver-isolate helper does
//! not belong in the schema-authoring module).

use zeroship_runtime::ModuleEntry;

/// The vendored `mysql2/promise` bundle for the Trusted JS-driver isolate.
const MYSQL2_PROMISE_BUNDLE_JS: &str = include_str!("vendor/mysql2-3.14.1.bundle.mjs");

fn module(specifier: &str, source: &str) -> ModuleEntry {
    ModuleEntry {
        specifier: specifier.to_string(),
        source: source.to_string(),
    }
}

/// Build the module graph for the platform-owned Trusted JS-driver isolate.
///
/// `entry_source` is the long-lived command-loop driver entry. The mysql2
/// bundle is registered under the package specifier the entry imports:
/// `import mysql from "mysql2/promise"`.
pub(crate) fn js_driver_module_graph(entry_source: &str) -> Vec<ModuleEntry> {
    vec![
        module("js_driver_entry.js", entry_source),
        module("mysql2/promise", MYSQL2_PROMISE_BUNDLE_JS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DB-free proof that the relocated `include_str!` still resolves and the
    /// module graph is well-formed without a live MySQL (§4).
    #[test]
    fn module_graph_is_well_formed_and_bundle_non_empty() {
        assert!(!MYSQL2_PROMISE_BUNDLE_JS.is_empty());
        let g = js_driver_module_graph("export default 1;");
        assert!(g.iter().any(|m| m.specifier == "mysql2/promise"));
        assert!(g.iter().any(|m| m.specifier == "js_driver_entry.js"));
    }
}
