//! The platform's record of what an app's schema corresponds to:
//! `zeroship.app_schema_applies`, one row per apply request.
//!
//! # Why the platform keeps a record at all
//!
//! The engine writes its own journal, and that journal is now the ONLY record of
//! what ran - except that it lives in the app's own schema, which the app's
//! migrator role owns. A schema owner can `DROP` and `TRUNCATE` anything in it and
//! owner privileges cannot be `REVOKE`d away, so a creator can destroy their own
//! journal. That is accepted (it is their database and corrupting it breaks only
//! them) and it is exactly why the platform must not treat the journal as a trust
//! anchor. The deploy precondition reads THIS table, on the control plane's own
//! connection, in the control plane's own schema.
//!
//! # One row per REQUEST, not per applied migration
//!
//! [`SchemaApplyStore::record_submitted`] runs before the engine does, and
//! [`SchemaApplyStore::mark_applied`] runs on every successful apply - including
//! one that applied nothing because every document was already journalled. That is
//! a requirement, not an artifact of where the insert sits: an engine upgrade that
//! changes descriptor bytes without changing any schema would otherwise halt every
//! app's next deploy forever, because control compares against the NEWEST applied
//! row. "Skip the write when nothing applied" is a natural optimisation and it
//! would brick every such app.
//!
//! `applied_versions` carries the engine's own `outcome.applied` for the request,
//! so a re-run that advanced nothing is visible as `[]` rather than being
//! indistinguishable from one that did.
//!
//! # There is no approval state machine here
//!
//! The `planned`/`pending_approval`/`approved` lifecycle and its audit log were
//! removed on 2026-08-28. A creator approves their own destructive ops; the
//! operator-approval capability is gone deliberately, not by oversight.
//!
//! # `app_id` is bound as `text`, and that is a requirement on the column
//!
//! Every statement here binds [`AppId::as_str`] and casts the placeholder to
//! `text`. [`AppId`] exposes no route to any embedded bits, so text against text
//! is the only comparison these statements can make, and a database whose
//! `zeroship.app_schema_applies.app_id` is still `uuid` fails them outright with
//! a type error. That is the loud failure, and it is the one to want: the two
//! terminal transitions are `UPDATE ... WHERE app_id = $1`, and a comparison
//! that merely matched no row would be reported as
//! [`TerminalTransition::Lost`] - a lost race, indistinguishable from the
//! benign case this store is built to tolerate.

use std::convert::TryFrom;

use compio_postgres::{Client, NoTls};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::app_id::AppId;

use crate::policy::ManagedPosture;

/// Reader/writer for `zeroship.app_schema_applies`.
#[derive(Debug, Clone)]
pub struct SchemaApplyStore {
    dsn: String,
}

/// The outcome of a TERMINAL status transition (`applied` / `failed`).
///
/// Both guards are negative on purpose - [`SchemaApplyStore::mark_applied`] refuses
/// a `failed` row and [`SchemaApplyStore::mark_failed`] refuses an `applied` one -
/// so whichever transition lands first wins and losing is not an error. It is not
/// success either: the row does not hold the status the caller just concluded, so a
/// caller that reported its own conclusion onward would be contradicting the
/// record. Both callers used to discard the row count and return `Ok(())`, which
/// made a lost race indistinguishable from a won one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum TerminalTransition {
    /// The update matched the row, which now holds the requested terminal status.
    Recorded,
    /// The update matched nothing: a concurrent transition had already moved the row
    /// to the OTHER terminal status, which it retains.
    Lost,
}

