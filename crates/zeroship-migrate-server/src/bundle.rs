//! Applying a PLATFORM schema bundle to one schema of a creator database.
//!
//! # What this module knows, and what it refuses to know
//!
//! It knows about bundles, stamps and versions. It does not know what any
//! particular bundle is FOR. A second platform-owned per-schema artifact reuses
//! this unchanged, which is the test of whether the boundary is real: if adding
//! one required a line here, the boundary would be decoration.
//!
//! # Why the bundle carries SQL rather than recorded operations
//!
//! Because the engine cannot represent these names. A platform-owned schema
//! lives in the reserved `__zeroship` namespace exactly so a creator cannot
//! declare it, and `zeroship_migrate_core::schema::query::validate_collection`
//! refuses that prefix unconditionally, at validate, before lower - the same
//! function ORM CRUD dispatch uses. An operation-carrying bundle is therefore
//! not available for the artifacts this path exists to install.
//!
//! The bundle is still policed, at the layer that can see it. Every statement
//! goes through `MigrationGuard::check` under a [`GuardConfig`] bound to the
//! target schema: the same line-1 belt the creator apply path runs over its
//! rendered DDL, refusing a deny-listed construct and refusing any reference
//! outside the bound schema. The composed policy is the ceiling the bundle's
//! declared charter is admitted against, with escalation-reject.
//!
//! # The destructive posture is enforced by the guard, and that was measured
//!
//! This module briefly carried its own destructive gate, on the belief that
//! `MigrationGuard::check` only FLAGS a destructive statement and leaves the
//! denial to the engine's plan gate - which an applier executing vetted SQL
//! never reaches. Half of that is right: the guard flags under `allow` and
//! `warn`. Under `forbid` it DENIES, as a data-security policy decision layered
//! on the parse (`data_security_rule::DESTRUCTIVE_OPS_FORBID`), so a bundle
//! whose declared policy forbids destructive operations is refused here with no
//! second comparison. `a_destructive_step_is_refused_when_the_declared_policy_forbids_it`
//! is what holds that; the gate it replaced would have fired only under `warn`,
//! where proceeding is what `warn` means.

use compio_postgres::Client;
use zeroship_core::schema_bundle::{SchemaBundle, SchemaBundleAction, SchemaBundleOutcome};
use zeroship_core::schema_name::SchemaName;
use zeroship_migrate_backend::guard::{GuardConfig, MigrationGuard};
use zeroship_migrate_postgres::{PgGuard, DIALECT as POSTGRES};

use crate::policy::{bundle_policy_for_schema, ManagedPolicyError};
use crate::provisioning::{provision_database, ProvisionDatabaseError};
use crate::session::CompioPgSession;

/// The one dialect this host applies. The engine is multi-dialect; this service
/// is not, and a bundle for anything else belongs to a different applier.
const DIALECT: &str = "postgres";

/// A refused or failed bundle.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The request does not describe a bundle this service can reason about.
    #[error("invalid schema bundle: {0}")]
    Invalid(String),
    /// The bundle names a dialect this host does not apply.
    #[error("schema bundle dialect {0:?} is not applied by this service")]
    UnsupportedDialect(String),
    /// The declared policy did not parse, or escalated beyond the ceiling.
    #[error(transparent)]
    Policy(#[from] ManagedPolicyError),
    /// The rendered-DDL guard refused a statement.
    #[error("schema bundle version {version} refused by the guard: {detail}")]
    Guarded { version: u32, detail: String },
    /// The installed journal is at this version but carries a different
    /// fingerprint: it is corrupted, not merely out of date.
    #[error(
        "schema at version {version} carries fingerprint {installed}, not the bundle's \
         {expected}; this is a corrupted schema, not an out-of-date one"
    )]
    Corrupt {
        version: u32,
        installed: String,
        expected: String,
    },
    /// The installed schema is NEWER than the bundle. Never downgrade.
    #[error(
        "schema is at version {installed}, ahead of the bundle's {offered}; a newer platform \
         provisioned it and it must not be downgraded"
    )]
    Behind { installed: u32, offered: u32 },
    /// Provisioning the schema or its migrator role failed.
    #[error(transparent)]
    Provision(#[from] ProvisionDatabaseError),
    /// The database refused a statement, or was unreachable.
    #[error("schema bundle database error: {0}")]
    Database(#[from] compio_postgres::Error),
}

impl BundleError {
    /// The status and stable error code this refusal answers with.
    #[must_use]
    pub fn kind(&self) -> (ntex::http::StatusCode, &'static str) {
        use ntex::http::StatusCode;
        match self {
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_schema_bundle"),
            Self::UnsupportedDialect(_) => (StatusCode::BAD_REQUEST, "unsupported_dialect"),
            Self::Policy(policy) if policy.is_creator_fault() => {
                (StatusCode::UNPROCESSABLE_ENTITY, "bundle_policy_invalid")
            }
            Self::Guarded { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "bundle_refused"),
            Self::Corrupt { .. } => (StatusCode::CONFLICT, "schema_corrupt"),
            Self::Behind { .. } => (StatusCode::CONFLICT, "bundle_behind"),
            Self::Policy(_) | Self::Provision(_) | Self::Database(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_bundle_infrastructure",
            ),
        }
    }
}

/// What the stamp says today.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Installed {
    version: u32,
    fingerprint: String,
}

