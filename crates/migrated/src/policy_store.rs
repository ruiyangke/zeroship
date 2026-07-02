use std::convert::TryFrom;

use compio_postgres::{Client, NoTls};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_migrate::PolicyProfile;

use crate::policy::EffectivePolicy;

#[derive(Debug, Clone)]
pub struct AppPolicyStore {
    dsn: String,
}

impl AppPolicyStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    pub async fn insert_version(
        &self,
        app_id: Uuid,
        submitted_by: Uuid,
        raw_toml: &str,
        parsed_profile: &PolicyProfile,
        effective: &EffectivePolicy,
    ) -> Result<AppPolicyRecord, AppPolicyStoreError> {
        let ceiling_version = i64::try_from(effective.ceiling_version)
            .map_err(|_| AppPolicyStoreError::CeilingVersionOverflow(effective.ceiling_version))?;
        let parsed_json = serde_json::to_value(parsed_profile)
            .map_err(|err| AppPolicyStoreError::EncodeProfile(err.to_string()))?;
        let effective_json = serde_json::to_value(&effective.profile)
            .map_err(|err| AppPolicyStoreError::EncodeProfile(err.to_string()))?;
        let client = self.connect().await?;
        client
            .batch_execute("BEGIN")
            .await
            .map_err(AppPolicyStoreError::Query)?;

        let result = async {
            client
                .execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text, 0))",
                    &[&app_id],
                )
                .await?;
            let rows = client
                .query(
                    "INSERT INTO zeroship.migrated_app_policies \
                        (app_id, version, raw_toml, parsed_profile, effective_profile, \
                         ceiling_id, ceiling_version, submitted_by) \
                     SELECT $1, COALESCE(MAX(version), 0) + 1, $2, $3::jsonb, $4::jsonb, \
                            $5, $6, $7 \
                       FROM zeroship.migrated_app_policies \
                      WHERE app_id = $1 \
                     RETURNING app_id, version, raw_toml, parsed_profile, effective_profile, \
                               ceiling_id, ceiling_version, submitted_by, \
                               submitted_at::text AS submitted_at",
                    &[
                        &app_id,
                        &raw_toml,
                        &parsed_json,
                        &effective_json,
                        &effective.ceiling_id,
                        &ceiling_version,
                        &submitted_by,
                    ],
                )
                .await?;
            client.batch_execute("COMMIT").await?;
            Ok::<_, compio_postgres::Error>(
                AppPolicyRecord::from_row(rows.first().expect("INSERT RETURNING row")),
            )
        }
        .await;

        match result {
            Ok(record) => Ok(record),
            Err(err) => {
                let _ = client.batch_execute("ROLLBACK").await;
                Err(AppPolicyStoreError::Query(err))
            }
        }
    }

    pub async fn get_current(
        &self,
        app_id: Uuid,
    ) -> Result<Option<AppPolicyRecord>, AppPolicyStoreError> {
        self.get(app_id, None).await
    }

    pub async fn get(
        &self,
        app_id: Uuid,
        version: Option<i64>,
    ) -> Result<Option<AppPolicyRecord>, AppPolicyStoreError> {
        let client = self.connect().await?;
        let rows = if let Some(version) = version {
            client
                .query(
                    "SELECT app_id, version, raw_toml, parsed_profile, effective_profile, \
                            ceiling_id, ceiling_version, submitted_by, \
                            submitted_at::text AS submitted_at \
                       FROM zeroship.migrated_app_policies \
                      WHERE app_id = $1 AND version = $2",
                    &[&app_id, &version],
                )
                .await
        } else {
            client
                .query(
                    "SELECT app_id, version, raw_toml, parsed_profile, effective_profile, \
                            ceiling_id, ceiling_version, submitted_by, \
                            submitted_at::text AS submitted_at \
                       FROM zeroship.migrated_app_policies \
                      WHERE app_id = $1 \
                      ORDER BY version DESC \
                      LIMIT 1",
                    &[&app_id],
                )
                .await
        }
        .map_err(AppPolicyStoreError::Query)?;
        Ok(rows.first().map(AppPolicyRecord::from_row))
    }

    pub async fn list_versions(
        &self,
        app_id: Uuid,
    ) -> Result<Vec<AppPolicyVersion>, AppPolicyStoreError> {
        let client = self.connect().await?;
        let rows = client
            .query(
                "SELECT app_id, version, ceiling_id, ceiling_version, submitted_by, \
                        submitted_at::text AS submitted_at \
                   FROM zeroship.migrated_app_policies \
                  WHERE app_id = $1 \
                  ORDER BY version ASC",
                &[&app_id],
            )
            .await
            .map_err(AppPolicyStoreError::Query)?;
        Ok(rows.iter().map(AppPolicyVersion::from_row).collect())
    }

    async fn connect(&self) -> Result<Client, AppPolicyStoreError> {
        let (client, conn) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(AppPolicyStoreError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = conn.run().await {
                tracing::debug!(error = %err, "migrated: app policy store connection ended");
            }
        })
        .detach();
        Ok(client)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppPolicyRecord {
    pub app_id: Uuid,
    pub version: i64,
    pub raw_toml: String,
    pub parsed_profile: Value,
    pub effective_profile: Value,
    pub ceiling_id: String,
    pub ceiling_version: i64,
    pub submitted_by: Uuid,
    pub submitted_at: String,
}

impl AppPolicyRecord {
    fn from_row(row: &compio_postgres::Row) -> Self {
        Self {
            app_id: row.get("app_id"),
            version: row.get("version"),
            raw_toml: row.get("raw_toml"),
            parsed_profile: row.get("parsed_profile"),
            effective_profile: row.get("effective_profile"),
            ceiling_id: row.get("ceiling_id"),
            ceiling_version: row.get("ceiling_version"),
            submitted_by: row.get("submitted_by"),
            submitted_at: row.get("submitted_at"),
        }
    }

    pub fn parsed_policy_profile(&self) -> Result<PolicyProfile, AppPolicyStoreError> {
        serde_json::from_value(self.parsed_profile.clone())
            .map_err(|err| AppPolicyStoreError::DecodeProfile(err.to_string()))
    }

    pub fn effective_policy_profile(&self) -> Result<PolicyProfile, AppPolicyStoreError> {
        serde_json::from_value(self.effective_profile.clone())
            .map_err(|err| AppPolicyStoreError::DecodeProfile(err.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppPolicyVersion {
    pub app_id: Uuid,
    pub version: i64,
    pub ceiling_id: String,
    pub ceiling_version: i64,
    pub submitted_by: Uuid,
    pub submitted_at: String,
}

impl AppPolicyVersion {
    fn from_row(row: &compio_postgres::Row) -> Self {
        Self {
            app_id: row.get("app_id"),
            version: row.get("version"),
            ceiling_id: row.get("ceiling_id"),
            ceiling_version: row.get("ceiling_version"),
            submitted_by: row.get("submitted_by"),
            submitted_at: row.get("submitted_at"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppPolicyStoreError {
    #[error("policy store connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("policy store query: {0}")]
    Query(compio_postgres::Error),
    #[error("encode policy profile: {0}")]
    EncodeProfile(String),
    #[error("decode policy profile: {0}")]
    DecodeProfile(String),
    #[error("ceiling version {0} cannot be stored as BIGINT")]
    CeilingVersionOverflow(u64),
}