impl SchemaApplyStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    /// Open one connection and confirm the server answers. Backs `/readyz`: every
    /// API call this service serves opens a connection on this DSN, so a DSN it
    /// cannot connect on means it cannot serve.
    ///
    /// Deliberately no query - a connect plus a protocol-level sync proves
    /// reachability, credentials and an accepting server without depending on any
    /// table existing.
    ///
    /// # Errors
    /// [`SchemaApplyStoreError::Connect`] when the DSN will not connect;
    /// [`SchemaApplyStoreError::Query`] when the server does not answer the sync.
    pub async fn probe(&self) -> Result<(), SchemaApplyStoreError> {
        let client = self.connect().await?;
        client
            .check_connection()
            .await
            .map_err(SchemaApplyStoreError::Query)
    }

    /// Open the row for one apply request, before the engine runs.
    ///
    /// # Errors
    /// [`SchemaApplyStoreError::CeilingVersionOverflow`] when the ceiling version
    /// does not fit a `BIGINT`; [`SchemaApplyStoreError::Connect`] /
    /// [`SchemaApplyStoreError::Query`] on any database failure.
    pub async fn record_submitted(
        &self,
        input: SchemaApplyInput<'_>,
    ) -> Result<(), SchemaApplyStoreError> {
        let ceiling_version = i64::try_from(input.ceiling_version).map_err(|_| {
            SchemaApplyStoreError::CeilingVersionOverflow(input.ceiling_version)
        })?;
        let effective_profile = input.effective_profile.to_audit_json();
        let client = self.connect().await?;
        client
            .execute(
                "INSERT INTO zeroship.app_schema_applies \
                    (app_id, migration_id, status, request_body, effective_profile, \
                     ceiling_id, ceiling_version, descriptor_sha256, submitted_by) \
                 VALUES ($1::text, $2, 'submitted', $3::jsonb, $4::jsonb, $5, $6, $7, $8)",
                &[
                    &input.app_id.as_str(),
                    &input.migration_id,
                    &input.request_body,
                    &effective_profile,
                    &input.ceiling_id,
                    &ceiling_version,
                    &input.descriptor_sha256,
                    &input.principal_id,
                ],
            )
            .await
            .map_err(SchemaApplyStoreError::Query)?;
        Ok(())
    }

    /// APPLY transition: the engine returned success, so the row becomes `applied`
    /// and carries the versions the engine reported as applied.
    ///
    /// `applied_versions` may legitimately be empty - see the module note on why the
    /// row is still written.
    ///
    /// # Errors
    /// [`SchemaApplyStoreError::EncodeVersions`] when the version list will not
    /// serialize; [`SchemaApplyStoreError::Connect`] / [`SchemaApplyStoreError::Query`]
    /// on any database failure.
    pub async fn mark_applied(
        &self,
        app_id: &AppId,
        migration_id: Uuid,
        applied_versions: &[String],
    ) -> Result<TerminalTransition, SchemaApplyStoreError> {
        let applied = serde_json::to_value(applied_versions)
            .map_err(|err| SchemaApplyStoreError::EncodeVersions(err.to_string()))?;
        let client = self.connect().await?;
        let updated = client
            .execute(
                "UPDATE zeroship.app_schema_applies \
                    SET status = 'applied', applied_at = NOW(), applied_versions = $3::jsonb, \
                        last_error = NULL \
                  WHERE app_id = $1::text AND migration_id = $2 AND status <> 'failed'",
                &[&app_id.as_str(), &migration_id, &applied],
            )
            .await
            .map_err(SchemaApplyStoreError::Query)?;
        Ok(if updated == 0 {
            TerminalTransition::Lost
        } else {
            TerminalTransition::Recorded
        })
    }

    /// FAILURE transition: preflight or apply failed, so the row becomes `failed`
    /// with the message.
    ///
    /// # Errors
    /// [`SchemaApplyStoreError::Connect`] / [`SchemaApplyStoreError::Query`] on any
    /// database failure.
    pub async fn mark_failed(
        &self,
        app_id: &AppId,
        migration_id: Uuid,
        message: &str,
    ) -> Result<TerminalTransition, SchemaApplyStoreError> {
        let client = self.connect().await?;
        let updated = client
            .execute(
                "UPDATE zeroship.app_schema_applies \
                    SET status = 'failed', last_error = $3 \
                  WHERE app_id = $1::text AND migration_id = $2 AND status <> 'applied'",
                &[&app_id.as_str(), &migration_id, &message],
            )
            .await
            .map_err(SchemaApplyStoreError::Query)?;
        Ok(if updated == 0 {
            TerminalTransition::Lost
        } else {
            TerminalTransition::Recorded
        })
    }

    /// Open a connection for one store operation.
    ///
    /// Every method above is a single autocommit statement and the concurrency
    /// safety is in the SQL - each terminal transition constrains the prior status in
    /// its `WHERE` clause and matches zero rows when it loses, so two racing callers
    /// cannot both win regardless of which session they ran on.
    ///
    /// So a shared pipelined `Arc<Client>` would be correct here, and is what the
    /// control plane holds for exactly this kind of traffic. The trade taken
    /// instead: per-operation connect costs a handshake per statement, and buys that
    /// a dropped or poisoned session costs ONE request rather than every request
    /// until the process restarts. Reconsider it if the handshake ever shows up in a
    /// measurement - it has not been measured.
    async fn connect(&self) -> Result<Client, SchemaApplyStoreError> {
        let (client, conn) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(SchemaApplyStoreError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = conn.run().await {
                tracing::debug!(error = %err, "migrate-server: schema apply store connection ended");
            }
        })
        .detach();
        Ok(client)
    }
}