/// Install or upgrade one schema from a bundle.
///
/// Idempotent: a bundle whose version and fingerprint already match leaves the
/// schema and its ROWS untouched.
///
/// # Errors
/// Every variant of [`BundleError`]. The refusals that matter are
/// [`BundleError::Behind`] (never downgrade a schema) and
/// [`BundleError::Corrupt`] (a fingerprint mismatch at the same version is
/// damage, not staleness, and is not silently repaired).
pub async fn apply_schema_bundle(
    provision_dsn: &str,
    bundle: &SchemaBundle,
) -> Result<SchemaBundleOutcome, BundleError> {
    let schema = validate(bundle)?;
    let policy = bundle_policy_for_schema(schema.as_str(), &bundle.policy)?;
    let guard = PgGuard::from_config(GuardConfig::from_policy(policy, POSTGRES, schema.as_str()));

    let mut session = CompioPgSession::connect(provision_dsn).await?;
    // The schema and its least-privilege migrator role are this service's OWN
    // capability, not the bundle's domain, and both steps are idempotent. A
    // bundle therefore never has to be sequenced behind a separate create call.
    provision_database(session.client(), schema.as_str()).await?;
    apply_within_transaction(session.client_mut(), bundle, &schema, &guard).await
}

/// The whole decision and every write, inside ONE transaction.
///
/// The transaction is the mechanism behind the property that matters: an upgrade
/// that fails part way leaves the stamp at the version that is actually
/// installed, because the stamp write and the steps it describes commit
/// together or not at all.
async fn apply_within_transaction(
    client: &mut Client,
    bundle: &SchemaBundle,
    schema: &SchemaName,
    guard: &PgGuard,
) -> Result<SchemaBundleOutcome, BundleError> {
    let transaction = client.transaction().await?;
    let installed = read_stamp(&transaction, bundle, schema).await?;

    let (action, apply_above) = match installed {
        None => (SchemaBundleAction::Installed, 0),
        Some(stamp) if stamp.version == bundle.version => {
            if stamp.fingerprint != bundle.fingerprint {
                return Err(BundleError::Corrupt {
                    version: stamp.version,
                    installed: stamp.fingerprint,
                    expected: bundle.fingerprint.clone(),
                });
            }
            transaction.commit().await?;
            return Ok(SchemaBundleOutcome {
                schema: schema.as_str().to_owned(),
                bundle: bundle.bundle.clone(),
                version: bundle.version,
                action: SchemaBundleAction::Unchanged,
            });
        }
        Some(stamp) if stamp.version < bundle.version => {
            (SchemaBundleAction::Upgraded, stamp.version)
        }
        Some(stamp) => {
            return Err(BundleError::Behind {
                installed: stamp.version,
                offered: bundle.version,
            })
        }
    };

    // VET EVERY STEP BEFORE EXECUTING ANY OF THEM. A bundle whose last step is
    // refused must leave the schema as it was, and the transaction alone would
    // not say so loudly: the refusal belongs before the first write.
    let steps: Vec<_> = bundle
        .versions
        .iter()
        .filter(|step| step.version > apply_above)
        .collect();
    for step in &steps {
        guard
            .check(&step.sql)
            .map_err(|error| BundleError::Guarded {
                version: step.version,
                detail: format!("{error:?}"),
            })?;
    }
    for step in &steps {
        transaction.batch_execute(&step.sql).await?;
    }
    write_stamp(&transaction, bundle, schema).await?;
    transaction.commit().await?;
    Ok(SchemaBundleOutcome {
        schema: schema.as_str().to_owned(),
        bundle: bundle.bundle.clone(),
        version: bundle.version,
        action,
    })
}

