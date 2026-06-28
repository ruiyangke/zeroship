//! Runtime-provided JS modules resolvable by bare specifier.
//!
//! # Why this exists (ISS-63)
//!
//! The runtime splices `runtime-entry.js` into the bootstrap `index.js`
//! it wraps every app with (see `core::init::BOOTSTRAP_JS`). During module
//! evaluation that entry runs for any app carrying a runtime schema descriptor:
//!
//! ```js
//! const sdk = await import("@zeroship/bootstrap/install-schema");
//! // ...and, for the mask-policy flush:
//! const policyMod = await import("@zeroship/db/internal");
//! ```
//!
//! The CODE that issues those imports is injected by the **runtime**, but
//! the modules it imports were assumed to be **bundle-resident** — pulled
//! in by the Vite plugin's synthetic SSR entry side-effect-importing them.
//! In practice the production `.zship` is a single self-contained
//! `index.js`: Vite tree-shakes the framework-internal `installSchema`
//! (and the `@zeroship/db/internal` machinery it needs) straight out,
//! because the user app never references them. Neither is a `node:*`
//! native module, so the dynamic-import host callback rejected with
//! `TypeError: Cannot find module '@zeroship/bootstrap/install-schema'`,
//! aborting module evaluation → HTTP 500 for EVERY env.db app.
//!
//! The runtime is the only component that can guarantee these resolve,
//! because it is the one that injects the import. So the runtime ships
//! their compiled source itself (`include_str!` of the same dist files it
//! already embeds for `runtime-entry.js` / `dispatcher.js`) and resolves
//! the bare specifiers against this table.
//!
//! # Resolution graph
//!
//! ```text
//! @zeroship/bootstrap/install-schema  →  @zeroship/db/internal  →  zeroship
//! @zeroship/db/internal               →  zeroship
//! ```
//!
//! `zeroship` is the runtime's `env` facade (`core::init::ZEROSHIP_MODULE_JS`),
//! already injected as a `ModuleEntry` by `wrap_with_bootstrap`. When the
//! host callback resolves a bootstrap specifier it compiles the whole
//! transitive set into the registry first, so the standard
//! `modules::resolve_callback` wires the static imports during
//! instantiation — no special-case resolver, no back-compat shim.
//!
//! # Dev path
//!
//! Untouched. In `pnpm dev` the Vite plugin's ModuleRunner resolves
//! `@zeroship/bootstrap/install-schema` itself (it's an on-disk package),
//! so this table is never consulted — the host callback's registry/native
//! paths fire first and this is only reached for bare specifiers the
//! bundle didn't carry, which is exactly the production gap.

use crate::core::init::ZEROSHIP_MODULE_JS;

/// Compiled `installSchema` — the framework-internal helper behind the
/// `export default { schema }` convention. Same dist file the bootstrap
/// package emits via `pnpm build`; statically imports `@zeroship/db/internal`.
const INSTALL_SCHEMA_JS: &str =
    include_str!("../../../../sdks/bootstrap/dist/install-schema.js");

/// Compiled `@zeroship/db/internal` — Collection / SchemaBuilder /
/// transaction machinery + `_flushPendingMaskPolicy`. Imports only the
/// runtime-provided `zeroship` facade.
const DB_INTERNAL_JS: &str = include_str!("../../../../sdks/db/dist/internal.js");

/// Resolve a bare specifier to a runtime-provided module source, or `None`
/// if it isn't one we own. The `zeroship` facade is resolved here too so
/// the transitive close-over works even for apps that never statically
/// import `zeroship` themselves (the BFS in `load_modules` only compiles
/// statically-reached modules, so `zeroship` may be absent from the
/// registry when an install-schema/internal import pulls it in).
pub(crate) fn source_for(specifier: &str) -> Option<&'static str> {
    match specifier {
        "@zeroship/bootstrap/install-schema" => Some(INSTALL_SCHEMA_JS),
        "@zeroship/db/internal" => Some(DB_INTERNAL_JS),
        "zeroship" => Some(ZEROSHIP_MODULE_JS),
        _ => None,
    }
}

/// The static imports a runtime-provided module declares — the transitive
/// closure the host callback must pre-compile into the registry before
/// instantiating `specifier`, so `modules::resolve_callback` finds each
/// one. Kept in lockstep with the `import ... from` lines in the dist
/// files above; a drift would surface immediately as a resolution failure
/// in `bootstrap_install_schema_resolve.rs`.
pub(crate) fn deps_of(specifier: &str) -> &'static [&'static str] {
    match specifier {
        "@zeroship/bootstrap/install-schema" => &["@zeroship/db/internal"],
        "@zeroship/db/internal" => &["zeroship"],
        // `zeroship` (env facade) imports nothing.
        _ => &[],
    }
}

/// True if `specifier` names a runtime-provided module — used to gate the
/// dynamic-import host callback's bootstrap path.
pub(crate) fn is_bootstrap_module(specifier: &str) -> bool {
    source_for(specifier).is_some()
}