/// The fields `record_submitted` writes.
#[derive(Debug, Clone)]
pub struct SchemaApplyInput<'a> {
    pub app_id: &'a AppId,
    pub migration_id: Uuid,
    pub principal_id: Uuid,
    pub request_body: Value,
    pub effective_profile: &'a ManagedPosture,
    pub ceiling_id: &'a str,
    pub ceiling_version: u64,
    /// The runtime descriptor this document set folds to, lowercase sha256 hex.
    ///
    /// Validated at the request door (`is_sha256_hex`), so the spelling recorded
    /// here is the one a `.zship` manifest hash can be compared to with a plain SQL
    /// `=`. An uppercase or whitespace-padded hash of the SAME bytes would be
    /// recorded, would look right in the row, and would refuse every deploy of the
    /// app that submitted it.
    pub descriptor_sha256: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaApplyStoreError {
    #[error("schema apply store connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("schema apply store query: {0}")]
    Query(compio_postgres::Error),
    #[error("encode applied versions: {0}")]
    EncodeVersions(String),
    #[error("ceiling version {0} cannot be stored as BIGINT")]
    CeilingVersionOverflow(u64),
}

// Every case in this module opens a real PostgreSQL connection and `expect`s it,
// so with no server reachable the LIB test target panics and `cargo test
// --workspace` - which provisions no database - can never be green. The
// `live-db-tests` feature is the same gate the crate's `apply_api_test`
// integration target carries in Cargo.toml; `required-features` cannot reach
// inside a lib, hence the `cfg` here. Nothing in this module is database-free,
// so the whole module moves behind the gate rather than individual cases.
#[cfg(all(test, feature = "live-db-tests"))]
mod tests {
    use super::*;

    /// The database these tests dial, or a panic naming the provisioner.
    fn test_dsn() -> String {
        zeroship_core::config::test_database_url()
    }

