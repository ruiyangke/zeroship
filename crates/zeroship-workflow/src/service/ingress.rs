use super::{
    app::{encode, lock_app, lock_run, parse_state, request_result, store_request},
    capability::{
        mint_signal_capability, verify_signal_capability, CapabilityToken, SignalGrant,
        SignalTarget, WORKFLOW_AUDIENCE,
    },
    models, signals,
    store::Transaction,
    types::digest,
    AppWorkflows, RequestId, WorkflowService,
};
use crate::{operations::SignalOptions, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{ServiceIssuer, ServiceSigningKey, ServiceTrustBundle},
};
use zeroship_data_orm::{
    orm::{Entity, FindOptions},
    value,
};

pub struct SignalAuthority {
    key: Arc<ServiceSigningKey>,
    trust: ServiceTrustBundle,
}
impl std::fmt::Debug for SignalAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalAuthority").finish_non_exhaustive()
    }
}
impl SignalAuthority {
    pub fn new(
        key: Arc<ServiceSigningKey>,
        mut trust: ServiceTrustBundle,
    ) -> Result<Self, WorkflowServiceError> {
        if trust
            .issuers_publishing(&key.verifying_key_bytes())
            .iter()
            .any(|issuer| *issuer != WORKFLOW_AUDIENCE)
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow signal signing key belongs to another service".into(),
            ));
        }
        let issuer = ServiceIssuer::parse(WORKFLOW_AUDIENCE)
            .map_err(|_| WorkflowServiceError::Internal("invalid workflow issuer".into()))?;
        trust
            .trust_signing_key(&issuer, key.key_id(), &key)
            .map_err(|_| {
                WorkflowServiceError::InvalidRequest(
                    "conflicting workflow signal verification key".into(),
                )
            })?;
        Ok(Self { key, trust })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalTokenRequest {
    pub target: SignalTarget,
    pub types: BTreeSet<String>,
    pub lifetime_seconds: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "lowercase")]
pub enum IngressReceipt {
    Direct { id: String },
    Topic { id: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedSignals {
    pub epoch: i64,
}

impl AppWorkflows {
    pub async fn issue_signal_token(
        &self,
        request: &RequestId,
        options: SignalTokenRequest,
    ) -> Result<CapabilityToken, WorkflowServiceError> {
        let authority = authority(&self.service)?;
        let digest = digest(&options)?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&tx, &self.app, request, "issue_signal_token", &digest).await?
        {
            return Ok(receipt);
        }
        policy.admit()?;
        if options.lifetime_seconds <= 0
            || options.lifetime_seconds > policy.max_signal_token_lifetime_seconds
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "signal token lifetime exceeds the app policy".into(),
            ));
        }
        let app_epoch = app_epoch(&mut tx, &self.app).await?;
        let epoch = target_epoch(&mut tx, &self.app, &options.target, true).await?;
        let grant = SignalGrant {
            app_id: self.app.clone(),
            target: options.target,
            types: options.types,
            epoch,
            app_epoch,
        };
        let token = mint_signal_capability(
            &authority.key,
            grant,
            now.div_euclid(1000),
            options.lifetime_seconds,
        )?;
        store_request(
            &mut tx,
            &self.app,
            request,
            "issue_signal_token",
            &digest,
            &token,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(token)
    }

    pub async fn revoke_signal_tokens(
        &self,
        request: &RequestId,
        target: Option<SignalTarget>,
    ) -> Result<RevokedSignals, WorkflowServiceError> {
        let digest = digest(&target)?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&tx, &self.app, request, "revoke_signal_tokens", &digest).await?
        {
            return Ok(receipt);
        }
        let previous = if let Some(target) = &target {
            target_epoch(&mut tx, &self.app, target, false).await?
        } else {
            app_epoch(&mut tx, &self.app).await?
        };
        let epoch = previous.checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow signal epoch exhausted".into())
        })?;
        match target {
            Some(SignalTarget::Run { run_id }) => {
                tx.database()
                    .collection(models::runs::Entity::COLLECTION)?
                    .update(
                        value!({"app_id":self.app.as_str(), "id":run_id}),
                        value!({"signal_epoch":epoch}),
                    )
                    .await?;
            }
            Some(SignalTarget::Topic { topic }) => {
                tx.database()
                    .collection(models::topics::Entity::COLLECTION)?
                    .update(
                        value!({"app_id":self.app.as_str(), "topic":topic}),
                        value!({"signal_epoch":epoch}),
                    )
                    .await?;
            }
            None => {
                tx.database()
                    .collection(models::app_state::Entity::COLLECTION)?
                    .update(
                        value!({"app_id":self.app.as_str()}),
                        value!({"signal_epoch":epoch}),
                    )
                    .await?;
            }
        }
        let result = RevokedSignals { epoch };
        store_request(
            &mut tx,
            &self.app,
            request,
            "revoke_signal_tokens",
            &digest,
            &result,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}
