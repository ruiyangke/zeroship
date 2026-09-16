//! Keeping a creator database's workflow journal at the current version.
//!
//! # Why the manager owns this and not Control
//!
//! The manager already learns when an app's workflows must be able to run, and
//! it is the platform side of the workflow domain. Routing journal provisioning
//! through Control would move the journal's ARTIFACTS into Control, which is the
//! leak the schema-bundle path exists to remove. Control asks; the manager holds
//! the artifacts and sends them.
//!
//! # Two triggers, one capability
//!
//! Registration, when Control learns an app deployed, and REFUSAL, when a
//! worker's host finds a journal it will not use. The second is what turns a
//! refusal into a repair: before it, a host that refused a journal had no way
//! forward and no process could bring the journal to the version it wanted.
//!
//! Both are the same idempotent call, so neither has to know what the other did.

#![allow(
    clippy::future_not_send,
    reason = "the bundle exchange stays on its owning compio runtime"
)]

use std::sync::Arc;

use zeroship_core::{
    schema_bundle::{SchemaBundle, SchemaBundleOutcome, SchemaBundleVersion, SchemaStamp},
    schema_name::SchemaName,
    service_peers::ServiceAuth,
};
use zeroship_workflow_client::{Options as ClientOptions, SchemaBundles};

use crate::coordinator::Error;

/// The bundle label the migration service echoes back in its logs. Opaque to
/// that service; this is the one place it is chosen.
const BUNDLE: &str = "workflow_journal";

/// Sends the journal bundle to the migration service.
#[derive(Clone, Debug)]
pub struct Journal {
    bundles: SchemaBundles,
    policy: String,
}

impl Journal {
    /// Bind the migration service origin and the manager's own signer.
    ///
    /// # Errors
    /// Refuses an invalid origin or a missing service identity.
    pub fn new(
        migrate_url: &str,
        auth: Arc<ServiceAuth>,
        options: ClientOptions,
    ) -> Result<Self, Error> {
        Ok(Self {
            bundles: SchemaBundles::new(migrate_url, auth, options)
                .map_err(|_| Error::Unavailable)?,
            policy: charter(),
        })
    }

    /// Ensure one schema's journal is at the version this build carries.
    ///
    /// # Errors
    /// Refuses an unusable schema name and reports every refusal the migration
    /// service answers with, including a journal NEWER than this build - which is
    /// a real refusal, not a failure to try: an older manager must not downgrade
    /// a creator's journal.
    pub async fn ensure(&self, schema: &str) -> Result<SchemaBundleOutcome, Error> {
        let schema = SchemaName::new(schema).map_err(|_| Error::Invalid)?;
        let bundle = self.bundle_for(schema.as_str())?;
        self.bundles.apply(&bundle).await.map_err(|error| {
            tracing::warn!(
                schema = schema.as_str(),
                ?error,
                "workflow journal provisioning refused"
            );
            Error::Unavailable
        })
    }

    fn bundle_for(&self, schema: &str) -> Result<SchemaBundle, Error> {
        journal_bundle(schema, &self.policy)
    }
}

/// The exact bundle this build sends for one schema.
///
/// Public without an HTTP client or a service identity because it IS the whole
/// contract with the migration service: a caller that wants to know what the
/// manager would install can read it here rather than infer it from a reply.
///
/// # Errors
/// Refuses a build whose artifacts do not carry the `PostgreSQL` series.
pub fn bundle_for(schema: &str) -> Result<SchemaBundle, Error> {
    journal_bundle(schema, &charter())
}

/// Build the bundle for one schema from the artifacts this build carries.
///
/// A free function so it can be exercised without an HTTP client or a service
/// identity: what it produces is the whole contract with the migration service,
/// and that is worth testing on its own.
fn journal_bundle(schema: &str, policy: &str) -> Result<SchemaBundle, Error> {
    let dialect = zeroship_workflow_schema::POSTGRES;
    let series = zeroship_workflow_schema::versions(dialect).ok_or(Error::Unavailable)?;
    let fingerprint = zeroship_workflow_schema::fingerprint(dialect).ok_or(Error::Unavailable)?;
    Ok(SchemaBundle {
        bundle: BUNDLE.to_owned(),
        schema: schema.to_owned(),
        dialect: dialect.to_owned(),
        version: zeroship_workflow_schema::VERSION,
        fingerprint: fingerprint.to_owned(),
        stamp: SchemaStamp {
            table: zeroship_workflow_schema::STAMP_TABLE.to_owned(),
            row_id: zeroship_workflow_schema::STAMP_ROW_ID.to_owned(),
        },
        policy: policy.replace(SCHEMA_PLACEHOLDER, schema),
        versions: series
            .iter()
            .map(|step| SchemaBundleVersion {
                version: step.version,
                sql: step.bound_to(schema),
            })
            .collect(),
    })
}

/// The token the charter template carries where the target schema goes.
const SCHEMA_PLACEHOLDER: &str = "__ZEROSHIP_BUNDLE_SCHEMA__";

/// The policy the journal bundle DECLARES, narrowed to the schema it targets.
///
/// It asks for exactly what the generated DDL needs and nothing else: create the
/// journal's tables, name its own schema explicitly, and carry the raw statements
/// the portable op vocabulary cannot yet express (a column collation). It does
/// NOT ask to be destructive - the journal's current series never drops anything,
/// so declaring the tighter posture makes a bundle that suddenly did into a
/// refusal rather than a surprise.
fn charter() -> String {
    format!(
        r#"policy_version = 1
[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["{SCHEMA_PLACEHOLDER}"] }}
[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = ["{SCHEMA_PLACEHOLDER}"] }}
[[grant]]
key = "sql.raw"
value = true
scope = {{ include = ["{SCHEMA_PLACEHOLDER}"] }}
[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle the manager sends must describe the artifacts this build
    /// carries. A mismatch between the declared version and the series would
    /// stamp a version whose DDL never ran.
    #[test]
    fn the_bundle_carries_the_whole_series_this_build_holds() {
        let bundle = journal_bundle("customer", &charter()).expect("build the bundle");
        assert_eq!(bundle.version, zeroship_workflow_schema::VERSION);
        assert_eq!(
            bundle
                .versions
                .iter()
                .map(|step| step.version)
                .collect::<Vec<_>>(),
            (1..=zeroship_workflow_schema::VERSION).collect::<Vec<_>>()
        );
        assert_eq!(
            bundle.fingerprint,
            zeroship_workflow_schema::fingerprint(zeroship_workflow_schema::POSTGRES).unwrap()
        );
        assert_eq!(bundle.stamp.table, zeroship_workflow_schema::STAMP_TABLE);
    }

    /// Every step, and the declared policy, must name the target schema. An
    /// unbound placeholder would reach the service as a charter granting nothing
    /// on the schema the DDL touches, and every statement would be refused.
    #[test]
    fn the_bundle_is_bound_to_the_schema_it_names() {
        let bundle = journal_bundle("customer", &charter()).expect("build the bundle");
        assert!(bundle.policy.contains("\"customer\""));
        assert!(!bundle.policy.contains(SCHEMA_PLACEHOLDER));
        for step in &bundle.versions {
            assert!(
                step.sql.contains("\"customer\""),
                "v{} is not bound to the target schema",
                step.version
            );
            assert!(
                !step
                    .sql
                    .contains(zeroship_workflow_schema::SCHEMA_PLACEHOLDER),
                "v{} still carries the unbound placeholder",
                step.version
            );
        }
    }
}
