//! Backend-owned authoring-validation policy.
//!
//! Core owns the structural walk and error envelope. A backend owns facts such
//! as identifier comparison, catalog namespaces, native partition support, and
//! the exact refusal it wants for a vendor-specific unsupported shape. Every
//! method is required so a newly registered backend cannot silently inherit one
//! of the shipping engines' answers.

use zeroship_migrate_ir::capability::VendorCapability;
use zeroship_migrate_ir::ir::ColType;
use zeroship_migrate_ir::policy::SchemaScope;

/// One backend's disposition for a closed operation-shape token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Core construct that this backend renders and validates.
    Portable,
    /// Native where supported and explicitly absence-tolerable otherwise.
    TransparentDegradable,
    /// Vendor-tier construct admitted by this backend.
    Vendor,
    /// Explicit fail-closed refusal.
    Unsupported,
}

impl Disposition {
    /// Whether this disposition admits the token on its backend - everything
    /// except an explicit `Unsupported` refusal renders and validates.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Disposition::Unsupported)
    }
}

/// A backend-authored validation refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationRefusal {
    /// Exact operator-facing explanation.
    pub reason: String,
    /// Exact operator-facing remedy.
    pub suggested_fix: String,
}

/// Vendor facts consumed by the neutral authoring validator.
pub trait ValidationPolicy: std::fmt::Debug + Sync {
    /// This backend's disposition for one closed `(kind, variant)` token.
    ///
    /// Implementations must explicitly fail closed for an unfamiliar token.
    fn op_disposition(&self, kind: &str, variant: &str) -> Disposition;

    /// Canonical key used when this backend compares unquoted identifiers.
    fn canonical_identifier(&self, identifier: &str) -> String;

    /// Whether native UUID storage and/or a recovered CHECK proves UUID format.
    fn catalog_proves_uuid_format(
        &self,
        native_uuid: bool,
        catalog_uuid_format_check: bool,
    ) -> bool;

    /// Canonical physical storage used for authored reference compatibility.
    fn lowered_reference_storage(&self, ty: &ColType) -> String;

    /// Whether explicit constraint names must be unique within one table.
    fn tracks_constraint_names(&self) -> bool;

    /// Whether relations and named types share this backend's type namespace.
    fn tracks_relation_type_namespace(&self) -> bool;

    // NOTE: a native-partitioning predicate used to live here. It was a THIRD
    // spelling of a fact this contract already carried twice: `DdlEmitter`'s four
    // required partition methods answer it by `Option`, and the render layer asked
    // it as `dialect != POSTGRES`. It is now one question in the vocabulary both
    // layers share - `Capability::PartitionRelationDdl`, off the backend's own
    // descriptor - and the shipping census in
    // `crates/zero-migrate/tests/dialect_matrix/vendor_registry_owns_shipping_descriptors.rs`
    // holds that answer against the emitters, which this method never did.
    /// A fail-closed backend refusal before the operator-capability gate.
    fn vendor_capability_refusal(&self, capability: VendorCapability) -> Option<ValidationRefusal>;

    /// Exact refusal when this backend lacks deferrable foreign keys.
    fn deferrable_foreign_key_refusal(&self) -> ValidationRefusal;

    /// Exact refusal when this backend lacks closed inline trigger bodies.
    fn inline_trigger_body_refusal(&self) -> ValidationRefusal;

    /// Refuse an event count this backend's trigger grammar cannot represent.
    fn trigger_event_count_refusal(&self, event_count: usize) -> Option<ValidationRefusal>;

    /// Exact refusal when this backend lacks standalone sequence defaults.
    fn sequence_default_refusal(&self, position: &str) -> ValidationRefusal;

    /// Exact refusal when this backend lacks virtual generated columns.
    fn virtual_generated_column_refusal(&self, column: &str) -> ValidationRefusal;

    /// Validate the backend's placement constraints for one integer identity.
    fn identity_placement_refusal(
        &self,
        column: &str,
        always: bool,
        primary_key_columns: Option<&[String]>,
        is_add_column: bool,
    ) -> Option<ValidationRefusal>;

    /// Vet a `ViewQuery::Raw` body in THIS backend's grammar. `None` admits it.
    ///
    /// The engine holds the authoring envelope - which op, which dialect, which
    /// error code - but it must not hold a parser. Until this method existed it
    /// did: `validate_raw_view_body_sql` called `pg_query::parse` directly, so a
    /// MySQL or SQLite raw view body was vetted against PostgreSQL's grammar and a
    /// backtick- or bracket-quoted identifier was refused on its own dialect with a
    /// PostgreSQL syntax error. That is the defect this method removes.
    ///
    /// `scope` is the authoring-time schema confinement, threaded so a backend that
    /// scans for cross-schema reach can honour it.
    ///
    /// # This is where trust is granted, so it is required
    ///
    /// Like every other method here, this one has no default body. A backend with
    /// no parser must WRITE `None`, which is a visible grant of trust attributable
    /// to that vendor, rather than inherit one by omission. Returning `None` means
    /// the body is admitted with no shape gate and no deny-list scan at all - see
    /// each vendor's impl for what that specifically costs there.
    fn raw_view_body_refusal(
        &self,
        sql: &str,
        scope: Option<&SchemaScope>,
    ) -> Option<ValidationRefusal>;
}
