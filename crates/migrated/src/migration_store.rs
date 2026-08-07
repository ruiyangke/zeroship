use std::convert::TryFrom;

use compio_postgres::{Client, NoTls};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::policy::ManagedPosture;

#[derive(Debug, Clone)]
pub struct MigrationStore {
    dsn: String,
}

impl MigrationStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    /// Plan transition — the migration needs NO approval: insert AUTO-`approved`,
    /// stamping the resolved content checksum as `approved_checksum` so the apply gate
    /// can detect drift the same way it does for operator-approved migrations.
    pub async fn insert_auto_approved(
        &self,
        input: StoreMigrationInput<'_>,
        approved_checksum: &str,
    ) -> Result<(), MigrationStoreError> {
        self.insert_migration(input, MigrationStatus::Approved, Some(approved_checksum))
            .await
    }

    /// Plan transition — the migration REQUIRES approval: insert `pending_approval`
    /// (no `approved_checksum` yet; it is stamped at `approve()`).
    pub async fn insert_pending(
        &self,
        input: StoreMigrationInput<'_>,
    ) -> Result<(), MigrationStoreError> {
        self.insert_migration(input, MigrationStatus::PendingApproval, None)
            .await
    }

    async fn insert_migration(
        &self,
        input: StoreMigrationInput<'_>,
        status: MigrationStatus,
        approved_checksum: Option<&str>,
    ) -> Result<(), MigrationStoreError> {
        let ceiling_version =
            i64::try_from(input.ceiling_version).map_err(|_| {
                MigrationStoreError::CeilingVersionOverflow(input.ceiling_version)
            })?;
        let effective_profile = input.effective_profile.to_audit_json();
        let gated_versions = serde_json::to_value(input.gated_versions)
            .map_err(|err| MigrationStoreError::EncodeVersions(err.to_string()))?;
        let client = self.connect().await?;
        client
            .execute(
                "INSERT INTO zeroship.migrated_migrations \
                    (app_id, migration_id, status, request_body, effective_profile, \
                     ceiling_id, ceiling_version, gated_versions, submitted_by, \
                     approved_checksum) \
                 VALUES ($1, $2, $3, $4::jsonb, $5::jsonb, $6, $7, $8::jsonb, $9, $10)",
                &[
                    &input.app_id,
                    &input.migration_id,
                    &status.as_str(),
                    &input.request_body,
                    &effective_profile,
                    &input.ceiling_id,
                    &ceiling_version,
                    &gated_versions,
                    &input.principal_id,
                    &approved_checksum,
                ],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
    }

    /// The migration already awaiting approval for this app with the SAME request
    /// body, if there is one.
    ///
    /// What an operator approves is the CONTENT, so identical content is one
    /// pending migration however many times it is submitted. Without this a
    /// retrying deploy inserts a fresh row per attempt: the operator sees several
    /// rows for one decision, approving one leaves the rest pending forever, and
    /// nothing reaps them.
    pub async fn find_pending_for_request(
        &self,
        app_id: Uuid,
        request_body: &serde_json::Value,
    ) -> Result<Option<Uuid>, MigrationStoreError> {
        let client = self.connect().await?;
        let rows = client
            .query(
                "SELECT migration_id FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND status = 'pending_approval' \
                    AND request_body = $2::jsonb \
                  ORDER BY submitted_at ASC LIMIT 1",
                &[&app_id, &request_body],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(rows.first().map(|row| row.get("migration_id")))
    }

    pub async fn get_pending(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
    ) -> Result<Option<StoredMigration>, MigrationStoreError> {
        let client = self.connect().await?;
        let rows = client
            .query(
                "SELECT app_id, migration_id, status, request_body, effective_profile, \
                        ceiling_id, ceiling_version, gated_versions, submitted_by, \
                        submitted_at::text AS submitted_at, approved_by, \
                        approved_at::text AS approved_at, applied_at::text AS applied_at, \
                        approved_checksum, last_error \
                   FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND migration_id = $2 AND status = 'pending_approval'",
                &[&app_id, &migration_id],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        rows.first().map(StoredMigration::from_row).transpose()
    }

    /// APPROVE transition: `pending_approval` → `approved`, stamping `approved_by`,
    /// `approved_at`, and `approved_checksum = X` (the content checksum the operator
    /// reviewed). The apply gate later re-resolves the migration to X' and proceeds
    /// only if `X' == approved_checksum` — so this checksum closes the approve/apply
    /// TOCTOU. The write is guarded on the current status so a double-approve or an
    /// approve of an already-applied/rejected row is a no-op ([`Self::NotPending`]).
    pub async fn mark_approved(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
        approved_by: Uuid,
        approved_checksum: &str,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        let updated = client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'approved', approved_by = $3, approved_at = NOW(), \
                        approved_checksum = $4, last_error = NULL \
                  WHERE app_id = $1 AND migration_id = $2 AND status = 'pending_approval'",
                &[&app_id, &migration_id, &approved_by, &approved_checksum],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        if updated == 0 {
            return Err(MigrationStoreError::NotPending);
        }
        Ok(())
    }

    /// Apply-gate DRIFT transition: an `approved` migration whose re-resolved content
    /// checksum X' NO LONGER matches the `approved_checksum` reverts to
    /// `pending_approval` (clearing the stale approval) — it must be re-reviewed. This
    /// is the fail-closed arm of the TOCTOU gate.
    pub async fn revert_to_pending(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
        message: &str,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        let updated = client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'pending_approval', approved_by = NULL, approved_at = NULL, \
                        approved_checksum = NULL, last_error = $3 \
                  WHERE app_id = $1 AND migration_id = $2 AND status = 'approved'",
                &[&app_id, &migration_id, &message],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        if updated == 0 {
            return Err(MigrationStoreError::InvalidTransition);
        }
        Ok(())
    }

    pub async fn mark_applied(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'applied', applied_at = NOW(), last_error = NULL \
                  WHERE app_id = $1 AND migration_id = $2 \
                    AND status <> 'rejected'",
                &[&app_id, &migration_id],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
    }

    /// REJECT transition: a migration that failed preflight / drifted-and-abandoned /
    /// errored moves to `rejected` (terminal) with the failure message.
    pub async fn mark_rejected(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
        message: &str,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'rejected', last_error = $3 \
                  WHERE app_id = $1 AND migration_id = $2 \
                    AND status <> 'applied'",
                &[&app_id, &migration_id, &message],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
    }

    pub async fn record_audit(&self, input: AuditInput<'_>) -> Result<(), MigrationStoreError> {
        let ceiling_version =
            i64::try_from(input.ceiling_version).map_err(|_| {
                MigrationStoreError::CeilingVersionOverflow(input.ceiling_version)
            })?;
        let effective_profile = input.effective_profile.to_audit_json();
        let migration_versions = serde_json::to_value(input.migration_versions)
            .map_err(|err| MigrationStoreError::EncodeVersions(err.to_string()))?;
        let client = self.connect().await?;
        client
            .execute(
                "INSERT INTO zeroship.migrated_migration_audit \
                    (app_id, migration_id, migration_versions, action, outcome, \
                     principal_id, effective_profile, sealed_profile, ceiling_id, \
                     ceiling_version, detail) \
                 VALUES ($1, $2, $3::jsonb, $4, $5, $6, $7::jsonb, $8::jsonb, $9, $10, $11::jsonb)",
                &[
                    &input.app_id,
                    &input.migration_id,
                    &migration_versions,
                    &input.action.as_str(),
                    &input.outcome,
                    &input.principal_id,
                    &effective_profile,
                    &input.sealed_profile,
                    &input.ceiling_id,
                    &ceiling_version,
                    &input.detail,
                ],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
    }

    async fn connect(&self) -> Result<Client, MigrationStoreError> {
        let (client, conn) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(MigrationStoreError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = conn.run().await {
                tracing::debug!(error = %err, "migrated: migration store connection ended");
            }
        })
        .detach();
        Ok(client)
    }
}

#[derive(Debug, Clone)]
pub struct StoreMigrationInput<'a> {
    pub app_id: Uuid,
    pub migration_id: Uuid,
    pub principal_id: Uuid,
    pub request_body: Value,
    pub effective_profile: &'a ManagedPosture,
    pub ceiling_id: &'a str,
    pub ceiling_version: u64,
    pub gated_versions: &'a [String],
}

#[derive(Debug, Clone)]
pub struct AuditInput<'a> {
    pub app_id: Uuid,
    pub migration_id: Uuid,
    pub principal_id: Uuid,
    pub migration_versions: &'a [String],
    pub action: AuditAction,
    pub outcome: &'a str,
    pub effective_profile: &'a ManagedPosture,
    pub sealed_profile: Option<Value>,
    pub ceiling_id: &'a str,
    pub ceiling_version: u64,
    pub detail: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Submit,
    RejectPending,
    Approve,
    Apply,
}

