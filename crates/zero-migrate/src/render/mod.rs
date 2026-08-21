pub(crate) mod backends;
pub mod declarative;
pub mod dml;
pub mod existence_probe;
pub mod expand_contract;
pub mod fold;
pub mod gen_types;
pub mod lower;
pub mod plan;
pub mod sql_preview;
pub mod step;
pub(crate) mod value_format;
pub mod vendor;

// ── The backend CONTRACT moved into `zero-migrate-backend` and is re-exported under
// its historical `crate::render::*` paths, so every in-crate `render::dml::…`,
// `render::renderer::…` and `render::vendor::…` reference resolves unchanged.
//
// The three modules had to go because the two renderer TRAITS are in `renderer`, and
// a vendor crate cannot implement a trait it may not name. `dml` and `vendor` went
// with them because the vendors CALL them — the identifier seam, the inline
// expression renderer, `render_vendor_op` and `VendorStatement` (the return type of
// `DmlRenderer::render_trigger_op`).
pub(crate) mod renderer {
    pub(crate) use zero_migrate_backend::renderer::*;

    use crate::schema::query::SqlDialect;

    /// Temporary compatibility for core carriers that still hold the closed enum.
    ///
    /// The capability row is resolved through the build's open vendor registry. No
    /// descriptor table or vendor match lives in the contract crate; this trait goes
    /// away with the remaining `SqlDialect` carriers.
    pub(crate) trait DialectSupports {
        fn supports(self, cap: Capability) -> bool;
    }

    impl DialectSupports for SqlDialect {
        fn supports(self, cap: Capability) -> bool {
            crate::render::backends::vendor(&self.id())
                .descriptor
                .capabilities
                .contains(cap)
        }
    }
}
