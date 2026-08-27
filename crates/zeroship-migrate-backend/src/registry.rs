//! What a BACKEND CRATE hands the engine, and the shape the engine composes them in.
//!
//! # Why this exists and `zeroship_migrate_ir::backend::BackendRegistry` does not suffice
//!
//! `zeroship-migrate-ir` already has a `BackendRegistry`, built fallibly, refusing
//! duplicate and malformed ids and naming both registrants on a collision. It is
//! reused verbatim by [`VendorSet::descriptors`] and it is the right thing for what
//! it holds - but what it holds is `&'static BackendDescriptor`, i.e. a vendor's
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
//! the former closed dialect enum, naming `postgres::RENDERER`, `sqlite::RENDERER`
//! and `mysql::RENDERER` - three statics in the same crate as the trait. Extract the
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
//! global. Dispatch now looks up the open [`DialectId`](zeroship_migrate_ir::dialect::DialectId)
//! filed by each descriptor, so the contract contains no enum match and a fourth
//! backend requires no contract edit. Engine callers pass that open id directly.

use crate::advisory::OperationalAdvisor;
use crate::attribute::AttributeVocabulary;
use crate::ddl::DdlEmitter;
use crate::existence_probe::ExistenceProbePolicy;
use crate::fold::CatalogFoldPolicy;
use crate::guard::{GuardConfig, MigrationGuard};
use crate::renderer::DmlRenderer;
use crate::schema::SchemaRenderer;
use crate::validation::ValidationPolicy;
use crate::value_format::ValueFormatRenderer;
use zeroship_migrate_ir::backend::{BackendDescriptor, BackendRegistry, RegistryError};

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
/// names it. Nothing else of a vendor crate's surface is public API - the renderer
/// structs themselves stay crate-private, so a caller cannot reach past this
/// descriptor to a vendor's spelling without going through a registry.
///
/// # Every field is REQUIRED, and `guard` is why that matters
///
/// This struct derives no `Default`, has no `Default` impl, and is not
/// `#[non_exhaustive]`. Every field must be written out in a struct literal at the
/// vendor's own definition site. A new backend that ships no DDL emitter, no guard or
/// no advisor therefore fails to compile **in its own crate, named** - E0063 for the
/// missing field - rather than picking one up by omission.
///
/// That is the whole point of the field. It replaced a `guard_for(cfg)` function whose
/// match on the former closed dialect enum handed both descriptor-only dialects one shared
/// trusting guard.
/// The match was exhaustive, so a fourth dialect broke the build - but the obvious way
/// to fix that break was a `_ =>` arm, which would have granted every future backend
/// the trusting path in one line and in silence. There is no such arm to add now.
///
/// Making `guard` an `Option<GuardFactory>`, adding a `Default`, or marking the struct
/// `#[non_exhaustive]` would each hand that failure mode straight back. Do none of them.
#[derive(Debug)]
pub struct BackendVendor {
    /// The vendor's capability row, the same `&'static BackendDescriptor`
    /// `zeroship-migrate-ir` keys its registry by.
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
    /// How this vendor's catalog identities behave under existence probes.
    ///
    /// Required, never defaulted. A future backend must explicitly state whether
    /// unique indexes carry constraint identity, whether a constraint miss proves
    /// absence, how constraint definitions normalize, and whether its catalog
    /// silently truncates identifiers.
    pub existence_probe: &'static dyn ExistenceProbePolicy,
    /// How this vendor's catalog semantics shape the shared structural fold.
    ///
    /// Required, never defaulted. A future backend must state its implicit-name,
    /// rowid, primary-key, named-type, CHECK-scope, and physical-type behavior in
    /// its own crate.
    pub catalog_fold: &'static dyn CatalogFoldPolicy,
    /// Backend-owned authoring-validation facts and exact refusals.
    ///
    /// Required, never defaulted: a future backend must state its identifier,
    /// namespace, and unsupported-shape policy in its own crate. (Its PARTITION
    /// posture used to be one of these answers; it is now
    /// `Capability::PartitionRelationDdl` on `descriptor`, so the render and
    /// validate layers ask one question instead of two.)
    pub validation: &'static dyn ValidationPolicy,
    /// How this vendor spells schema-changing statements.
    ///
    /// Required, never defaulted. A vendor cannot silently inherit another backend's
    /// DDL or disappear behind a catch-all registry arm.
    pub ddl: DdlFactory,
    /// What this vendor REFUSES to run - its line-1 defense.
    ///
    /// Required, never defaulted. A vendor that trusts its input must say so by
    /// writing a [`MigrationGuard`] whose `check` returns `Ok(GuardOutcome::default())`
    /// - visible in the diff, attributable to the vendor, and impossible to acquire by
    ///   forgetting something.
    ///
    /// [`GuardOutcome`]: crate::guard::GuardOutcome
    pub guard: GuardFactory,
    /// What this vendor says about a migration's OPERATIONAL risk - the advisory
    /// half of the contract, next to `guard`'s security half.
    ///
    /// Required, never defaulted, for the same reason `guard` is. A backend that
    /// ships no analyzer must say so by returning
    /// [`AdvisoryVerdict::NotAnalyzed`](crate::advisory::AdvisoryVerdict::NotAnalyzed)
    /// with its own reason - visible in the diff, attributable to the vendor, and
    /// impossible to acquire by forgetting something. An `Option` here, or a default
    /// body on either trait method, would hand a future backend a SILENTLY empty
    /// advisory report, which is exactly the "unchecked reads as clean" defect
    /// [`AnalyzerAbsent`](crate::advisory::AnalyzerAbsent) documents.
    ///
    /// A `&'static dyn` rather than a factory because an analyzer is stateless: it
    /// reads SQL and nothing else. `guard` is a `fn` pointer only because it carries
    /// the per-migration [`GuardConfig`] it decides against.
    pub advisor: &'static dyn OperationalAdvisor,
    /// The vendor knobs this backend OWNS - which `<dialect>.<name>` keys exist, which
    /// IR node each attaches to, and what a legal value is.
    ///
    /// Required, never defaulted, and for a sharper reason than the fields above. The
    /// attribute space is the one part of the IR a backend extends WITHOUT editing a
    /// neutral crate, so this field is the entire mechanism by which it does so. A
    /// backend that declares nothing must say so with
    /// [`AttributeVocabulary::empty()`](crate::attribute::AttributeVocabulary::empty) -
    /// one visible line in its own crate - because "declares no attributes" and "forgot
    /// to declare attributes" produce identical behaviour at every later layer, and only
    /// the diff can tell them apart.
    ///
    /// Not an `Option`, and no `Default`: either would let a new backend acquire an empty
    /// vocabulary by omission, and an empty vocabulary REFUSES every attribute of its own
    /// dialect. That failure is at least loud. The dangerous direction is the reverse -
    /// see [`crate::attribute`] for why a key belonging to an unasked dialect is skipped
    /// rather than refused, which is what makes a missing declaration invisible on every
    /// target except the vendor's own.
    pub attributes: AttributeVocabulary,
}

