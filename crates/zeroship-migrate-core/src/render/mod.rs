pub(crate) mod backends;
pub mod declarative;
pub mod dml;
// -- The existence-guard DECIDER moved into `zero-migrate-backend`. It had to: the
// only production callers are the three backends' session paths, and a backend in
// its own crate cannot reach an engine module. Nothing about it was engine-shaped -
// every private helper below `decide` was already parameterized by
// `&BackendVendor`, and `decide` itself resolved one from a `DialectId`, which was
// the single line that had to change.
//
// What stays HERE is that resolution, and only that. The engine looks the vendor up
// and hands it down; a vendor hands its OWN `VENDOR` down and never asks. That is
// the same split `render::backends` draws everywhere else, and it is why this is a
// shim rather than a `pub use` of the whole module: a bare re-export would put the
// vendor-taking `decide` on the engine's surface under the name its dialect-taking
// callers use.
pub mod existence_probe {
    pub use zeroship_migrate_backend::existence_probe::{
        Divergence, ExistenceProbePolicy, GuardVerdict,
    };

    use zeroship_migrate_backend::registry::VendorSet;
    use zeroship_migrate_backend::snapshot::SchemaSnapshot;
    use zeroship_migrate_ir::dialect::DialectId;
    use zeroship_migrate_ir::probe::GuardProbe;

    /// Decide the verdict for `probe` against the LIVE catalog `live`, resolving
    /// `dialect`'s registered backend through the build's vendor registry.
    ///
    /// The engine's entry point. A backend that already knows which vendor it is
    /// calls [`zeroship_migrate_backend::existence_probe::decide`] with its own
    /// `BackendVendor` instead - see that function for the per-variant fail-closed
    /// rules, which is where all of them now live.
    #[must_use]
    pub fn decide(
        vendors: VendorSet,
        probe: &GuardProbe,
        live: &SchemaSnapshot,
        dialect: &DialectId,
    ) -> GuardVerdict {
        zeroship_migrate_backend::existence_probe::decide(
            probe,
            live,
            crate::render::backends::vendor(vendors, dialect),
        )
    }
}
pub mod expand_contract;
pub mod fold;
pub mod gen_types;
pub mod lower;
pub mod plan;
pub mod sql_preview;
pub mod step;
pub(crate) mod value_format;
pub mod vendor;

// -- The backend CONTRACT moved into `zero-migrate-backend` and is re-exported under
// its historical `crate::render::*` paths, so every in-crate `render::dml::...`,
// `render::renderer::...` and `render::vendor::...` reference resolves unchanged.
//
// The three modules had to go because the two renderer TRAITS are in `renderer`, and
// a vendor crate cannot implement a trait it may not name. `dml` and `vendor` went
// with them because the vendors CALL them - the identifier seam, the inline
// expression renderer, `render_vendor_op` and `VendorStatement` (the return type of
// `DmlRenderer::render_trigger_op`).
pub(crate) mod renderer {
    pub(crate) use zeroship_migrate_backend::renderer::*;

    use zeroship_migrate_backend::registry::VendorSet;
    use zeroship_migrate_ir::dialect::DialectId;

    /// Resolve one capability through the build's open vendor registry.
    pub(crate) trait DialectSupports {
        fn supports(self, vendors: VendorSet, cap: Capability) -> bool;
    }

    impl DialectSupports for &DialectId {
        fn supports(self, vendors: VendorSet, cap: Capability) -> bool {
            crate::render::backends::vendor(vendors, self)
                .descriptor
                .capabilities
                .contains(cap)
        }
    }
}
