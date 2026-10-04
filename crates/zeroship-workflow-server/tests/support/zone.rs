use crate::support::platform;
use zeroship_core::{
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    typed_id,
    workflow_coordination::{WorkerId, AUDIENCE},
    ZoneId,
};

/// Declare an operator zone and a join signer trusted to mint for it, the way
/// `db/migrations-ts/20260914000400_execution_zones_and_join_signers.ts` and
/// `20260914000500_worker_join_bindings.ts` model Control's own rows.
pub async fn declare_zone(platform: &platform::Platform) -> (ZoneId, String) {
    assert!(
        platform.is_fresh(),
        "an operator-declared execution zone is deployment-global: declaring it in the \
         process-shared database leaves every sibling case a second active zone, so an \
         app created without a name is refused. Give the case a fresh_database() clone."
    );
    let zone = ZoneId::mint();
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.execution_zones(id,name,status) VALUES($1,$2,'active')",
            &[&zone.as_str(), &zone.as_str()],
        )
        .await
        .unwrap();
    let signer = typed_id::generate("wjs");
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_join_signers(id,public_key,status) \
             VALUES($1,$2,'active')",
            &[
                &signer,
                &ServiceSigningKey::generate().verifying_key_bytes().to_vec(),
            ],
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_join_signer_zones(signer_id,execution_zone_id) \
             VALUES($1,$2)",
            &[&signer, &zone.as_str()],
        )
        .await
        .unwrap();
    (zone, signer)
}

/// A worker instance Control admitted into a zone, holding the key its
/// assertions are signed with. Enough to present an instance credential and
/// read back what verification resolved.
pub struct Enrolled {
    pub instance: WorkerId,
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
}

impl Enrolled {
    /// Insert an `active` instance row in `zone`, admitted under `signer`, and
    /// keep the generated key so the caller can present an assertion signed by
    /// the exact public half the registry will resolve.
    pub async fn join(platform: &platform::Platform, signer: &str, zone: &str) -> Self {
        let instance = WorkerId::mint();
        let key = ServiceSigningKey::generate();
        platform
            .admin
            .execute(
                "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
                 VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,$5,$6,now() + interval '1 hour')",
                &[
                    &instance.as_str(),
                    &vec![3_u8],
                    &key.verifying_key_bytes().to_vec(),
                    &signer,
                    &typed_id::generate("wjt"),
                    &zone,
                ],
            )
            .await
            .unwrap();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            instance.as_str()
        ))
        .unwrap();
        Self {
            instance,
            issuer,
            key,
        }
    }

    /// A bearer assertion signed by this instance's key, addressed to the
    /// workflow service.
    pub fn authorization(&self) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap()
        )
    }
}
