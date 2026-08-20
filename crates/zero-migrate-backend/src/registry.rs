//! What a BACKEND CRATE hands the engine, and the shape the engine composes them in.
//!
//! # Why this exists and `zero_migrate_ir::backend::BackendRegistry` does not suffice
//!
//! `zero-migrate-ir` already has a `BackendRegistry`, built fallibly, refusing
//! duplicate and malformed ids and naming both registrants on a collision. It is
//! reused verbatim by [`VendorSet::descriptors`] and it is the right thing for what
//! it holds — but what it holds is `&'static BackendDescriptor`, i.e. a vendor's
//! CAPABILITY row. It cannot hold a renderer, and it cannot be made to: a renderer
//! is a `&'static dyn` of a trait declared in THIS crate, and `-ir` is below this
//! crate and may not name it.
//!
//! So there are two registries and they answer different questions. The descriptor
//! registry answers "what can this vendor do", and `-ir` owns it. This one answers
//! "who spells this vendor's SQL", and it pairs each descriptor with the two
//! renderers that go with it. [`VendorSet::descriptors`] derives the first from the
//! second, so the two cannot drift: a vendor that ships a renderer ships exactly one
//! descriptor, and the id rule is enforced by `-ir`'s builder rather than restated
//! here.
//!
//! # The cycle this breaks
//!
//! Before the split, `render::backends::renderer` was an exhaustive `match` over
//! `SqlDialect` naming `postgres::RENDERER`, `sqlite::RENDERER` and
//! `mysql::RENDERER` — three statics in the same crate as the trait. Extract the
//! vendors and the engine names them for its registry while they name the engine for
//! `DmlRenderer`, `IrLowerError` and `DmlError`. Cargo refuses.
//!
//! The direction is fixed by putting the CONTRACT below both: the vendor crates
//! depend on this crate and nothing else of the engine's, the engine depends on this
//! crate AND on the three vendor crates, and the arrow never comes back. A fourth
//! backend is a fourth `[dependencies]` line and a fourth entry in the engine's
//! `VendorSet`, with no edit to any file here.
//!
//! # What did NOT change
//!
//! The engine's dispatch is still EXHAUSTIVE over the closed `SqlDialect`, and that
//! is deliberate. [`VendorSet`] is a fixed-size array, not a growable map, so the
//! shipping set is still resolved at compile time and a fourth `SqlDialect` variant
//! still breaks the engine's `for_dialect` match until its vendor is wired. The
//! alternative — a lazily-populated global the host fills at startup — would turn
//! "no backend for this dialect" from a compile error into a runtime one, and would
//! make every one of the engine's ~1200 in-crate render tests depend on registration
//! order.

use crate::renderer::DmlRenderer;
use crate::schema::SchemaRenderer;
use zero_migrate_ir::backend::{BackendDescriptor, BackendRegistry, RegistryError};

/// Everything one backend crate exports: its capability row and its two renderers.
///
/// A vendor crate declares exactly one of these as a `pub static` and the engine
/// names it. Nothing else of a vendor crate's surface is public API — the renderer
/// structs themselves stay crate-private, so a caller cannot reach past this
/// descriptor to a vendor's spelling without going through a registry.
#[derive(Debug)]
pub struct BackendVendor {
    /// The vendor's capability row, the same `&'static BackendDescriptor`
    /// `zero-migrate-ir` keys its registry by.
    pub descriptor: &'static BackendDescriptor,
    /// How this vendor spells DML, views and triggers.
    pub dml: &'static dyn DmlRenderer,
    /// How this vendor spells columns and DDL.
    pub schema: &'static dyn SchemaRenderer,
}

/// The set of backends a build ships.
///
/// Deliberately a slice of `&'static BackendVendor` rather than a map: the shipping
/// set is a compile-time fact, and a set that could grow at run time would let a
/// duplicate id in behind [`BackendRegistry`]'s check — which is the same reason
/// `-ir`'s registry has no `insert`.
#[derive(Debug, Clone, Copy)]
pub struct VendorSet {
    vendors: &'static [&'static BackendVendor],
}

impl VendorSet {
    /// Wrap a shipping list.
    #[must_use]
    pub const fn new(vendors: &'static [&'static BackendVendor]) -> Self {
        Self { vendors }
    }

    /// The vendors, in registration order.
    #[must_use]
    pub const fn as_slice(self) -> &'static [&'static BackendVendor] {
        self.vendors
    }

    /// How many backends this build ships.
    #[must_use]
    pub const fn len(self) -> usize {
        self.vendors.len()
    }

    /// Whether this build ships no backend at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.vendors.is_empty()
    }

    /// The vendor filed under `descriptor.id`, if this build has one.
    #[must_use]
    pub fn get(self, id: zero_migrate_ir::dialect::DialectId) -> Option<&'static BackendVendor> {
        self.vendors.iter().copied().find(|v| v.descriptor.id == id)
    }

    /// Validate this set's descriptors into `-ir`'s [`BackendRegistry`].
    ///
    /// This is the composition the hard-coded `match` used to stand in for, and it
    /// is where a duplicate or malformed id is caught — by the leaf crate's builder,
    /// which names BOTH registrants on a collision, rather than by a rule restated
    /// here.
    ///
    /// # Errors
    ///
    /// Whatever [`BackendRegistry::build`] returns.
    pub fn descriptors(self) -> Result<BackendRegistry, RegistryError> {
        let descriptors: Vec<&'static BackendDescriptor> =
            self.vendors.iter().map(|v| v.descriptor).collect();
        BackendRegistry::build(&descriptors)
    }
}