impl AuditAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::RejectPending => "reject_pending",
            Self::Approve => "approve",
            Self::Apply => "apply",
        }
    }
}

/// The migration-record lifecycle status. The state machine:
/// `planned` → (`pending_approval` → `approved` | `approved`) → `applied`, with
/// `rejected` as the terminal failure arm. A drifted `approved` reverts to
/// `pending_approval` (see [`MigrationStore::revert_to_pending`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationStatus {
    /// Composed + resolved, decision not yet taken (transient in-request state; the
    /// store lands the row directly in `approved`/`pending_approval`).
    #[allow(dead_code)]
    Planned,
    /// Requires operator approval; awaiting `approve()`.
    PendingApproval,
    /// Approved (operator-approved OR auto-approved because no approval was needed);
    /// carries `approved_checksum`.
    Approved,
    /// Successfully applied (terminal).
    #[allow(dead_code)]
    Applied,
    /// Rejected / failed (terminal).
    #[allow(dead_code)]
    Rejected,
}

impl MigrationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredMigration {
    pub app_id: Uuid,
    pub migration_id: Uuid,
    pub status: String,
    pub request_body: Value,
    pub effective_profile: Value,
    pub ceiling_id: String,
    pub ceiling_version: i64,
    pub gated_versions: Vec<String>,
    pub submitted_by: Uuid,
    pub submitted_at: String,
    pub approved_by: Option<Uuid>,
    pub approved_at: Option<String>,
    pub applied_at: Option<String>,
    /// The content checksum the operator reviewed at `approve()` — the TOCTOU pin the
    /// apply gate re-verifies against the freshly re-resolved migration set.
    pub approved_checksum: Option<String>,
    pub last_error: Option<String>,
}

