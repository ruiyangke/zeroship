//! Enrolled worker identity, checked against the live registry.

use compio_postgres::Pool;
use std::sync::Arc;
use zeroship_core::service_assertion::{
    presented_issuer, thumbprint_key_id, InMemoryReplayStore, ServiceAssertionVerifier,
    ServiceTrustBundle,
};
use zeroship_core::service_identity::{endpoints, verify_service_call};
use zeroship_core::service_peers::service_issuer;

type Error = Box<dyn std::error::Error>;

pub(crate) async fn public_key(pool: &Pool, instance: &str) -> Result<Option<[u8; 32]>, Error> {
    let rows = pool
        .query(
            "SELECT public_key FROM zeroship.worker_instances WHERE id = $1 AND status = 'active'",
            &[&instance],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let bytes: Vec<u8> = row.try_get(0)?;
    Ok(bytes.as_slice().try_into().ok())
}

pub(crate) async fn verify(
    pool: &Pool,
    replay: Arc<InMemoryReplayStore>,
    authorization: &str,
) -> Result<(String, [u8; 32]), Error> {
    let issuer = presented_issuer(Some(authorization)).ok_or("invalid worker assertion")?;
    if issuer.principal() != service_issuer("svc/worker")?.principal() {
        return Err("worker identity required".into());
    }
    let instance = issuer
        .instance()
        .ok_or("enrolled worker instance required")?;
    let public = public_key(pool, instance)
        .await?
        .ok_or("worker instance inactive")?;
    let mut bundle = ServiceTrustBundle::new();
    bundle.trust(&issuer, thumbprint_key_id(&public), public)?;
    replay.purge_expired(std::time::SystemTime::now());
    let verifier = ServiceAssertionVerifier::new(bundle, replay);
    verify_service_call(
        &verifier,
        Some(authorization),
        service_issuer("svc/cdc")?.as_str(),
        endpoints::CDC_SUBSCRIBE,
    )
    .await
    .map_err(|_| "worker assertion rejected")?;
    Ok((instance.into(), public))
}
