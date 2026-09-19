//! Physical names derived from a canonical [`DatabaseId`] or [`BindingId`].
//!
//! A database is a schema owned by a project, and a binding is one app's edge
//! to it with one capability. Both are named in `PostgreSQL` by names nobody
//! stores: the schema, the three roles a database has, and the role a binding
//! narrows to at one schema epoch are all derived here. Hoisting them is the
//! same property [`crate::app_derivation`] exists for - the cluster
//! reconciler, the migration service and the data plane must not be able to
//! answer "what is this database's schema called" differently, because the
//! three of them create it, apply DDL to it and read it.
//!
//! The composers taking text live in [`crate::database_role`], which is where
//! the identifier-length refusal is applied and where a name too long for
//! `PostgreSQL` can be exhibited. This module is the typed seam over them and
//! introduces no second opinion: it returns their refusal unchanged.

use crate::binding_id::BindingId;
use crate::database_id::DatabaseId;
use crate::database_role::{self, DatabaseCapability, RoleNameTooLong};

/// The physical `PostgreSQL` schema this database's tables live in.
///
/// Derived and never stored, so there is no row that can disagree with the
/// cluster. It is the tenant boundary: the migration service creates it, the
/// migrator role owns it, and a binding reaches exactly this schema and no
/// other.
///
/// [`crate::schema_name::SchemaName`] is the validator this spelling has to
/// pass, and every caller puts it through that type before it reaches DDL:
/// that is what keeps a name needing quotes - or carrying one - out of
/// composed SQL. `tests::the_derived_schema_name_validates` binds it.
#[must_use]
pub fn schema_name(database: &DatabaseId) -> String {
    format!("db_{}", database.as_str())
}

/// The role that OWNS this database's schema and applies its DDL.
///
/// It belongs to the migration service. No binding names it and no capability
/// composes it, so schema change is not reachable from an app's role
/// membership - which is the whole of the privilege invariant here: the worker
/// runs creator code, so anything it can assume is something creator code can
/// use.
///
/// # Errors
///
/// [`RoleNameTooLong`] if the composed name exceeds `PostgreSQL`'s identifier
/// limit. Refused rather than shortened, because a truncated name resolves to
/// a DIFFERENT database's role.
pub fn migrator_role_name(database: &DatabaseId) -> Result<String, RoleNameTooLong> {
    database_role::database_migrator_role_name(database.as_str())
}

/// One of this database's two capability roles.
///
/// A binding role inherits exactly one of these `WITH SET FALSE`, so an app's
/// statements are confined to one database's grants while revoking that app's
/// edge still removes its access. The roles are per database and not per app
/// precisely so that the grants they carry are stated once.
///
/// # Errors
///
/// [`RoleNameTooLong`], on the terms [`migrator_role_name`] gives.
pub fn capability_role_name(
    database: &DatabaseId,
    capability: DatabaseCapability,
) -> Result<String, RoleNameTooLong> {
    database_role::database_capability_role_name(database.as_str(), capability)
}

