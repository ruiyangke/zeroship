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
//! "who spells this vendor's SQL, and who refuses to run it", and it pairs each
//! descriptor with the registered renderers and the one guard that go with it.
//! [`VendorSet::descriptors`] derives the first from the second, so the two cannot
//! drift: a vendor that ships a renderer ships exactly one descriptor, and the id rule
//! is enforced by `-ir`'s builder rather than restated here.
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
//! [`VendorSet`] remains a compile-time shipping slice, not a lazily populated
//! global. Dispatch now looks up the open [`DialectId`](zero_migrate_ir::dialect::DialectId)
//! filed by each descriptor, so the contract contains no enum match and a fourth
//! backend requires no contract edit. The engine's temporary `SqlDialect` callers
//! still convert their target to that id until the enum is deleted.

use crate::ddl::DdlEmitter;
use crate::guard::{GuardConfig, MigrationGuard};
use crate::renderer::DmlRenderer;
use crate::schema::SchemaRenderer;
use crate::value_format::ValueFormatRenderer;
use zero_migrate_ir::backend::{BackendDescriptor, BackendRegistry, RegistryError};

/// Build this vendor's line-1 guard for a config.
///
/// A plain `fn` pointer rather than a `&'static dyn MigrationGuard` because a guard
/// is not `'static`: PostgreSQL's holds the [`GuardConfig`] it decides against, and
/// that config is composed per call from a per-migration [`crate::guard::GuardConfig`].
/// This is the same signature the engine's old `guard_for` had; what changed is WHO
/// owns the dispatch.
pub type GuardFactory = fn(&GuardConfig) -> Box<dyn MigrationGuard>;

/// Build this vendor's DDL emitter for one project schema.
///
/// PostgreSQL and MySQL retain the schema for ordinary qualification. SQLite emits
/// ordinary table DDL into unqualified `main`, but retains the schema too so its
/// capability-gated dormant FK-clause answer stays byte-identical to the former
/// core route. A function pointer keeps the registered vendor static while each
/// author receives an owned, schema-bound emitter.
pub type DdlFactory = fn(&str) -> Box<dyn DdlEmitter>;

/// Everything one backend crate exports: its capability row, its renderers,
/// and its line-1 guard.
///
/// A vendor crate declares exactly one of these as a `pub static` and the engine
/// names it. Nothing else of a vendor crate's surface is public API — the renderer
/// structs themselves stay crate-private, so a caller cannot reach past this
/// descriptor to a vendor's spelling without going through a registry.
///
/// # Every field is REQUIRED, and `guard` is why that matters
///
/// This struct derives no `Default`, has no `Default` impl, and is not
/// `#[non_exhaustive]`. All six fields must be written out in a struct literal at the
/// vendor's own definition site. A new backend that ships no DDL emitter or no guard
/// therefore fails to compile **in its own crate, named** — E0063 for the missing
/// field — rather than picking one up by omission.
///
/// That is the whole point of the field. It replaced a `guard_for(cfg)` function whose
/// `SqlDialect` match handed both descriptor-only dialects one shared trusting guard.
/// The match was exhaustive, so a fourth dialect broke the build — but the obvious way
/// to fix that break was a `_ =>` arm, which would have granted every future backend
/// the trusting path in one line and in silence. There is no such arm to add now.
///
/// Making `guard` an `Option<GuardFactory>`, adding a `Default`, or marking the struct
/// `#[non_exhaustive]` would each hand that failure mode straight back. Do none of them.
#[derive(Debug)]
pub struct BackendVendor {
    /// The vendor's capability row, the same `&'static BackendDescriptor`
    /// `zero-migrate-ir` keys its registry by.
    pub descriptor: &'static BackendDescriptor,
    /// How this vendor spells DML, views and triggers.
    pub dml: &'static dyn DmlRenderer,
    /// How this vendor spells columns and DDL.
    pub schema: &'static dyn SchemaRenderer,
    /// How this vendor renders and recognizes logical value-format contracts.
    ///
    /// Required, never defaulted. Catalog normalization is a vendor fact just as
    /// surely as emitted DDL is; a future backend must write its own answer.
    pub value_format: &'static dyn ValueFormatRenderer,
    /// How this vendor spells schema-changing statements.
    ///
    /// Required, never defaulted. A vendor cannot silently inherit another backend's
    /// DDL or disappear behind a catch-all registry arm.
    pub ddl: DdlFactory,
    /// What this vendor REFUSES to run — its line-1 defense.
    ///
    /// Required, never defaulted. A vendor that trusts its input must say so by
    /// writing a [`MigrationGuard`] whose `check` returns `Ok(GuardOutcome::default())`
    /// — visible in the diff, attributable to the vendor, and impossible to acquire by
    /// forgetting something.
    ///
    /// [`GuardOutcome`]: crate::guard::GuardOutcome
    pub guard: GuardFactory,
}

