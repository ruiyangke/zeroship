//! Host-provided JavaScript modules for runtime initialization.
//! The DB internal entry supplies the facade installer and SDK helpers together.
//! Creator builds need not retain an otherwise unused installer import.

use crate::core::init::ZEROSHIP_MODULE_JS;

const DB_INTERNAL_JS: &str = include_str!("../../../../sdks/db/dist/internal.js");

/// Resolve a bare specifier to a runtime-provided module source, or `None`
/// if it isn't one we own. The `zeroship` facade is resolved here too so
/// the transitive close-over works even for apps that never statically
/// import `zeroship` themselves (the BFS in `load_modules` only compiles
/// statically-reached modules, so `zeroship` may be absent from the
/// registry when an install-schema/internal import pulls it in).
pub(crate) fn source_for(specifier: &str) -> Option<&'static str> {
    match specifier {
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