impl WorkflowService {
    pub async fn ingest_signal(
        &self,
        request: &RequestId,
        token: &str,
        app: &AppId,
        target: &SignalTarget,
        options: SignalOptions,
    ) -> Result<IngressReceipt, WorkflowServiceError> {
        let authority = authority(self)?;
        let mut tx = self.begin().await?;
        let now = tx.now().await?;
        let grant = verify_signal_capability(token, &authority.trust, now.div_euclid(1000))?;
        if &grant.app_id != app || &grant.target != target {
            return Err(WorkflowServiceError::NotFound(
                "workflow signal target not found".into(),
            ));
        }
        if !grant.types.contains(&options.signal_type) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let policy = lock_app(&mut tx, app).await?;
        let now = tx.now().await?;
        verify_signal_capability(token, &authority.trust, now.div_euclid(1000))?;
        if app_epoch(&mut tx, app).await? != grant.app_epoch
            || target_epoch(&mut tx, app, target, false).await? != grant.epoch
        {
            return Err(WorkflowServiceError::Unauthenticated);
        }
        let digest = digest(&(target, &options))?;
        if let Some(receipt) = request_result(&tx, app, request, "signal_ingress", &digest).await? {
            return Ok(receipt);
        }
        policy.admit()?;
        if !policy.ingress {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        if encode(&options.payload)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let result = match target {
            SignalTarget::Run { run_id } => {
                let delivered =
                    signals::deliver(&mut tx, app, run_id, &options, "ingress", now).await?;
                IngressReceipt::Direct { id: delivered.id }
            }
            SignalTarget::Topic { topic } => {
                let broadcast =
                    signals::publish(&mut tx, app, topic, &options, "ingress", now).await?;
                IngressReceipt::Topic { id: broadcast.id }
            }
        };
        store_request(
            &mut tx,
            app,
            request,
            "signal_ingress",
            &digest,
            &result,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}
fn authority(service: &WorkflowService) -> Result<Arc<SignalAuthority>, WorkflowServiceError> {
    service.signal_authority.clone().ok_or_else(|| {
        WorkflowServiceError::Unavailable("workflow signal authority is not configured".into())
    })
}
async fn app_epoch(tx: &mut Transaction, app: &AppId) -> Result<i64, WorkflowServiceError> {
    let rows = tx
        .database()
        .entity::<models::app_state::Entity>()?
        .find::<models::AppSignalEpoch>(
            models::app_state::app_id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    Ok(rows
        .first()
        .ok_or_else(|| WorkflowServiceError::NotFound("workflow app not found".into()))?
        .signal_epoch)
}
async fn target_epoch(
    tx: &mut Transaction,
    app: &AppId,
    target: &SignalTarget,
    issuing: bool,
) -> Result<i64, WorkflowServiceError> {
    match target {
        SignalTarget::Run { run_id } => {
            super::app::validate_run(run_id)?;
            let run = lock_run(tx, app, run_id).await?;
            if issuing && parse_state(&run.text("state")?)?.is_terminal() {
                return Err(WorkflowServiceError::Conflict(
                    "cannot issue a signal token for a terminal run".into(),
                ));
            }
            run.integer("signal_epoch")
        }
        SignalTarget::Topic { topic } => {
            signals::validate_topic(topic)?;
            let rows = tx
                .database()
                .entity::<models::topics::Entity>()?
                .find::<models::TopicSignalEpoch>(
                    models::topics::app_id
                        .eq(app.as_str())?
                        .and(models::topics::topic.eq(topic.as_str())?),
                    FindOptions {
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await?;
            if let Some(row) = rows.first() {
                return Ok(row.signal_epoch);
            }
            // Callers hold the app lock through commit, so initialization cannot
            // race another issuer or overwrite a persisted revocation epoch.
            tx.database()
                .collection(models::topics::Entity::COLLECTION)?
                .insert(value!({"id":super::types::storage_id(), "app_id":app.as_str(), "topic":topic, "signal_epoch":0}))
                .await?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_authority_refuses_another_services_signing_key() {
        let key = Arc::new(ServiceSigningKey::generate());
        let mut trust = ServiceTrustBundle::new();
        trust
            .trust_signing_key(
                &ServiceIssuer::parse("spiffe://zeroship.ai/svc/control").unwrap(),
                key.key_id(),
                &key,
            )
            .unwrap();
        assert!(SignalAuthority::new(key, trust).is_err());
    }
}
