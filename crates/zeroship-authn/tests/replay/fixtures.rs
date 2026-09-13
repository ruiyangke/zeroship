use super::{AuthError, Client, Database, ReplayStore, SystemTime};
use std::sync::Arc;
use zeroship_core::service_assertion::{
    ClaimFuture, ReplayClaim, ReplayStoreError, ServiceAssertionMinter, ServiceAssertionVerifier,
    ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
};
use zeroship_core::service_identity::{verify_identity, PeerCredentials, ServiceIdentity};

pub const CALLER: &str = "spiffe://zeroship.ai/svc/gateway";
const CALLEE: &str = "spiffe://zeroship.ai/svc/control";
const KEY_ID: &str = "replay-fixture-signing-key";

pub struct Assertion {
    token: String,
    public_key: [u8; 32],
}

impl Assertion {
    pub fn new() -> Self {
        let signing = ServiceSigningKey::generate();
        let token = ServiceAssertionMinter::new(
            ServiceIssuer::parse(CALLER).unwrap(),
            KEY_ID.to_owned(),
            &signing,
        )
        .unwrap()
        .mint(&ServiceIssuer::parse(CALLEE).unwrap())
        .unwrap();
        Self {
            token,
            public_key: signing.verifying_key_bytes(),
        }
    }

    pub fn verifier(&self, store: Arc<dyn ReplayStore + Send + Sync>) -> ServiceAssertionVerifier {
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust(
                &ServiceIssuer::parse(CALLER).unwrap(),
                KEY_ID.to_owned(),
                self.public_key,
            )
            .unwrap();
        ServiceAssertionVerifier::new(bundle, store)
    }

    pub async fn verify(
        &self,
        verifier: &ServiceAssertionVerifier,
    ) -> Result<ServiceIdentity, AuthError> {
        let result = verify_identity(
            verifier,
            &PeerCredentials::new(Some(&self.token), None, CALLEE),
        )
        .await;
        if let Ok(identity) = &result {
            assert!(identity.matches_principal(ServiceIssuer::parse(CALLER).unwrap().principal()));
        }
        result
    }
}

/// Gate the claim statements in PostgreSQL while the clients verify the same assertion.
pub async fn race(
    database: &Database,
    store: impl Fn(Client) -> Arc<dyn ReplayStore + Send + Sync>,
) -> [Result<ServiceIdentity, AuthError>; 2] {
    let first = database.connect_as("zeroship_control").await;
    let second = database.connect_as("zeroship_control").await;
    let mut pids = Vec::new();
    for client in [&first, &second] {
        pids.push(
            client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
        );
    }
    let mut admin = database.connect().await;
    let held = admin.transaction().await.unwrap();
    // SELECT can finish under this lock; INSERT must wait. The broken store
    // therefore reads absence on both sessions before either can write.
    held.batch_execute("LOCK TABLE service_authn.service_assertion_replay IN SHARE MODE")
        .await
        .unwrap();
    let request = Assertion::new();
    let first = request.verifier(store(first));
    let second = request.verifier(store(second));
    let (left, right, blocked) =
        futures::join!(request.verify(&first), request.verify(&second), async {
            let blocked = database.wait_until_blocked(&pids).await;
            held.commit().await.unwrap();
            blocked
        },);
    assert!(
        blocked,
        "both claims must reach PostgreSQL before the fixture releases them"
    );
    [left, right]
}

/// A deliberately non-atomic store for the race's rejection control.
pub struct ReadThenWriteStore {
    pub client: Client,
}

impl ReplayStore for ReadThenWriteStore {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        Box::pin(async move {
            let seen = self.client.query(
                "SELECT 1 FROM service_authn.service_assertion_replay WHERE replay_key = $1 AND expires_at > now()",
                &[&key],
            ).await.map_err(|error| ReplayStoreError(error.to_string()))?;
            if !seen.is_empty() {
                return Ok(ReplayClaim::AlreadyUsed);
            }
            let seconds = expires_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            self.client
                .execute(
                    "INSERT INTO service_authn.service_assertion_replay (replay_key, expires_at) \
                 VALUES ($1, to_timestamp($2::double precision)) \
                 ON CONFLICT (replay_key) DO UPDATE SET expires_at = excluded.expires_at",
                    &[&key, &seconds],
                )
                .await
                .map_err(|error| ReplayStoreError(error.to_string()))?;
            Ok(ReplayClaim::Accepted)
        })
    }
}
