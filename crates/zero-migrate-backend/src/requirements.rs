//! What a lowered plan needs the LIVE target to be able to do.
//!
//! A version-gated database feature is not a spelling question, so it is not a
//! renderer's answer, and it is not migration identity, so it is not in the
//! checksum. It is a claim about the server on the other end of the connection,
//! which is exactly what `MigrationBackend::verify_database_requirements` is
//! asked to check before a plan's first authored step runs. The vocabulary of
//! that question therefore sits with the backend contract rather than in the
//! engine that composes it.
//!
//! The set is deliberately CLOSED and small. Each variant names a feature whose
//! exact IR lowering the engine cannot render around - a generator function or an
//! enforced constraint the target either has or has not - never a general
//! capability probe. The engine re-exports both types at
//! `zero_migrate::render::plan`.

/// A database feature whose exact IR lowering has live target requirements.
///
/// A VERSION FLOOR IS NOT HERE, and its absence is the point. The enum carried a
/// `minimum_postgres_version_num` for a while: one backend's version-number scheme,
/// one backend's release history, as a method on the neutral question. Its only
/// production caller was that backend's own `verify_database_requirements`, which is
/// exactly the seam the trait exists to route through - the engine asks WHAT a plan
/// needs, and each target answers whether it has it, in whatever terms its own
/// server versions come in. It is `zero_migrate_postgres::backend::minimum_server_version_num`
/// now, private to the crate that reads it.
/// These are derived from the typed expression AST and carried on the complete
/// engine's `AppliedPlan` so apply can check them before any authored step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DatabaseFeature {
    /// Exact RFC 9562 UUIDv4 database generation: PostgreSQL's core
    /// `gen_random_uuid()` or the capability-gated MySQL synthesis.
    UuidV4Generation,
    /// Exact RFC 9562 UUIDv7 generation through PostgreSQL's core `uuidv7()`
    /// function.
    UuidV7Generation,
    /// Enforced canonical UUID text checks on MySQL. MySQL parsed but ignored
    /// `CHECK` constraints before 8.0.16.
    UuidValidation,
    /// Enforced canonical TypeID format checks on MySQL. MySQL parsed but
    /// ignored `CHECK` constraints before 8.0.16.
    TypeIdValidation,
    /// Enforced canonical ULID format checks on MySQL. MySQL parsed but ignored
    /// `CHECK` constraints before 8.0.16.
    UlidValidation,
}

impl DatabaseFeature {
    /// Operator-facing feature description for a target-capability error.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::UuidV4Generation => "exact RFC 9562 UUIDv4 database generation",
            Self::UuidV7Generation => "exact RFC 9562 UUIDv7 database generation",
            Self::UuidValidation => "canonical UUID format validation",
            Self::TypeIdValidation => "canonical TypeID format validation",
            Self::UlidValidation => "canonical ULID format validation",
        }
    }
}

/// The deduplicated database capabilities one lowered plan requires.
///
/// Requirements are execution metadata, not migration identity: the originating
/// expression nodes are already folded into the canonical IR checksum, while the
/// connected server version is an apply-time fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseRequirements {
    features: std::collections::BTreeSet<DatabaseFeature>,
}

impl DatabaseRequirements {
    /// Add one required database feature. Repeated expressions deduplicate.
    pub fn require(&mut self, feature: DatabaseFeature) {
        self.features.insert(feature);
    }

    /// Whether this plan needs no version-gated database feature.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// Iterate required features in stable order.
    pub fn iter(&self) -> impl Iterator<Item = DatabaseFeature> + '_ {
        self.features.iter().copied()
    }
}