    async fn test_client() -> Client {
        let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
            .await
            .expect("connect to migrate-server test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        assert_platform_schema_present(&client).await;
        client
    }

    /// Tables the fixtures need, ALL of which come from the platform migrations.
    ///
    /// `app_schema_applies` is on this list and the earlier version of this suite
    /// created its own copy of the equivalent table instead. That was the defect:
    /// a hand-rolled `CREATE TABLE IF NOT EXISTS` in the fixture meant the suite
    /// passed against a shape the corpus does not produce, so a column the corpus
    /// declares `NOT NULL` could be nullable here and nothing would notice. The
    /// suite now REQUIRES a migrated database for the table under test as well.
    const REQUIRED_PLATFORM_TABLES: [&str; 4] =
        ["plans", "users", "apps", "app_schema_applies"];

    /// Fail with an actionable message when the target database has no platform schema.
    async fn assert_platform_schema_present(client: &Client) {
        let rows = client
            .query(
                "SELECT table_name FROM information_schema.tables \
                  WHERE table_schema = 'zeroship'",
                &[],
            )
            .await
            .expect("read information_schema for platform tables");
        let present: std::collections::HashSet<String> = rows
            .iter()
            .map(|row| row.get::<_, String>("table_name"))
            .collect();
        let missing: Vec<&str> = REQUIRED_PLATFORM_TABLES
            .iter()
            .copied()
            .filter(|table| !present.contains(*table))
            .collect();
        assert!(
            missing.is_empty(),
            "the target database has no platform schema - missing zeroship.{}.\n\
             This suite needs a database the PLATFORM migrations have been applied to.\n\
             Point it at one, e.g. run `tests/provision_test_backends.sh` to provision \
             a test database and generate the TOML overlay, then:\n  \
             cargo test -p zeroship-migrate-server --features live-db-tests\n\
             The DSN comes from `zeroship_core::config::test_database_url()`, which \
             panics naming that command when nothing is configured.",
            missing.join(", zeroship."),
        );
    }

    async fn insert_transition_row(
        client: &Client,
        app_id: &AppId,
        migration_id: Uuid,
        principal_id: Uuid,
        status: &str,
    ) {
        client
            .execute(
                "INSERT INTO zeroship.app_schema_applies \
                    (app_id, migration_id, status, request_body, effective_profile, \
                     ceiling_id, ceiling_version, descriptor_sha256, applied_versions, \
                     submitted_by, submitted_at, applied_at, last_error) \
                 VALUES ($1::text, $2, $3, '{\"marker\":\"original\"}'::jsonb, \
                         '{\"require_rls\":true}'::jsonb, 'test-ceiling', 7, \
                         repeat('a', 64), '[\"mig_original\"]'::jsonb, $4, \
                         TIMESTAMPTZ '2026-08-01 01:02:03+00', \
                         CASE WHEN $3 = 'applied' \
                              THEN TIMESTAMPTZ '2026-08-01 03:04:05+00' END, \
                         CASE WHEN $3 = 'failed' THEN 'original failure' END)",
                &[&app_id.as_str(), &migration_id, &status, &principal_id],
            )
            .await
            .expect("insert transition test row");
    }

    async fn seed_transition_dependencies(client: &Client, app_id: &AppId, principal_id: Uuid) {
        let plan_id = "pln_schema_apply_transition_test";
        client
            .execute(
                "INSERT INTO zeroship.plans \
                    (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                     runtime_limits_json) \
                 VALUES ($1, 'Schema Apply Transition Test', 0, 0, 0, '{}'::jsonb) \
                 ON CONFLICT (id) DO NOTHING",
                &[&plan_id],
            )
            .await
            .expect("seed transition test plan");
        let email = format!("schema-apply-{principal_id}@zeroship.test");
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, 'Schema Apply Test User', NOW())",
                &[&principal_id, &email],
            )
            .await
            .expect("seed transition test user");
        let app_name = format!("schema-apply-{}", app_id.as_str());
        // An app needs a project and a project needs an organization:
        // `apps.project_id` is NOT NULL against a RESTRICT foreign key. This
        // fixture is about the apply-record state machine, not about who may
        // apply, so the organization is left member-less.
        let organization_id = zeroship_core::typed_id::generate("org");
        let project_id = zeroship_core::typed_id::generate("prj");
        client
            .execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
                 VALUES ($1, $2, 'Schema Apply Fixture', 'fixture@zeroship.test')",
                &[
                    &organization_id,
                    &format!("schema-apply-org-{}", Uuid::new_v4().simple()),
                ],
            )
            .await
            .expect("seed transition test organization");
        client
            .execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
                 VALUES ($1, $2, 'default', 'Default')",
                &[&project_id, &organization_id],
            )
            .await
            .expect("seed transition test project");
        client
            .execute(
                "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
                 SELECT $1::text, $2, $3, p.id, p.organization_id \
                   FROM zeroship.projects p WHERE p.id = $4",
                &[&app_id.as_str(), &app_name, &plan_id, &project_id],
            )
            .await
            .expect("seed transition test app");
    }

    /// Terminal states are terminal: neither marking may overwrite the other, and
    /// the loser SAYS SO.
    ///
    /// The guard is what stops a late error path flipping a row the engine already
    /// applied: the DDL is committed and cannot be taken back, so an operator
    /// reading `status = 'failed'` would believe nothing changed while the app's
    /// schema had in fact moved.
    ///
    /// What this does NOT cover: the callers in `apply.rs` acting on `Lost`. They
    /// log and continue, and nothing here observes a log.
    #[ntex::test]
    async fn terminal_status_transitions_do_not_clobber_each_other_pg() {
        let client = test_client().await;
        let store = SchemaApplyStore::new(test_dsn());
        let app_id = AppId::mint();
        let principal_id = Uuid::new_v4();
        let applied_id = Uuid::now_v7();
        let failed_id = Uuid::now_v7();

        seed_transition_dependencies(&client, &app_id, principal_id).await;
        insert_transition_row(&client, &app_id, applied_id, principal_id, "applied").await;
        insert_transition_row(&client, &app_id, failed_id, principal_id, "failed").await;

        // An error path firing after a successful apply must not erase it.
        let outcome = store
            .mark_failed(&app_id, applied_id, "late error on an applied migration")
            .await
            .expect("call must not error");
        assert_eq!(
            outcome,
            TerminalTransition::Lost,
            "a failure that matched no row must report the loss, not success"
        );
        let row = client
            .query_one(
                "SELECT status FROM zeroship.app_schema_applies \
                  WHERE app_id = $1::text AND migration_id = $2",
                &[&app_id.as_str(), &applied_id],
            )
            .await
            .expect("read the applied row");
        assert_eq!(
            row.get::<_, String>("status"),
            "applied",
            "a failed marking must not overwrite an applied migration"
        );

        // And the converse: a failed migration must not silently become applied.
        let outcome = store
            .mark_applied(&app_id, failed_id, &["mig_late".to_string()])
            .await
            .expect("call must not error");
        assert_eq!(
            outcome,
            TerminalTransition::Lost,
            "an apply that matched no row must report the loss, not success"
        );
        let row = client
            .query_one(
                "SELECT status FROM zeroship.app_schema_applies \
                  WHERE app_id = $1::text AND migration_id = $2",
                &[&app_id.as_str(), &failed_id],
            )
            .await
            .expect("read the failed row");
        assert_eq!(
            row.get::<_, String>("status"),
            "failed",
            "an applied marking must not overwrite a failed migration"
        );

        // POSITIVE CONTROL. Both assertions above are satisfied by a method that
        // returns `Lost` unconditionally, which is the same shape of mistake the
        // discarded row count was. A transition that WINS must say `Recorded`, or
        // `Lost` carries no information.
        let winning_id = Uuid::now_v7();
        insert_transition_row(&client, &app_id, winning_id, principal_id, "submitted").await;
        let outcome = store
            .mark_applied(&app_id, winning_id, &["mig_winner".to_string()])
            .await
            .expect("call must not error");
        assert_eq!(
            outcome,
            TerminalTransition::Recorded,
            "a transition that matched its row must report success"
        );
        let row = client
            .query_one(
                "SELECT status, applied_versions FROM zeroship.app_schema_applies \
                  WHERE app_id = $1::text AND migration_id = $2",
                &[&app_id.as_str(), &winning_id],
            )
            .await
            .expect("read the winning row");
        assert_eq!(
            row.get::<_, String>("status"),
            "applied",
            "a submitted migration must reach applied"
        );
        assert_eq!(
            row.get::<_, Value>("applied_versions"),
            serde_json::json!(["mig_winner"]),
            "the engine's applied set must be recorded, not the row's prior value"
        );
    }

    /// A request that applied NOTHING still lands a row, and the row says so.
    ///
    /// This is the case the deploy precondition depends on: an engine upgrade that
    /// changes descriptor bytes without changing any schema must be repairable by a
    /// migrate that applies nothing. If `mark_applied` skipped the write for an
    /// empty set - a natural-looking optimisation - every such app's next deploy
    /// would be refused forever.
    #[ntex::test]
    async fn an_apply_that_advanced_nothing_still_records_an_applied_row_pg() {
        let client = test_client().await;
        let store = SchemaApplyStore::new(test_dsn());
        let app_id = AppId::mint();
        let principal_id = Uuid::new_v4();
        let migration_id = Uuid::now_v7();

        seed_transition_dependencies(&client, &app_id, principal_id).await;
        insert_transition_row(&client, &app_id, migration_id, principal_id, "submitted").await;

        let outcome = store
            .mark_applied(&app_id, migration_id, &[])
            .await
            .expect("call must not error");
        assert_eq!(outcome, TerminalTransition::Recorded);
        let row = client
            .query_one(
                "SELECT status, applied_versions, applied_at IS NOT NULL AS stamped \
                   FROM zeroship.app_schema_applies \
                  WHERE app_id = $1::text AND migration_id = $2",
                &[&app_id.as_str(), &migration_id],
            )
            .await
            .expect("read the row");
        assert_eq!(row.get::<_, String>("status"), "applied");
        assert!(row.get::<_, bool>("stamped"), "applied_at must be stamped");
        assert_eq!(
            row.get::<_, Value>("applied_versions"),
            serde_json::json!([]),
            "an apply that advanced nothing must be visible as an empty applied set"
        );
    }
}