/// The role this binding narrows to at this schema epoch.
///
/// `SET LOCAL ROLE` authorizes against the memberships of the role that
/// CONNECTED, which is the shared worker login and never the app, so the role
/// has to be per binding: a role per database would leave one app's revoked
/// edge with no database consequence while any parallel edge survived.
///
/// The epoch is the last component of the name, which is what makes an apply
/// able to retire access: a new epoch is a new role, and an isolate built
/// against a retired one fails at `SET LOCAL ROLE` rather than reading through
/// a stale grant. It is unsigned because it is a counter, so the name cannot
/// acquire a sign character that would need quoting.
///
/// # Errors
///
/// [`RoleNameTooLong`] if the composed name exceeds `PostgreSQL`'s identifier
/// limit. This is the refusal the epoch's position makes load-bearing:
/// `PostgreSQL` shortens from the tail, so a truncated name is the same name at
/// every epoch and the roles stop expiring.
pub fn binding_role_name(binding: &BindingId, epoch: u32) -> Result<String, RoleNameTooLong> {
    database_role::binding_role_name(binding.as_str(), epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database_role::POSTGRES_IDENTIFIER_MAX_BYTES;
    use crate::schema_name::SchemaName;

    /// The database fixture: the canonical rendering of the uuid
    /// `0191e7a2-b3c4-4d5e-8f90-123456789abc`.
    const DATABASE_FIXTURE: &str = "dbs_03cgepu94hyemwpcipafo7264";

    /// The binding fixture: the canonical rendering of the uuid
    /// `0192a3b4-c5d6-7e8f-9012-3456789abcde`. A DIFFERENT uuid from the
    /// database's, so a vector below cannot pass on an id that was substituted
    /// for the other.
    const BINDING_FIXTURE: &str = "bnd_03coc2qj4x2ae61h80zwlnnq6";

    fn database() -> DatabaseId {
        DatabaseId::parse(DATABASE_FIXTURE).expect("the fixture is a canonical database id")
    }

    fn binding() -> BindingId {
        BindingId::parse(BINDING_FIXTURE).expect("the fixture is a canonical binding id")
    }

    /// Every derived name, pinned to its bytes.
    ///
    /// TWO ORACLES PER LINE where a text composer exists: the frozen literal
    /// catches this seam and the composer it wraps drifting together, and the
    /// comparison against that composer catches the literal being updated to
    /// match a mistake. The schema name has no text composer - the spelling is
    /// composed here - so the literal is its only oracle and says so.
    #[test]
    fn golden_vectors() {
        let database = database();
        let binding = binding();

        // The schema the reconciler creates and the migrator role owns.
        assert_eq!(schema_name(&database), "db_dbs_03cgepu94hyemwpcipafo7264");

        assert_eq!(
            migrator_role_name(&database).expect("the fixture migrator role fits"),
            "zs_db_dbs_03cgepu94hyemwpcipafo7264_mig"
        );
        assert_eq!(
            migrator_role_name(&database).expect("composes"),
            database_role::database_migrator_role_name(DATABASE_FIXTURE).expect("untyped"),
            "the seam must compose the role the reconciler provisions"
        );

        assert_eq!(
            capability_role_name(&database, DatabaseCapability::ReadWrite).expect("composes"),
            "zs_db_dbs_03cgepu94hyemwpcipafo7264_rw"
        );
        assert_eq!(
            capability_role_name(&database, DatabaseCapability::ReadOnly).expect("composes"),
            "zs_db_dbs_03cgepu94hyemwpcipafo7264_ro"
        );
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            assert_eq!(
                capability_role_name(&database, capability).expect("composes"),
                database_role::database_capability_role_name(DATABASE_FIXTURE, capability)
                    .expect("untyped"),
                "{capability:?} must reach the untyped composer unchanged"
            );
        }

        assert_eq!(
            binding_role_name(&binding, 3).expect("composes"),
            "zs_bind_bnd_03coc2qj4x2ae61h80zwlnnq6_e3"
        );
        assert_eq!(
            binding_role_name(&binding, 3).expect("composes"),
            database_role::binding_role_name(BINDING_FIXTURE, 3).expect("untyped"),
            "the seam must compose the role the grant statements name"
        );
    }

    /// Every derivation is a function OF THE ID, not of a constant.
    ///
    /// The vectors above drive one database and one binding, so a composer
    /// that ignored its argument and returned its own frozen literal would
    /// pass all of them. Two distinct ids must disagree everywhere: a
    /// collision here would put two projects' tables in one schema, or two
    /// apps' statements under one grant.
    #[test]
    fn every_derivation_varies_with_the_id() {
        let first = DatabaseId::mint();
        let second = DatabaseId::mint();
        assert_ne!(first, second, "the control: two mints are two databases");

        assert_ne!(schema_name(&first), schema_name(&second));
        assert_ne!(
            migrator_role_name(&first).expect("composes"),
            migrator_role_name(&second).expect("composes")
        );
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            assert_ne!(
                capability_role_name(&first, capability).expect("composes"),
                capability_role_name(&second, capability).expect("composes"),
                "{capability:?} must name one database"
            );
        }

        let one_binding = BindingId::mint();
        let other_binding = BindingId::mint();
        assert_ne!(one_binding, other_binding, "the control: two mints");
        assert_ne!(
            binding_role_name(&one_binding, 1).expect("composes"),
            binding_role_name(&other_binding, 1).expect("composes")
        );
    }

    /// One database's three roles are three names.
    ///
    /// The migrator owns the schema while the capability roles hold the
    /// column-listed grants, so two of them collapsing onto one name would
    /// hand an app the owner's authority or a readonly binding the write
    /// grants.
    #[test]
    fn a_database_names_three_distinct_roles() {
        let database = database();
        let mut names = vec![
            migrator_role_name(&database).expect("composes"),
            capability_role_name(&database, DatabaseCapability::ReadWrite).expect("composes"),
            capability_role_name(&database, DatabaseCapability::ReadOnly).expect("composes"),
        ];
        let composed = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), composed, "distinct names: {names:?}");
    }

    /// The epoch is what an apply rotates, so it must move the name.
    ///
    /// This is the property truncation destroys, stated on the typed seam:
    /// consecutive epochs of ONE binding are different roles, and the previous
    /// epoch's role is the one an apply drops.
    #[test]
    fn the_binding_role_varies_with_the_epoch() {
        let binding = binding();
        let mut seen = Vec::new();
        for epoch in [0u32, 1, 2, 3, u32::MAX] {
            seen.push(binding_role_name(&binding, epoch).expect("composes"));
        }
        let minted = seen.len();
        assert_eq!(minted, 5, "the arm must compose every epoch it lists");
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), minted, "one role per epoch: {seen:?}");
    }

    /// The derived schema name validates as a schema name.
    ///
    /// `SchemaName` is what every caller passes the derived spelling through,
    /// so a derivation producing a name it rejects is unusable rather than
    /// merely ugly. Minted ids as well as the fixture, because the fixture
    /// alone would not exercise the whole base36 alphabet.
    #[test]
    fn the_derived_schema_name_validates() {
        let mut checked = 0;
        for database in std::iter::once(database()).chain((0..64).map(|_| DatabaseId::mint())) {
            let derived = schema_name(&database);
            let validated = SchemaName::new(&derived)
                .unwrap_or_else(|error| panic!("`{derived}` must validate: {error}"));
            assert_eq!(validated.as_str(), derived);
            checked += 1;
        }
        assert_eq!(checked, 65, "the arm must not pass over an empty iterator");
    }

    /// Every name a real id and a real epoch can produce fits `PostgreSQL`.
    ///
    /// The refusal in [`crate::database_role`] is the fence; this is the early
    /// warning. A prefix change or a longer id body that pushed a composed
    /// name past the limit turns every one of these into a refusal at runtime,
    /// and this arm fails at the boundary the epoch counter can actually
    /// reach - `u32::MAX` digits - rather than waiting for a cluster to reach
    /// it.
    #[test]
    fn every_composable_name_fits_the_identifier_limit() {
        let database = DatabaseId::mint();
        let binding = BindingId::mint();

        let mut widest = schema_name(&database);
        for name in [
            migrator_role_name(&database).expect("composes"),
            capability_role_name(&database, DatabaseCapability::ReadWrite).expect("composes"),
            capability_role_name(&database, DatabaseCapability::ReadOnly).expect("composes"),
            binding_role_name(&binding, u32::MAX).expect("the widest epoch composes"),
        ] {
            if name.len() > widest.len() {
                widest = name;
            }
        }
        assert!(
            widest.len() <= POSTGRES_IDENTIFIER_MAX_BYTES,
            "`{widest}` is {} bytes and PostgreSQL would shorten it",
            widest.len()
        );
    }
}