impl StoredMigration {
    fn from_row(row: &compio_postgres::Row) -> Result<Self, MigrationStoreError> {
        let gated_versions: Value = row.get("gated_versions");
        Ok(Self {
            app_id: row.get("app_id"),
            migration_id: row.get("migration_id"),
            status: row.get("status"),
            request_body: row.get("request_body"),
            effective_profile: row.get("effective_profile"),
            ceiling_id: row.get("ceiling_id"),
            ceiling_version: row.get("ceiling_version"),
            gated_versions: serde_json::from_value(gated_versions)
                .map_err(|err| MigrationStoreError::DecodeVersions(err.to_string()))?,
            submitted_by: row.get("submitted_by"),
            submitted_at: row.get("submitted_at"),
            approved_by: row.get("approved_by"),
            approved_at: row.get("approved_at"),
            applied_at: row.get("applied_at"),
            approved_checksum: row.get("approved_checksum"),
            last_error: row.get("last_error"),
        })
    }

}

#[derive(Debug, thiserror::Error)]
pub enum MigrationStoreError {
    #[error("migration store connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("migration store query: {0}")]
    Query(compio_postgres::Error),
    #[error("encode policy profile: {0}")]
    EncodeProfile(String),
    #[error("decode policy profile: {0}")]
    DecodeProfile(String),
    #[error("encode migration versions: {0}")]
    EncodeVersions(String),
    #[error("decode migration versions: {0}")]
    DecodeVersions(String),
    #[error("ceiling version {0} cannot be stored as BIGINT")]
    CeilingVersionOverflow(u64),
    #[error("migration is not pending approval")]
    NotPending,
    #[error("invalid migration transition: expected approved status")]
    InvalidTransition,
}

impl Default for AuditAction {
    fn default() -> Self {
        Self::Submit
    }
}

