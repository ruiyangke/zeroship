//! The confinement settings PostgreSQL reads, and the seam a host sets them through.
//!
//! # Why this is not a field on the neutral connection config
//!
//! It was one: `ConfinementConfig::postgres`, a `PostgresConfinement` declared in
//! `zeroship-migrate-backend`. The doc beside it argued - correctly - that these are
//! "genuinely one vendor's" rather than a shared concept wearing a vendor hat, and
//! that the reason they stayed was that no carrier for RUN-TIME vendor data existed:
//! [`BackendVendor`](zeroship_migrate_backend::registry::BackendVendor) holds
//! `&'static dyn` policy objects, and a migrator role is per-project host input.
//!
//! That argument was about the absence of a mechanism, not about the name being
//! right. A vendor name in a neutral crate is a violation however well the comment
//! beside it reads. The mechanism now exists and does not need to live in the static
//! vendor table - it only needs to be keyed the same way, by
//! [`DialectId`](zeroship_migrate_ir::dialect::DialectId).
//!
//! # What an absent leg means
//!
//! The host supplied no PostgreSQL settings, and [`of`] answers with this crate's own
//! [`PostgresConfinement::default`] - no `SET ROLE`, `public` as the extension
//! resolution schema. That is exactly what the neutral `ConfinementConfig::default`
//! used to install eagerly, so a miss reproduces the old default rather than
//! inventing one, and it cannot be a silently wrong answer because only this crate
//! reads this crate's leg.

use std::any::Any;
use std::sync::{Arc, LazyLock};

use crate::DIALECT;
use zeroship_migrate_backend::conn::ExecutorConfig;
use zeroship_migrate_backend::dialectal::{DialectalValue, VendorConfinement};

/// The confinement settings **only the PostgreSQL backend reads**.
///
/// The MySQL and SQLite backends read neither field, and would have nothing to do
/// with them if they did - MySQL has no `SET ROLE`-per-transaction confinement
/// model and SQLite has neither roles nor schemas.
///
/// # Where they are read from, measured
///
/// This doc used to claim every field was referenced "solely from
/// `apply/backend/postgres/` and the precondition evaluator". Half of that has
/// become true - the precondition evaluator IS the PostgreSQL backend now - and the
/// other half was never true, which is why the claim is replaced by the measurement
/// rather than trimmed. (That backend is `zeroship-migrate-postgres/src/backend/` since
/// the execution half left the engine; the paths below are relative to it.)
///
/// `migrator_role` is read only from the PostgreSQL backend
/// (`session`, `backfill_sql`, `primary_key_sql`, `precondition`) and written by
/// [`ExecutorConfig::with_migrator_role`], the host's provisioning seam.
///
/// `extension_schemas` is read from exactly ONE place, and that place is now the
/// PostgreSQL backend too: `search_path_clause`, in that crate's
/// `backend/session.rs`. It used to be a method on the neutral
/// [`ExecutorConfig`] in this file, and this doc named that as the real reason the
/// neutral [`ConfinementConfig`](zeroship_migrate_backend::conn::ConfinementConfig) still carried a vendor-typed field - "relocating
/// the field without first relocating `search_path_clause` would only move the
/// coupling". That relocation has happened: a `search_path` is PostgreSQL's
/// concept, all three callers were already in that file, and all three passed
/// `POSTGRES` as the dialect.
///
/// So what is left here is only DATA, and only the vendor that reads it reads it -
/// which is why the type is HERE now rather than on the neutral `ConfinementConfig`.
/// The blocker this doc used to name was real and is gone: `BackendVendor` holds
/// `&'static dyn` policy objects and these are per-project host input, so they could
/// not live there. They did not need to. A carrier for run-time vendor data does not
/// have to live in the static vendor table; it only has to be keyed the same way, and
/// [`Dialectal`](zeroship_migrate_backend::dialectal::Dialectal) keyed by
/// [`DialectId`](zeroship_migrate_ir::dialect::DialectId) is that.
#[derive(Debug, Clone)]
pub struct PostgresConfinement {
    /// The least-privilege `migrator` role the apply flow runs each migration's
    /// DDL + journal writes under, via `SET ROLE` / `RESET ROLE` (the
    /// DB-privilege defense layer). `None` runs as the connecting
    /// (admin) role - used only by tests / single-tenant dev where the role
    /// model is not provisioned. In the platform this is always `Some`, matching
    /// the deterministic name returned by the PostgreSQL backend crate's
    /// `role::migrator_role_name` and provisioned by the host.
    pub migrator_role: Option<String>,
    /// The schema(s) that host shared **extension types/functions** the engine
    /// emits UNQUALIFIED (e.g. pgvector's `vector(N)`, `PostGIS`'s
    /// `geography(POINT,4326)`). pgvector / `PostGIS` install into `public` on the
    /// platform image (and the dev `pgvector/pgvector:pg16`), so this defaults to
    /// `["public"]`.
    ///
    /// These schemas are appended (after the project schema) to the migrator's
    /// `search_path` so unqualified extension types/functions RESOLVE, and the
    /// migrator is granted **`USAGE` only** on them (lookup, never CREATE/write).
    /// This matches plugin-db's RUNTIME, which references the same unqualified
    /// `vector`/`geography` types with `public` reachable on its connection path.
    ///
    /// SECURITY: `USAGE` permits *resolving* objects in the schema; it does NOT
    /// permit creating objects there (that needs `CREATE`, which stays revoked)
    /// nor writing existing tables (that needs per-table grants the migrator never
    /// receives). So the cross-schema **write** confinement is unchanged - these
    /// schemas are resolution-only.
    pub extension_schemas: Vec<String>,
}