/// Read the bundle's stamp row, or `None` when the schema carries no stamp table
/// at all. The table is created by the bundle's own first version, so its
/// absence is what "not installed" means.
async fn read_stamp(
    transaction: &compio_postgres::Transaction<'_>,
    bundle: &SchemaBundle,
    schema: &SchemaName,
) -> Result<Option<Installed>, BundleError> {
    let present: bool = transaction
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = $2)",
            &[&schema.as_str(), &bundle.stamp.table.as_str()],
        )
        .await?
        .get(0);
    if !present {
        return Ok(None);
    }
    let rows = transaction
        .query(
            &format!(
                "SELECT version, fingerprint FROM {}.{} WHERE id = $1",
                quote_ident(schema.as_str()),
                quote_ident(&bundle.stamp.table)
            ),
            &[&bundle.stamp.row_id.as_str()],
        )
        .await?;
    let Some(row) = rows.first() else {
        // The table exists but carries no row for this bundle. Treat it as not
        // installed rather than as damage: another bundle may own the table, and
        // the install path's own DDL is written to tolerate what is already
        // there.
        return Ok(None);
    };
    let version: i64 = row.try_get("version")?;
    let version = u32::try_from(version).map_err(|_| {
        BundleError::Invalid(format!("installed version {version} is out of range"))
    })?;
    Ok(Some(Installed {
        version,
        fingerprint: row.try_get("fingerprint")?,
    }))
}

/// Record the version and fingerprint now installed, in the same transaction as
/// the steps that installed them.
async fn write_stamp(
    transaction: &compio_postgres::Transaction<'_>,
    bundle: &SchemaBundle,
    schema: &SchemaName,
) -> Result<(), BundleError> {
    transaction
        .execute(
            &format!(
                "INSERT INTO {}.{} (id, version, fingerprint) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version, \
                 fingerprint = EXCLUDED.fingerprint",
                quote_ident(schema.as_str()),
                quote_ident(&bundle.stamp.table)
            ),
            &[
                &bundle.stamp.row_id.as_str(),
                &i64::from(bundle.version),
                &bundle.fingerprint.as_str(),
            ],
        )
        .await?;
    Ok(())
}

/// Refuse a bundle that does not describe an ordered series this service can
/// reason about, before any connection is opened.
fn validate(bundle: &SchemaBundle) -> Result<SchemaName, BundleError> {
    if bundle.dialect != DIALECT {
        return Err(BundleError::UnsupportedDialect(bundle.dialect.clone()));
    }
    if bundle.bundle.trim().is_empty() {
        return Err(BundleError::Invalid("bundle name is empty".into()));
    }
    let schema = SchemaName::new(&bundle.schema)
        .map_err(|reason| BundleError::Invalid(format!("schema {:?}: {reason}", bundle.schema)))?;
    if bundle.version == 0 {
        return Err(BundleError::Invalid("a series starts at version 1".into()));
    }
    if !is_sha256_hex(&bundle.fingerprint) {
        return Err(BundleError::Invalid(format!(
            "fingerprint {:?} is not lowercase sha256 hex",
            bundle.fingerprint
        )));
    }
    if !is_bare_identifier(&bundle.stamp.table) {
        return Err(BundleError::Invalid(format!(
            "stamp table {:?} is not a bare identifier",
            bundle.stamp.table
        )));
    }
    if bundle.stamp.row_id.is_empty() {
        return Err(BundleError::Invalid("stamp row id is empty".into()));
    }
    if bundle.policy.trim().is_empty() {
        return Err(BundleError::Invalid(
            "a bundle must declare the policy it runs under".into(),
        ));
    }
    let offered: Vec<u32> = bundle.versions.iter().map(|step| step.version).collect();
    if offered != (1..=bundle.version).collect::<Vec<_>>() {
        return Err(BundleError::Invalid(format!(
            "the series must be 1..={} in order; got {offered:?}",
            bundle.version
        )));
    }
    if let Some(empty) = bundle
        .versions
        .iter()
        .find(|step| step.sql.trim().is_empty())
    {
        return Err(BundleError::Invalid(format!(
            "version {} carries no statements",
            empty.version
        )));
    }
    Ok(schema)
}

