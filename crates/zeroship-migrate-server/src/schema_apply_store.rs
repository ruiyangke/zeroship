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
use zeroship_id::{AppId, UserId};

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
        let ceiling_version = i64::try_from(input.ceiling_version)
            .map_err(|_| SchemaApplyStoreError::CeilingVersionOverflow(input.ceiling_version))?;
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
                    &input.principal_id.as_str(),
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
    pub principal_id: &'a UserId,
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