/// Audit view of a sealed managed policy. Records the seal's public BINDING fields
/// (dialect, matcher version, ceiling version, registry digest) — the tamper-evidence
/// identity boundary the seal HMAC binds. (The old `PolicyProfile`-era seal exposed a
/// posture/issued-at/nonce; the surviving `zero-migrate-policy` seal binds these.)
pub fn sealed_profile_audit_json(
    dialect: &str,
    matcher_version: u32,
    ceiling_version: u64,
    registry_digest: &[u8],
) -> Value {
    json!({
        "dialect": dialect,
        "matcher_version": matcher_version,
        "ceiling_version": ceiling_version,
        "registry_digest_hex": hex_bytes(registry_digest),
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TEST_DSN: &str =
        "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_control_test";
    const INVALID_TRANSITION_ERROR: &str =
        "invalid migration transition: expected approved status";

    fn test_dsn() -> String {
        std::env::var("MIGRATED_TEST_DB")
            .or_else(|_| std::env::var("CONTROL_TEST_DB"))
            .or_else(|_| std::env::var("PG_TEST_URL"))
            .unwrap_or_else(|_| DEFAULT_TEST_DSN.to_string())
    }

    async fn test_client() -> Client {
        let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
            .await
            .expect("connect to migrated test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
            .batch_execute(
                r#"
                CREATE SCHEMA IF NOT EXISTS zeroship;
                CREATE TABLE IF NOT EXISTS zeroship.migrated_migrations (
                  app_id uuid NOT NULL,
                  migration_id uuid NOT NULL,
                  status text NOT NULL CHECK (
                    status IN ('planned', 'pending_approval', 'approved', 'applied', 'rejected')
                  ),
                  request_body jsonb NOT NULL,
                  effective_profile jsonb NOT NULL,
                  ceiling_id text NOT NULL,
                  ceiling_version bigint NOT NULL CHECK (ceiling_version > 0),
                  gated_versions jsonb NOT NULL DEFAULT '[]'::jsonb,
                  submitted_by uuid NOT NULL,
                  submitted_at timestamptz NOT NULL DEFAULT now(),
                  approved_by uuid,
                  approved_at timestamptz,
                  applied_at timestamptz,
                  approved_checksum text,
                  last_error text,
                  PRIMARY KEY (app_id, migration_id)
                );
                "#,
            )
            .await
            .expect("ensure migrated migration table");
        client
    }

    async fn insert_transition_row(
        client: &Client,
        app_id: Uuid,
        migration_id: Uuid,
        principal_id: Uuid,
        status: &str,
    ) {
        client
            .execute(
                "INSERT INTO zeroship.migrated_migrations \
                    (app_id, migration_id, status, request_body, effective_profile, \
                     ceiling_id, ceiling_version, gated_versions, submitted_by, submitted_at, \
                     approved_by, approved_at, applied_at, approved_checksum, last_error) \
                 VALUES ($1, $2, $3, '{\"marker\":\"original\"}'::jsonb, \
                         '{\"require_rls\":true}'::jsonb, 'test-ceiling', 7, \
                         '[\"0001\"]'::jsonb, $4, \
                         TIMESTAMPTZ '2026-08-01 01:02:03+00', $4, \
                         TIMESTAMPTZ '2026-08-01 02:03:04+00', \
                         CASE WHEN $3 = 'applied' \
                              THEN TIMESTAMPTZ '2026-08-01 03:04:05+00' END, \
                         'approved-checksum', \
                         CASE WHEN $3 = 'rejected' THEN 'original rejection' END)",
                &[&app_id, &migration_id, &status, &principal_id],
            )
            .await
            .expect("insert transition test row");
    }

    async fn seed_transition_dependencies(client: &Client, app_id: Uuid, principal_id: Uuid) {
        let plan_id = "pln_migration_store_transition_test";
        client
            .execute(
                "INSERT INTO zeroship.plans \
                    (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                     runtime_limits_json) \
                 VALUES ($1, 'Migration Store Transition Test', 0, 0, 0, '{}'::jsonb) \
                 ON CONFLICT (id) DO NOTHING",
                &[&plan_id],
            )
            .await
            .expect("seed transition test plan");
        let email = format!("migration-store-{principal_id}@zeroship.test");
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, 'Migration Store Test User', NOW())",
                &[&principal_id, &email],
            )
            .await
            .expect("seed transition test user");
        let app_name = format!("migration-store-{}", app_id.simple());
        client
            .execute(
                "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash')",
                &[&app_id, &app_name, &plan_id],
            )
            .await
            .expect("seed transition test app");
    }

    async fn row_snapshot(client: &Client, app_id: Uuid, migration_id: Uuid) -> String {
        let rows = client
            .query(
                "SELECT row_to_json(migration_row)::text AS snapshot \
                   FROM zeroship.migrated_migrations AS migration_row \
                  WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &migration_id],
            )
            .await
            .expect("snapshot migration row");
        rows.first().expect("migration row exists").get("snapshot")
    }

    #[ntex::test]
    async fn revert_to_pending_only_transitions_approved_rows_pg() {
        let client = test_client().await;
        let store = MigrationStore::new(test_dsn());
        let app_id = Uuid::now_v7();
        let principal_id = Uuid::new_v4();
        let approved_id = Uuid::now_v7();
        let applied_id = Uuid::now_v7();
        let rejected_id = Uuid::now_v7();

        seed_transition_dependencies(&client, app_id, principal_id).await;
        insert_transition_row(&client, app_id, approved_id, principal_id, "approved").await;
        insert_transition_row(&client, app_id, applied_id, principal_id, "applied").await;
        insert_transition_row(&client, app_id, rejected_id, principal_id, "rejected").await;

        store
            .revert_to_pending(app_id, approved_id, "approved content drifted")
            .await
            .expect("approved row must revert to pending");
        let approved_rows = client
            .query(
                "SELECT status, approved_by, approved_at::text AS approved_at, \
                        approved_checksum, last_error \
                   FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &approved_id],
            )
            .await
            .expect("read reverted approved row");
        let approved = approved_rows.first().expect("approved row exists");
        assert_eq!(approved.get::<_, String>("status"), "pending_approval");
        assert_eq!(approved.get::<_, Option<Uuid>>("approved_by"), None);
        assert_eq!(approved.get::<_, Option<String>>("approved_at"), None);
        assert_eq!(approved.get::<_, Option<String>>("approved_checksum"), None);
        assert_eq!(
            approved.get::<_, Option<String>>("last_error").as_deref(),
            Some("approved content drifted")
        );

        let applied_before = row_snapshot(&client, app_id, applied_id).await;
        let applied_error = store
            .revert_to_pending(app_id, applied_id, "must not replace applied data")
            .await
            .expect_err("reverting an applied row must return an invalid-transition error");
        assert_eq!(applied_error.to_string(), INVALID_TRANSITION_ERROR);
        assert_eq!(
            row_snapshot(&client, app_id, applied_id).await,
            applied_before,
            "applied row must remain byte-identical"
        );

        let rejected_before = row_snapshot(&client, app_id, rejected_id).await;
        let rejected_error = store
            .revert_to_pending(app_id, rejected_id, "must not replace rejected data")
            .await
            .expect_err("reverting a rejected row must return an invalid-transition error");
        assert_eq!(rejected_error.to_string(), INVALID_TRANSITION_ERROR);
        assert_eq!(
            row_snapshot(&client, app_id, rejected_id).await,
            rejected_before,
            "rejected row must remain byte-identical"
        );

        let missing_error = store
            .revert_to_pending(app_id, Uuid::now_v7(), "missing")
            .await
            .expect_err("reverting a missing row must return an invalid-transition error");
        assert_eq!(missing_error.to_string(), INVALID_TRANSITION_ERROR);

        client
            .execute(
                "DELETE FROM zeroship.migrated_migrations WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .expect("delete transition test rows");
        client
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
            .await
            .expect("delete transition test app");
        client
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&principal_id],
            )
            .await
            .expect("delete transition test user");
    }

    /// Terminal states are terminal: neither marking may overwrite the other.
    ///
    /// `mark_applied` and `mark_rejected` updated purely on
    /// `(app_id, migration_id)` with no status precondition, so whichever ran
    /// last won. A migration the engine applied could be flipped to `rejected`
    /// by a later error path, and an operator reading `status = 'rejected'`
    /// would believe nothing changed while the engine journal recorded applied
    /// steps. `revert_to_pending` already guards on the prior status; these two
    /// did not.
    #[ntex::test]
    async fn terminal_status_transitions_do_not_clobber_each_other_pg() {
        let client = test_client().await;
        let store = MigrationStore::new(test_dsn());
        let app_id = Uuid::now_v7();
        let principal_id = Uuid::new_v4();
        let applied_id = Uuid::now_v7();
        let rejected_id = Uuid::now_v7();

        seed_transition_dependencies(&client, app_id, principal_id).await;
        insert_transition_row(&client, app_id, applied_id, principal_id, "applied").await;
        insert_transition_row(&client, app_id, rejected_id, principal_id, "rejected").await;

        // An error path firing after a successful apply must not erase it.
        store
            .mark_rejected(app_id, applied_id, "late error on an applied migration")
            .await
            .expect("call must not error");
        let row = client
            .query_one(
                "SELECT status FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &applied_id],
            )
            .await
            .expect("read the applied row");
        assert_eq!(
            row.get::<_, String>("status"),
            "applied",
            "a rejected marking must not overwrite an applied migration"
        );

        // And the converse: a rejected migration must not silently become applied.
        store
            .mark_applied(app_id, rejected_id)
            .await
            .expect("call must not error");
        let row = client
            .query_one(
                "SELECT status FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &rejected_id],
            )
            .await
            .expect("read the rejected row");
        assert_eq!(
            row.get::<_, String>("status"),
            "rejected",
            "an applied marking must not overwrite a rejected migration"
        );
    }
}