impl Default for PostgresConfinement {
    /// No `SET ROLE` (the platform sets it via
    /// [`ExecutorConfig::with_migrator_role`]) and `public` as the
    /// extension-type resolution schema.
    fn default() -> Self {
        Self {
            // Defaults to no SET ROLE; the platform sets this to the provisioned
            // deterministic per-project migrator role. Tests opt in explicitly.
            migrator_role: None,
            // Extension types/functions (pgvector `vector`, PostGIS `geography`)
            // live in `public` on the platform/dev image. Resolution-only; the
            // migrator gets USAGE (not CREATE) on these - see the field doc.
            extension_schemas: vec!["public".to_string()],
        }
    }
}

impl DialectalValue for PostgresConfinement {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn dialectal_eq(&self, other: &dyn Any) -> bool {
        other.downcast_ref::<Self>().is_some_and(|other| {
            self.migrator_role == other.migrator_role
                && self.extension_schemas == other.extension_schemas
        })
    }
}

impl VendorConfinement for PostgresConfinement {}

/// The default a config with no PostgreSQL leg resolves to.
///
/// A `static` rather than a fresh `default()` per call so [`of`] can hand back a
/// borrow, which keeps every caller reading a `&PostgresConfinement` whether the host
/// supplied one or not - the miss is invisible at the call site, which is what makes
/// it safe to have one.
static ABSENT: LazyLock<PostgresConfinement> = LazyLock::new(PostgresConfinement::default);

/// The PostgreSQL confinement `cfg` carries, or this crate's default when the host
/// set none.
///
/// This is the ONLY read seam. Every `SET ROLE` bracket and every `search_path`
/// clause in this crate comes through here, so "the host said nothing" and "the host
/// said the default" cannot diverge between call sites.
#[must_use]
pub fn of(cfg: &ExecutorConfig) -> &PostgresConfinement {
    cfg.confinement
        .vendor
        .get::<PostgresConfinement>(&DIALECT)
        .unwrap_or(&ABSENT)
}

/// Install `confinement` as PostgreSQL's leg of `cfg`.
pub fn set(cfg: &mut ExecutorConfig, confinement: PostgresConfinement) {
    cfg.confinement
        .vendor
        .insert(DIALECT, Arc::new(confinement) as Arc<dyn VendorConfinement>);
}

/// The host's provisioning seam for the settings only this backend reads.
///
/// An extension trait rather than an inherent method because
/// [`ExecutorConfig`] is declared in the neutral crate, which cannot name this one.
/// `with_migrator_role` used to be an inherent builder method there, writing a
/// PostgreSQL-named field; the neutral crate no longer knows the concept exists.
pub trait PostgresConfinementExt {
    /// Set the least-privilege `migrator_role` the apply flow runs migrations under.
    /// Builder convenience, and the seam the platform's provisioned role arrives by.
    #[must_use]
    fn with_migrator_role(self, role: impl Into<String>) -> Self;

    /// Set the schemas whose extension types/functions the migrator's `search_path`
    /// must reach. Builder convenience.
    #[must_use]
    fn with_extension_schemas(self, schemas: Vec<String>) -> Self;
}

impl PostgresConfinementExt for ExecutorConfig {
    fn with_migrator_role(mut self, role: impl Into<String>) -> Self {
        let mut confinement = of(&self).clone();
        confinement.migrator_role = Some(role.into());
        set(&mut self, confinement);
        self
    }

    fn with_extension_schemas(mut self, schemas: Vec<String>) -> Self {
        let mut confinement = of(&self).clone();
        confinement.extension_schemas = schemas;
        set(&mut self, confinement);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ExecutorConfig {
        ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"))
    }

    #[test]
    fn a_config_with_no_leg_reads_this_crates_own_default() {
        let cfg = cfg();
        assert!(cfg.confinement.vendor.is_empty());
        // The miss is invisible: `of` answers with the same values the neutral
        // constructor used to install eagerly.
        assert_eq!(of(&cfg).migrator_role, None);
        assert_eq!(of(&cfg).extension_schemas, vec!["public".to_string()]);
    }

    #[test]
    fn the_provisioning_seam_installs_a_leg_the_read_seam_finds() {
        let cfg = cfg().with_migrator_role("migrator_prj_x");
        assert!(cfg.confinement.vendor.carries(&DIALECT));
        assert_eq!(of(&cfg).migrator_role.as_deref(), Some("migrator_prj_x"));
        // The other field keeps its default rather than being reset by the builder.
        assert_eq!(of(&cfg).extension_schemas, vec!["public".to_string()]);
    }

    #[test]
    fn each_builder_preserves_what_the_other_set() {
        let cfg = cfg()
            .with_extension_schemas(vec!["ext".to_string()])
            .with_migrator_role("migrator_prj_x");
        assert_eq!(of(&cfg).migrator_role.as_deref(), Some("migrator_prj_x"));
        assert_eq!(of(&cfg).extension_schemas, vec!["ext".to_string()]);
    }
}
