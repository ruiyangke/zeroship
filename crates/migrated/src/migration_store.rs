use std::convert::TryFrom;

use compio_postgres::{Client, NoTls};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_migrate::PolicyProfile;

#[derive(Debug, Clone)]
pub struct MigrationStore {
    dsn: String,
}

impl MigrationStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    pub async fn insert_submitted(
        &self,
        input: StoreMigrationInput<'_>,
    ) -> Result<(), MigrationStoreError> {
        self.insert_migration(input, MigrationStatus::Submitted).await
    }

    pub async fn insert_pending(
        &self,
        input: StoreMigrationInput<'_>,
    ) -> Result<(), MigrationStoreError> {
        self.insert_migration(input, MigrationStatus::PendingApproval)
            .await
    }

    async fn insert_migration(
        &self,
        input: StoreMigrationInput<'_>,
        status: MigrationStatus,
    ) -> Result<(), MigrationStoreError> {
        let ceiling_version =
            i64::try_from(input.ceiling_version).map_err(|_| {
                MigrationStoreError::CeilingVersionOverflow(input.ceiling_version)
            })?;
        let effective_profile = serde_json::to_value(input.effective_profile)
            .map_err(|err| MigrationStoreError::EncodeProfile(err.to_string()))?;
        let gated_versions = serde_json::to_value(input.gated_versions)
            .map_err(|err| MigrationStoreError::EncodeVersions(err.to_string()))?;
        let client = self.connect().await?;
        client
            .execute(
                "INSERT INTO zeroship.migrated_migrations \
                    (app_id, migration_id, status, request_body, effective_profile, \
                     ceiling_id, ceiling_version, gated_versions, submitted_by) \
                 VALUES ($1, $2, $3, $4::jsonb, $5::jsonb, $6, $7, $8::jsonb, $9)",
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
                ],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
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
                        last_error \
                   FROM zeroship.migrated_migrations \
                  WHERE app_id = $1 AND migration_id = $2 AND status = 'pending_approval'",
                &[&app_id, &migration_id],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        rows.first().map(StoredMigration::from_row).transpose()
    }

    pub async fn mark_approved(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
        approved_by: Uuid,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        let updated = client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'approved', approved_by = $3, approved_at = NOW(), last_error = NULL \
                  WHERE app_id = $1 AND migration_id = $2 AND status = 'pending_approval'",
                &[&app_id, &migration_id, &approved_by],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        if updated == 0 {
            return Err(MigrationStoreError::NotPending);
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
                  WHERE app_id = $1 AND migration_id = $2",
                &[&app_id, &migration_id],
            )
            .await
            .map_err(MigrationStoreError::Query)?;
        Ok(())
    }

    pub async fn mark_failed(
        &self,
        app_id: Uuid,
        migration_id: Uuid,
        message: &str,
    ) -> Result<(), MigrationStoreError> {
        let client = self.connect().await?;
        client
            .execute(
                "UPDATE zeroship.migrated_migrations \
                    SET status = 'failed', last_error = $3 \
                  WHERE app_id = $1 AND migration_id = $2",
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
        let effective_profile = serde_json::to_value(input.effective_profile)
            .map_err(|err| MigrationStoreError::EncodeProfile(err.to_string()))?;
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
    pub effective_profile: &'a PolicyProfile,
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
    pub effective_profile: &'a PolicyProfile,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationStatus {
    Submitted,
    PendingApproval,
}

impl MigrationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::PendingApproval => "pending_approval",
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
            last_error: row.get("last_error"),
        })
    }

    pub fn effective_policy_profile(&self) -> Result<PolicyProfile, MigrationStoreError> {
        serde_json::from_value(self.effective_profile.clone())
            .map_err(|err| MigrationStoreError::DecodeProfile(err.to_string()))
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
}

impl Default for AuditAction {
    fn default() -> Self {
        Self::Submit
    }
}

pub fn sealed_profile_audit_json(
    posture: impl std::fmt::Debug,
    ceiling_version: u64,
    issued_at: u64,
    nonce: &[u8],
) -> Value {
    json!({
        "posture": format!("{posture:?}"),
        "ceiling_version": ceiling_version,
        "issued_at": issued_at,
        "nonce_hex": hex_bytes(nonce),
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