/// A stamp table name is interpolated into SQL as an identifier, so it must be
/// one - never a quoted string carrying anything else.
fn is_bare_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::schema_bundle::{SchemaBundleVersion, SchemaStamp};

    fn bundle() -> SchemaBundle {
        SchemaBundle {
            bundle: "probe".into(),
            schema: "customer".into(),
            dialect: DIALECT.into(),
            version: 1,
            fingerprint: "a".repeat(64),
            stamp: SchemaStamp {
                table: "__zeroship_probe_version".into(),
                row_id: "probe".into(),
            },
            policy: "policy_version = 1\n".into(),
            versions: vec![SchemaBundleVersion {
                version: 1,
                sql: "CREATE TABLE \"customer\".\"t\" (id text PRIMARY KEY)".into(),
            }],
        }
    }

    #[test]
    fn a_well_formed_bundle_validates() {
        assert_eq!(validate(&bundle()).unwrap().as_str(), "customer");
    }

    /// Each refusal is checked against a control that differs in ONE field, so a
    /// validator that refused everything would not pass this.
    #[test]
    fn every_malformed_bundle_is_refused_for_its_own_reason() {
        let cases: Vec<(&str, Box<dyn Fn(&mut SchemaBundle)>)> = vec![
            (
                "dialect",
                Box::new(|b: &mut SchemaBundle| b.dialect = "sqlite".into()),
            ),
            (
                "bundle name",
                Box::new(|b: &mut SchemaBundle| b.bundle = "  ".into()),
            ),
            (
                "schema",
                Box::new(|b: &mut SchemaBundle| b.schema = "no\"good".into()),
            ),
            ("version", Box::new(|b: &mut SchemaBundle| b.version = 0)),
            (
                "fingerprint",
                Box::new(|b: &mut SchemaBundle| b.fingerprint = "A".repeat(64)),
            ),
            (
                "stamp table",
                Box::new(|b: &mut SchemaBundle| b.stamp.table = "a\"; DROP".into()),
            ),
            (
                "stamp row",
                Box::new(|b: &mut SchemaBundle| b.stamp.row_id = String::new()),
            ),
            (
                "policy",
                Box::new(|b: &mut SchemaBundle| b.policy = " ".into()),
            ),
            (
                "gap",
                Box::new(|b: &mut SchemaBundle| {
                    b.version = 2;
                }),
            ),
            (
                "empty sql",
                Box::new(|b: &mut SchemaBundle| b.versions[0].sql = "\n".into()),
            ),
        ];
        for (name, mutate) in cases {
            let mut broken = bundle();
            mutate(&mut broken);
            assert!(
                validate(&broken).is_err(),
                "a bundle with a bad {name} was accepted"
            );
        }
        assert!(
            validate(&bundle()).is_ok(),
            "the control must still validate"
        );
    }

    /// The stamp table reaches SQL as an identifier. A name that could close the
    /// quote would be a statement boundary in a platform-authenticated call.
    #[test]
    fn a_stamp_table_cannot_carry_a_statement_boundary() {
        assert!(is_bare_identifier("__zeroship_probe_schema_version"));
        for hostile in [
            "a\"; DROP SCHEMA public; --",
            "",
            "1leading",
            "has space",
            "quote\"",
        ] {
            assert!(
                !is_bare_identifier(hostile),
                "{hostile:?} passed as an identifier"
            );
        }
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    /// The series is 1..=version, in order. An out-of-order or short series would
    /// silently skip a step.
    #[test]
    fn a_series_with_a_hole_or_out_of_order_is_refused() {
        let mut two = bundle();
        two.version = 2;
        two.versions.push(SchemaBundleVersion {
            version: 2,
            sql: "ALTER TABLE \"customer\".\"t\" ADD COLUMN b text".into(),
        });
        assert!(validate(&two).is_ok(), "a contiguous series must validate");

        let mut reversed = two.clone();
        reversed.versions.reverse();
        assert!(
            validate(&reversed).is_err(),
            "an out-of-order series was accepted"
        );

        let mut holed = two;
        holed.versions[1].version = 3;
        holed.version = 3;
        assert!(
            validate(&holed).is_err(),
            "a series with a hole was accepted"
        );
    }
}