/// The "a vendor that ships no guard does not compile" property, pinned as a
/// `compile_fail` doctest so it is checked rather than asserted in prose.
///
/// A doctest compiles as a SEPARATE crate that `use`s `zeroship_migrate_backend`, which is
/// exactly the position a new backend crate sits in.
///
/// READ THIS BEFORE EDITING. A `compile_fail` doctest passes when the code fails to
/// compile for ANY reason, so it silently stops testing anything the moment a name in
/// it goes stale. Both blocks below are deliberately written to fail for exactly ONE
/// reason, and both were verified BY INVERSION - supply the missing thing, drop the
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
/// `guard: guard::guard` from `zeroship-migrate-sqlite`'s live `VENDOR` literal produces
/// `error[E0063]: missing field `guard` in initializer of `BackendVendor``, pointing
/// at that vendor's own crate.
///
/// (1) A `BackendVendor` without a guard MUST fail to compile - E0063:
///
/// ```compile_fail
/// use zeroship_migrate_backend::registry::BackendVendor;
/// use zeroship_migrate_backend::registry::DdlFactory;
/// use zeroship_migrate_backend::renderer::DmlRenderer;
/// use zeroship_migrate_backend::advisory::OperationalAdvisor;
/// use zeroship_migrate_backend::existence_probe::ExistenceProbePolicy;
/// use zeroship_migrate_backend::fold::CatalogFoldPolicy;
/// use zeroship_migrate_backend::schema::SchemaRenderer;
/// use zeroship_migrate_backend::value_format::ValueFormatRenderer;
/// use zeroship_migrate_backend::validation::ValidationPolicy;
/// use zeroship_migrate_ir::backend::BackendDescriptor;
/// fn vendor(
///     descriptor: &'static BackendDescriptor,
///     dml: &'static dyn DmlRenderer,
///     schema: &'static dyn SchemaRenderer,
///     value_format: &'static dyn ValueFormatRenderer,
///     existence_probe: &'static dyn ExistenceProbePolicy,
///     catalog_fold: &'static dyn CatalogFoldPolicy,
///     validation: &'static dyn ValidationPolicy,
///     ddl: DdlFactory,
///     advisor: &'static dyn OperationalAdvisor,
/// ) -> BackendVendor {
///     BackendVendor {
///         descriptor,
///         dml,
///         schema,
///         value_format,
///         existence_probe,
///         catalog_fold,
///         validation,
///         ddl,
///         advisor,
///     }
/// }
/// ```
///
/// (2) And an EMPTY `MigrationGuard` impl MUST fail to compile - E0046. Not one of
/// the trait's methods has a default body, so no part of a security posture can be
/// inherited by omission; every one has to be written. That covers `check`, both raw
/// island backstops, the `require_rls` raw-door question, and the SQL flag
/// derivation - a vendor that stays silent on any of them does not build:
///
/// ```compile_fail
/// struct TrustsEverything;
/// impl zeroship_migrate_backend::guard::MigrationGuard for TrustsEverything {}
/// ```
///
/// (3) The same property for `advisor`, which is a SEPARATE block precisely because
/// each of these must fail for exactly one reason. Here `guard` IS supplied and
/// `advisor` is the only omission, so this block goes green the moment the advisor
/// field acquires a `Default`, an `Option`, or a `#[non_exhaustive]` escape hatch -
/// which is the failure mode it exists to catch:
///
/// ```compile_fail
/// use zeroship_migrate_backend::registry::BackendVendor;
/// use zeroship_migrate_backend::registry::{DdlFactory, GuardFactory};
/// use zeroship_migrate_backend::renderer::DmlRenderer;
/// use zeroship_migrate_backend::existence_probe::ExistenceProbePolicy;
/// use zeroship_migrate_backend::fold::CatalogFoldPolicy;
/// use zeroship_migrate_backend::schema::SchemaRenderer;
/// use zeroship_migrate_backend::value_format::ValueFormatRenderer;
/// use zeroship_migrate_backend::validation::ValidationPolicy;
/// use zeroship_migrate_ir::backend::BackendDescriptor;
/// fn vendor(
///     descriptor: &'static BackendDescriptor,
///     dml: &'static dyn DmlRenderer,
///     schema: &'static dyn SchemaRenderer,
///     value_format: &'static dyn ValueFormatRenderer,
///     existence_probe: &'static dyn ExistenceProbePolicy,
///     catalog_fold: &'static dyn CatalogFoldPolicy,
///     validation: &'static dyn ValidationPolicy,
///     ddl: DdlFactory,
///     guard: GuardFactory,
/// ) -> BackendVendor {
///     BackendVendor {
///         descriptor,
///         dml,
///         schema,
///         value_format,
///         existence_probe,
///         catalog_fold,
///         validation,
///         ddl,
///         guard,
///     }
/// }
/// ```
///
/// (4) And an `OperationalAdvisor` impl that supplies no method MUST fail to
/// compile - E0046, for the same reason as (2): no default body, so "this backend
/// ships no analyzer" cannot be inherited in silence.
///
/// ```compile_fail
/// #[derive(Debug)]
/// struct AnalyzesNothing;
/// impl zeroship_migrate_backend::advisory::OperationalAdvisor for AnalyzesNothing {}
/// ```
#[cfg(doctest)]
struct VendorWithoutAGuardCompileFail;