/// The "a vendor that ships no guard does not compile" property, pinned as a
/// `compile_fail` doctest so it is checked rather than asserted in prose.
///
/// A doctest compiles as a SEPARATE crate that `use`s `zero_migrate_backend`, which is
/// exactly the position a new backend crate sits in.
///
/// READ THIS BEFORE EDITING. A `compile_fail` doctest passes when the code fails to
/// compile for ANY reason, so it silently stops testing anything the moment a name in
/// it goes stale. Both blocks below are deliberately written to fail for exactly ONE
/// reason, and both were verified BY INVERSION — supply the missing thing, drop the
/// `compile_fail`, and confirm the same code compiles.
///
/// That check is not ceremony. The first draft of block (1) wrote the vendor as a
/// `static` with `unimplemented!()` renderers, and it passed for the WRONG reason:
/// `unimplemented!()` in a `static` is a const-eval error (E0080) that fires whether
/// or not `guard` is present, so the test would have kept passing after the field was
/// made optional. Taking the renderers as parameters of a `fn` is what leaves the
/// missing field as the only defect.
///
/// The property was also confirmed against the real tree: deleting
/// `guard: guard::guard` from `zero-migrate-sqlite`'s live `VENDOR` literal produces
/// `error[E0063]: missing field `guard` in initializer of `BackendVendor``, pointing
/// at that vendor's own crate.
///
/// (1) A `BackendVendor` without a guard MUST fail to compile — E0063:
///
/// ```compile_fail
/// use zero_migrate_backend::registry::BackendVendor;
/// use zero_migrate_backend::registry::DdlFactory;
/// use zero_migrate_backend::renderer::DmlRenderer;
/// use zero_migrate_backend::schema::SchemaRenderer;
/// use zero_migrate_backend::value_format::ValueFormatRenderer;
/// use zero_migrate_ir::backend::BackendDescriptor;
/// fn vendor(
///     descriptor: &'static BackendDescriptor,
///     dml: &'static dyn DmlRenderer,
///     schema: &'static dyn SchemaRenderer,
///     value_format: &'static dyn ValueFormatRenderer,
///     ddl: DdlFactory,
/// ) -> BackendVendor {
///     BackendVendor {
///         descriptor,
///         dml,
///         schema,
///         value_format,
///         ddl,
///     }
/// }
/// ```
///
/// (2) And a `MigrationGuard` impl that supplies no `check` MUST fail to compile —
/// E0046. The trait method has no default body, so trusting the input cannot be
/// inherited; it has to be written:
///
/// ```compile_fail
/// struct TrustsEverything;
/// impl zero_migrate_backend::guard::MigrationGuard for TrustsEverything {}
/// ```
#[cfg(doctest)]
struct VendorWithoutAGuardCompileFail;

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
    pub fn get(self, id: &zero_migrate_ir::dialect::DialectId) -> Option<&'static BackendVendor> {
        self.vendors
            .iter()
            .copied()
            .find(|v| &v.descriptor.id == id)
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