/// The set of backends a build ships.
///
/// Deliberately a slice of `&'static BackendVendor` rather than a map: the shipping
/// set is a compile-time fact, and a set that could grow at run time would let a
/// duplicate id in behind [`BackendRegistry`]'s check - which is the same reason
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

    /// The registered backend identities, in composition order.
    ///
    /// Consumers that need the shipping census derive it from this iterator;
    /// they never restate a second vendor list in core.
    pub fn dialects(self) -> impl Iterator<Item = zeroship_migrate_ir::dialect::DialectId> {
        self.vendors
            .iter()
            .map(|vendor| vendor.descriptor.id.clone())
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
    pub fn get(self, id: &zeroship_migrate_ir::dialect::DialectId) -> Option<&'static BackendVendor> {
        self.vendors
            .iter()
            .copied()
            .find(|v| &v.descriptor.id == id)
    }

    /// Validate this set's descriptors into `-ir`'s [`BackendRegistry`].
    ///
    /// This is the composition the hard-coded `match` used to stand in for, and it
    /// is where a duplicate or malformed id is caught - by the leaf crate's builder,
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

impl zeroship_migrate_ir::validate::ExprDialectValidatorSet for VendorSet {
    fn get(
        &self,
        dialect: &zeroship_migrate_ir::dialect::DialectId,
    ) -> Option<&dyn zeroship_migrate_ir::validate::ExprDialectValidator> {
        VendorSet::get(*self, dialect).map(|vendor| vendor.dml.expr_validator())
    }
}
