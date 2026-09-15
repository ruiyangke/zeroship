//! Boot-time enrolment and graceful retirement: how this process stops being
//! "a worker" and becomes "worker `wkr_…`", and how it stops being that again.
//!
//! # Two keyrings, and which is used where is the design
//!
//! The worker holds its deployment unit's ENROLLER key on disk: one Ed25519
//! key per host or pool, shared by every worker replica of that unit, and
//! recorded by Control with an id, an execution zone and a status. An
//! assertion minted under it names a UNIT rather than a process. Enrolment
//! exchanges it, once, for an identity that names one process:
//!
//! - The **enroller keyring** mints under `svc/worker-enroller/<wen_id>` and
//!   authenticates the ENROLMENT CALL and nothing else - `svc/worker-enroller`
//!   holds no other grant anywhere, and at that moment no instance exists, so
//!   there is nothing else it could be. This module takes it BY VALUE
//!   ([`enrol`]) and drops it, so "the enrolment call only" is a fact the
//!   compiler holds rather than a convention a later edit can quietly widen.
//! - The **instance keyring** is built on a keypair drawn at boot from the
//!   operating system's CSPRNG, in memory, NEVER ON DISK. It mints under
//!   `svc/worker/<wkr_id>` and is ADDRESSED AS `svc/worker`, because the gateway
//!   dispatches over a hash ring that holds the role name only. Everything after
//!   enrolment uses it, retirement included.
//!
//! No worker holds a `svc/worker` ROLE key, and none is needed: Control refuses
//! a role-arity `svc/worker` assertion outright, so every grant the worker role
//! carries is reachable only by an enrolled, active instance.
//!
//! Two keyrings rather than one relabelled keyring is FORCED.
//! `ServiceKeyring::from_parts` builds the minter from the issuer at
//! construction, so the issuer cannot change afterwards; and the enroller key
//! is SHARED by the unit, so minting instance assertions on it would make every
//! replica of the unit the same instance.
//!
//! # Per-instance identity is a DISTINGUISHER, the enroller is the boundary
//!
//! Enrolment authenticates with the UNIT's key, so whoever holds that key can
//! enrol as many instances IN THAT UNIT as they like, and every one of them is
//! as genuine as the last. What bounds that is the enroller itself: an operator
//! revoking it in Control retires every instance it enrolled and refuses every
//! enrolment after, so a revoked unit cannot come back under a fresh identity.
//! Retiring one instance is attribution and hygiene, not a boundary against a
//! process that still holds its unit's key.
//!
//! # Retirement
//!
//! A worker that shuts down GRACEFULLY calls [`retire`] once its server has
//! drained, declaring its own instance `gone`, so the instance key it is about
//! to discard stops authenticating immediately. It is the instance speaking for
//! itself, never an observation about it: a worker that crashes never calls
//! it, and nothing else writes it on the worker's behalf.
//!
//! # The `UserEnvelopeSigner` this creates on the instance key, and why it stays
//!
//! `ServiceKeyring` holds a [`zeroship_core::user_envelope::UserEnvelopeSigner`]
//! unconditionally, so the keyring built here has one on the boot-generated key.
//! It is INERT - the worker builds its verifier for the GATEWAY issuer alone, so
//! an envelope signed under any other key resolves to no key and is refused -
//! and `instance_signer_cannot_forge_an_identity_the_worker_accepts` in this
//! module's tests is what says so, for the instance key specifically.
//!
//! It is NOT withheld, and the reason is the one
//! `ServiceKeyring::user_envelope_signer` already records: the fence lives on
//! the VERIFYING side, where a test can bind it. Making the field optional would
//! move the fence to the minting side, where "this process was not handed a
//! signer" is invisible to every peer and provable by nothing - a fence that
//! looks like one and is not. The privilege-follows-the-process invariant is
//! satisfied the other way round: the capability grants no reach, because no
//! process anywhere trusts an envelope this key signed.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use zeroship_core::service_assertion::{
    ServiceIssuer, ServiceTrustBundle, TransportAssertionVerifier,
};
use zeroship_core::service_peers::{
    service_issuer, InstanceSigningKey, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME,
    WORKER_SERVICE_NAME,
};
use zeroship_core::user_envelope::UserEnvelopeVerifier;

/// How long one enrolment attempt may take.
const ENROLMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the retirement call may take.
///
/// Bounded tighter than enrolment because it runs inside the shutdown the
/// orchestrator is timing: a control plane that does not answer must not hold
/// a drained worker past the point where it gets killed anyway.
const RETIREMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to keep retrying while the control plane is UNREACHABLE.
///
/// Only that case retries, and the distinction is the whole of it: a control
/// plane that has not finished starting has not refused anything. `deploy/compose/
/// docker-compose.yml` orders the worker after `control: condition:
/// service_started`, and Kubernetes offers no ordering at all, so a single
/// attempt would turn ordinary boot ordering into a refusal. Every other
/// outcome - a 503 naming an undeclared envelope, a 401, a body that does not
/// parse - is terminal on the first answer, because retrying a deployment fault
/// only makes it a slow one.
const UNREACHABLE_BUDGET: Duration = Duration::from_secs(60);

/// The pause between attempts while control is unreachable.
const UNREACHABLE_RETRY_PAUSE: Duration = Duration::from_secs(2);

/// The operator's key material, read once, before anything else in the boot can
/// fail.
///
/// Held as one value rather than four locals so the boot path cannot end up
/// with a keyring and no verifier, or a verifier built from a DIFFERENT read of
/// the peer document than the keyring was checked against. There is exactly one
/// [`ServiceTrustBundle`] in this process and it is moved along the chain:
/// enroller keyring -> instance keyring -> inbound verifier.
#[allow(missing_debug_implementations)]
pub struct EnrollerMaterial {
    /// The unit's enroller keyring, minting under
    /// `svc/worker-enroller/<wen_id>`. Spent by [`enrol`] and unreachable
    /// afterwards.
    enroller: ServiceKeyring,
    /// The peer document.
    bundle: ServiceTrustBundle,
    /// The inbound `ZeroShip-User` verifier, built for the GATEWAY issuer.
    ///
    /// Built while the material is loaded rather than after enrolment, so fence
    /// F4's "the peer document must publish the gateway's key" refusal stays
    /// unconditional. Deferring it would make the one check the worker cannot
    /// serve a request without depend on the control plane being reachable.
    user_envelope: UserEnvelopeVerifier,
    /// `svc/worker`: the name callers address this process by, whatever it
    /// mints under.
    role_issuer: ServiceIssuer,
}

impl EnrollerMaterial {
    /// Assemble the material. The caller has already validated every part.
    #[must_use]
    pub const fn new(
        enroller: ServiceKeyring,
        bundle: ServiceTrustBundle,
        user_envelope: UserEnvelopeVerifier,
        role_issuer: ServiceIssuer,
    ) -> Self {
        Self {
            enroller,
            bundle,
            user_envelope,
            role_issuer,
        }
    }
}

/// Register this process with the control plane and return the identity it must
/// use from then on.
///
/// `listening_port` is the ONE thing this process contributes to its own
/// address. Control derives the host from the peer socket it observed and
/// refuses anything outside the operator's declared envelope; there is no host
/// field on the request and no way to add one.
///
/// # Errors
///
/// Returns a message naming what failed. EVERY failure is fatal to the boot -
/// see the caller in `main.rs`. A worker that carried on without an instance
/// identity would have nothing to verify dispatch with and nothing Control
/// would accept an app read from.
pub async fn enrol(
    material: EnrollerMaterial,
    control_url: &str,
    listening_port: u16,
) -> Result<ServiceAuth, String> {
    let EnrollerMaterial {
        enroller,
        bundle,
        user_envelope,
        role_issuer,
    } = material;

    // Drawn BEFORE the call, so the public half in the request body and the
    // private half this process signs with are two ends of one keypair by
    // construction rather than by matching two values later.
    let instance_key = InstanceSigningKey::generate();
    let request = serde_json::json!({
        "port": listening_port,
        "public_key": URL_SAFE_NO_PAD.encode(instance_key.public_key()),
    })
    .to_string();

    let control = service_issuer(CONTROL_SERVICE_NAME)
        .map_err(|error| format!("control service issuer is malformed: {error}"))?;
    let instance_id = ask_control(&enroller, &control, control_url, request).await?;
    // The ENROLLER KEYRING DIES HERE. It was moved in, it is not returned, and
    // nothing below can reach it - which is how "the unit's key authenticates
    // the enrolment call and nothing else" is enforced rather than asserted.
    drop(enroller);

    let instance_issuer = service_issuer(&format!("{WORKER_SERVICE_NAME}/{instance_id}"))
        .map_err(|error| {
            format!("control returned an instance id that is not an issuer segment: {error}")
        })?;
    // `into_keyring` REPLACES the own-key check `from_parts` would apply: it
    // refuses a boot-drawn key the bundle publishes AT ALL, under any issuer,
    // because the foreign-issuer question has no content against a key nobody
    // has ever seen. `addressed_as` then splits the name this process MINTS
    // under from the name its callers ADDRESS it by; without it every gateway
    // dispatch would fail `aud` equality against the ring's `svc/worker`.
    let mut keyring = instance_key
        .into_keyring(instance_issuer, bundle)
        .map_err(|error| format!("instance key material rejected: {error}"))?
        .addressed_as(role_issuer);
    let Some(bundle) = keyring.take_bundle() else {
        return Err("the instance keyring carried no peer bundle".to_string());
    };

    tracing::info!(
        instance_id = %instance_id,
        issuer = keyring.issuer().as_str(),
        audience = keyring.audience().as_str(),
        "worker: enrolled; every outbound assertion from here is minted under the instance issuer"
    );
    Ok(
        ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(bundle)))
            .verifying_user_envelopes(user_envelope),
    )
}

/// Declare this process's own instance `gone` at Control, once, on a graceful
/// exit.
///
/// Called only after the server has drained: an instance retires the moment
/// Control records it, and a request still in flight that read an app's
/// environment afterwards would be refused. The call carries no body and no
/// selector - Control retires the instance whose key signed it.
///
/// ONE attempt, bounded by [`RETIREMENT_TIMEOUT`]. There is no retry because a
/// retry cannot help: once the first attempt reached Control the key no longer
/// authenticates, and if it did not, the process is exiting regardless. A
/// retirement that did not land leaves the row `active` with no process behind
/// it - exactly what a crash leaves, and no worse.
///
/// # Errors
///
/// Returns a message naming what failed. The caller logs it and exits anyway.
pub async fn retire(auth: &ServiceAuth, control_url: &str) -> Result<(), String> {
    let control = service_issuer(CONTROL_SERVICE_NAME)
        .map_err(|error| format!("control service issuer is malformed: {error}"))?;
    let authorization = auth
        .authorization_for(&control)
        .ok_or_else(|| "this process holds no instance identity to retire".to_string())?;
    let url = format!("{control_url}/internal/workers/retire");
    let client = cyper::Client::new();
    let builder = client
        .post(&url)
        .map_err(|error| format!("invalid control URL {url}: {error}"))?
        .header("authorization", authorization)
        .map_err(|error| format!("invalid auth header: {error}"))?;
    let response = compio::time::timeout(RETIREMENT_TIMEOUT, builder.send())
        .await
        .map_err(|_| {
            format!(
                "control did not answer the retirement within {}s",
                RETIREMENT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("retirement transport: {error}"))?;
    let status = response.status().as_u16();
    if status == 204 {
        Ok(())
    } else {
        let body = response.bytes().await.unwrap_or_default();
        let snippet: String = String::from_utf8_lossy(&body).chars().take(400).collect();
        Err(format!("control refused the retirement: HTTP {status} {snippet}"))
    }
}

/// POST the enrolment and return the instance id control minted.
///
/// A fresh assertion per attempt: `jti` is single use, so a retried attempt
/// presenting the first one would be refused by control's replay store and the
/// retry would report a credential fault instead of the transport fault it was
/// retrying.
async fn ask_control(
    enroller: &ServiceKeyring,
    control: &ServiceIssuer,
    control_url: &str,
    request: String,
) -> Result<String, String> {
    let url = format!("{control_url}/internal/workers/enrol");
    let deadline = std::time::Instant::now() + UNREACHABLE_BUDGET;
    loop {
        let assertion = enroller
            .mint_for(control)
            .map_err(|error| format!("could not mint this worker's enrolment credential: {error}"))?;
        match post_enrolment(&url, &format!("Bearer {assertion}"), request.clone()).await {
            Ok(body) => return instance_id_from(&body),
            Err(EnrolmentFailure::Answered(message)) => return Err(message),
            Err(EnrolmentFailure::Unreachable(message)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "the control plane at {control_url} never answered the enrolment: {message}"
                    ));
                }
                tracing::warn!(
                    control_url = %control_url,
                    error = %message,
                    "worker: control plane unreachable for enrolment; retrying until the boot budget runs out"
                );
                compio::time::sleep(UNREACHABLE_RETRY_PAUSE).await;
            }
        }
    }
}

/// Which kind of failure an attempt hit.
///
/// The split exists so the retry can be narrow. Folding them together would
/// either retry a refusal - turning a deployment fault into a slow one - or
/// refuse a control plane that is merely still starting.
enum EnrolmentFailure {
    /// Nothing answered.
    Unreachable(String),
    /// Control answered, and the answer was not an admitted enrolment.
    Answered(String),
}

async fn post_enrolment(
    url: &str,
    authorization: &str,
    body: String,
) -> Result<String, EnrolmentFailure> {
    let client = cyper::Client::new();
    let builder = client
        .post(url)
        .map_err(|error| EnrolmentFailure::Answered(format!("invalid control URL {url}: {error}")))?
        .header("content-type", "application/json")
        .map_err(|error| {
            EnrolmentFailure::Answered(format!("invalid content-type header: {error}"))
        })?
        .header("authorization", authorization)
        .map_err(|error| EnrolmentFailure::Answered(format!("invalid auth header: {error}")))?;

    let response = compio::time::timeout(ENROLMENT_TIMEOUT, builder.body(body).send())
        .await
        .map_err(|_| {
            EnrolmentFailure::Unreachable(format!(
                "no answer within {}s",
                ENROLMENT_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| EnrolmentFailure::Unreachable(format!("transport: {error}")))?;

    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| EnrolmentFailure::Answered(format!("read body: {error}")))?;
    // The body carries a machine-readable `reason` and no credential, so it is
    // surfaced whole: an operator reading a refused boot needs to know whether
    // the envelope is undeclared, their port is outside it, or the enroller was
    // revoked.
    let snippet: String = String::from_utf8_lossy(&bytes).chars().take(400).collect();
    if status == 201 {
        Ok(snippet)
    } else {
        Err(EnrolmentFailure::Answered(format!(
            "control refused the enrolment: HTTP {status} {snippet}"
        )))
    }
}

fn instance_id_from(body: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("enrolment response is not JSON: {error}"))?;
    parsed
        .get("instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("enrolment response carried no instance_id: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::service_assertion::{thumbprint_key_id, ServiceSigningKey};
    use zeroship_core::service_peers::{worker_enroller_issuer, WORKER_ENROLLER_SERVICE_NAME};

    /// A bundle publishing the GATEWAY's key and nothing else - which is all a
    /// worker's peer document needs, and in particular no `svc/worker` key.
    fn peer_bundle(gateway: &ServiceSigningKey) -> ServiceTrustBundle {
        let mut bundle = ServiceTrustBundle::new();
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        bundle
            .trust_signing_key(&gateway_issuer, gateway.key_id(), gateway)
            .expect("trust the gateway key");
        bundle
    }

    /// Decode the claims of an `Authorization: Bearer <jwt>` header value.
    fn claims(header: &str) -> serde_json::Value {
        header
            .strip_prefix("Bearer ")
            .and_then(|assertion| assertion.split('.').nth(1))
            .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .expect("the minted assertion has a readable payload")
    }

    /// The enroller material this process would load for `enroller_id`, plus
    /// the GATEWAY keyring that dispatches to it - handed back rather than
    /// discarded, because the audience split is only checkable against a real
    /// caller.
    fn material(enroller_id: &str) -> (EnrollerMaterial, ServiceKeyring) {
        let gateway_key = ServiceSigningKey::generate();
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let bundle = peer_bundle(&gateway_key);
        let mut enroller = ServiceKeyring::from_parts(
            worker_enroller_issuer(enroller_id).expect("a minted enroller id is an issuer"),
            ServiceSigningKey::generate(),
            bundle,
        )
        .expect("the enroller keyring loads");
        let bundle = enroller.take_bundle().expect("the enroller keyring carries a bundle");
        let user_envelope =
            UserEnvelopeVerifier::for_issuer(&bundle, &gateway_issuer).expect("gateway verifier");
        let gateway =
            ServiceKeyring::from_parts(gateway_issuer, gateway_key, ServiceTrustBundle::new())
                .expect("the gateway keyring loads");
        (
            EnrollerMaterial::new(enroller, bundle, user_envelope, worker_issuer),
            gateway,
        )
    }

    /// The shape [`enrol`] builds, without the HTTP hop. Kept in step with
    /// `enrol` by construction: it is the same three calls in the same order.
    fn identity_for(instance_id: &str) -> (ServiceAuth, ServiceKeyring, [u8; 32]) {
        let (material, gateway) = material(&zeroship_core::typed_id::new_worker_enroller_id());
        let EnrollerMaterial {
            enroller,
            bundle,
            user_envelope,
            role_issuer,
        } = material;
        drop(enroller);
        let instance_key = InstanceSigningKey::generate();
        let public = *instance_key.public_key();
        let issuer = service_issuer(&format!("{WORKER_SERVICE_NAME}/{instance_id}"))
            .expect("instance issuer parses");
        let mut keyring = instance_key
            .into_keyring(issuer, bundle)
            .expect("a fresh key is in no bundle")
            .addressed_as(role_issuer);
        let bundle = keyring.take_bundle().expect("bundle");
        (
            ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(bundle)))
                .verifying_user_envelopes(user_envelope),
            gateway,
            public,
        )
    }

    /// The enrolment credential names the unit's ENROLLER, never `svc/worker`.
    ///
    /// This is the assertion [`ask_control`] presents, minted from the same
    /// keyring the boot path loads. Control grants `CONTROL_WORKER_ENROL` to
    /// `svc/worker-enroller` alone and resolves the key from the enroller row
    /// the instance segment names, so both halves of the issuer are
    /// load-bearing.
    #[test]
    fn the_enrolment_assertion_is_minted_under_the_enroller_issuer() {
        let enroller_id = zeroship_core::typed_id::new_worker_enroller_id();
        let (material, _gateway) = material(&enroller_id);
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let header = format!(
            "Bearer {}",
            material.enroller.mint_for(&control).expect("the enroller mints")
        );
        let claims = claims(&header);
        assert_eq!(
            claims["iss"],
            format!("spiffe://zeroship.ai/svc/worker-enroller/{enroller_id}"),
            "the enrolment must be attributable to exactly one enroller row"
        );
        assert_eq!(claims["aud"], "spiffe://zeroship.ai/svc/control");
        // The control, one variable apart: the role this process will ANSWER
        // to is still the worker role, which the enrolment never mints under.
        assert_eq!(
            material.role_issuer.as_str(),
            "spiffe://zeroship.ai/svc/worker"
        );
    }

    /// An enroller id the operator mistyped refuses the boot rather than
    /// enrolling under a unit Control has never heard of.
    #[test]
    fn an_enroller_id_that_is_not_a_wen_typed_id_is_refused() {
        for id in [
            "",
            "wen_",
            "wkr_0000000000000000000000001",
            "wen_short",
            "wen_0000000000000000000000001/extra",
        ] {
            assert!(
                worker_enroller_issuer(id).is_err(),
                "{id:?} must not become an enroller issuer"
            );
        }
        // The control: a minted id is accepted, so the refusals above are about
        // the shape rather than a function that refuses everything.
        let minted = zeroship_core::typed_id::new_worker_enroller_id();
        let issuer = worker_enroller_issuer(&minted).expect("a minted id is an enroller issuer");
        assert_eq!(issuer.instance(), Some(minted.as_str()));
        assert_eq!(
            issuer.principal(),
            service_issuer(WORKER_ENROLLER_SERVICE_NAME)
                .expect("enroller role issuer")
                .principal()
        );
    }

    #[test]
    fn the_instance_mints_under_its_own_name_and_is_addressed_by_the_role() {
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, _gateway, _public) = identity_for(&instance_id);
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let header = auth
            .authorization_for(&control)
            .expect("the instance identity mints");
        let claims = claims(&header);
        assert_eq!(
            claims["iss"], format!("spiffe://zeroship.ai/svc/worker/{instance_id}"),
            "outbound assertions must name the INSTANCE, or control cannot attribute them"
        );
        assert_eq!(
            claims["aud"], "spiffe://zeroship.ai/svc/control",
            "the audience is the callee, not this process"
        );
        // THE OTHER HALF, and the one that breaks every gateway dispatch if it
        // is wrong: the name callers must address this process by is still the
        // ROLE, because the hash ring holds only that.
        let rendered = format!("{auth:?}");
        assert!(
            rendered.contains("audience: Some(\"spiffe://zeroship.ai/svc/worker\")"),
            "the inbound audience must stay the role name: {rendered}"
        );
    }

    /// THE HALF THAT BREAKS EVERY REQUEST IF THE SPLIT IS WRONG: the gateway
    /// dispatches to `svc/worker`, and an instance must still accept that.
    ///
    /// The hash ring holds the ROLE name and nothing else, so every dispatch
    /// carries `aud: svc/worker`. If the instance keyring required its own
    /// minting name as the audience, the worker would enrol successfully and
    /// then refuse every end-user request - a failure that is invisible until
    /// traffic arrives, which is the shape this platform's boot fences exist to
    /// remove.
    #[compio::test]
    async fn a_gateway_dispatch_addressed_to_the_role_is_still_accepted() {
        use zeroship_core::service_identity::endpoints;

        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, gateway, _public) = identity_for(&instance_id);
        let role = service_issuer(WORKER_SERVICE_NAME).expect("role issuer");
        let addressed_to_the_role = format!(
            "Bearer {}",
            gateway.mint_for(&role).expect("the gateway mints")
        );
        assert!(
            auth.verify(Some(&addressed_to_the_role), endpoints::WORKER_DISPATCH)
                .await
                .is_ok(),
            "an instance must accept the dispatch its ring position was chosen for"
        );

        // THE ONE-VARIABLE CONTROL: the same caller, the same key, the same
        // endpoint, addressed to the INSTANCE name instead. It is refused, so
        // the acceptance above is `aud` equality against the role rather than
        // an audience check that is not running.
        let instance = service_issuer(&format!("{WORKER_SERVICE_NAME}/{instance_id}"))
            .expect("instance issuer");
        let addressed_to_the_instance = format!(
            "Bearer {}",
            gateway.mint_for(&instance).expect("the gateway mints")
        );
        assert!(
            auth.verify(Some(&addressed_to_the_instance), endpoints::WORKER_DISPATCH)
                .await
                .is_err(),
            "the minting name is not an address, and admitting it would widen `aud`"
        );
    }

    /// The instance keyring's `UserEnvelopeSigner` is INERT, measured on the
    /// instance key rather than inferred from another key's behaviour.
    ///
    /// This is what settles "a capability with no reader". It has no reach: the
    /// worker's verifier is built for the GATEWAY issuer alone, so an envelope
    /// signed under the boot-drawn key resolves to no key.
    #[test]
    fn instance_signer_cannot_forge_an_identity_the_worker_accepts() {
        const USER: &[u8] = br#"{"id":"pws_self","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
        let (auth, _gateway, _public) = identity_for("wkr_0000000000000000000000002");
        let request_id = uuid::Uuid::new_v4();
        let verifier = auth
            .user_envelope_verifier()
            .expect("the worker verifies envelopes");
        let forged = auth
            .user_envelope_signer()
            .expect("the keyring structurally carries one")
            .sign(USER, request_id);
        assert_eq!(
            verifier.verify_for_request(&forged, request_id),
            None,
            "an instance key must not be able to mint an identity this worker would accept"
        );
    }

    /// THE PAIRED CONTROL for the case above. Without it that test passes
    /// against a verifier that rejects everything, including the gateway.
    #[test]
    fn the_gateway_signature_over_the_same_bytes_is_accepted() {
        const USER: &[u8] = br#"{"id":"pws_real","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
        let gateway_key = ServiceSigningKey::generate();
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let bundle = peer_bundle(&gateway_key);
        let verifier =
            UserEnvelopeVerifier::for_issuer(&bundle, &gateway_issuer).expect("gateway verifier");
        let signer = ServiceKeyring::from_parts(gateway_issuer, gateway_key, bundle)
            .expect("the gateway keyring loads");
        let request_id = uuid::Uuid::new_v4();
        let genuine = signer.user_envelope_signer().sign(USER, request_id);
        assert!(
            verifier.verify_for_request(&genuine, request_id).is_some(),
            "the gateway's envelope must still be accepted"
        );
    }

    /// The boot-drawn private half must not be reachable from any formatter.
    ///
    /// `InstanceSigningKey` has a hand-written `Debug`; this rules on the whole
    /// chain the boot path actually formats, which is the identity the key ends
    /// up inside.
    #[test]
    fn no_formatter_on_the_boot_path_can_reach_the_private_half() {
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, _gateway, public) = identity_for(&instance_id);
        let rendered = format!("{auth:?}");
        // The PUBLIC half's thumbprint is a legitimate thing to print; the
        // private half is not, and the two are distinguishable only if the
        // control below holds.
        assert!(
            !rendered.contains(&URL_SAFE_NO_PAD.encode(public)),
            "the raw key bytes reached a formatter: {rendered}"
        );
        // The one-variable control: this is not passing because the formatter
        // prints nothing at all.
        assert!(
            rendered.contains(&format!("spiffe://zeroship.ai/svc/worker/{instance_id}")),
            "{rendered}"
        );
        let _ = thumbprint_key_id(&public);
    }

    #[test]
    fn an_enrolment_response_without_an_instance_id_is_refused() {
        assert_eq!(
            instance_id_from(r#"{"instance_id":"wkr_0000000000000000000000004"}"#),
            Ok("wkr_0000000000000000000000004".to_string())
        );
        for body in [
            r#"{"instance_id":""}"#,
            r#"{"instance_id":null}"#,
            r#"{"error":"nope"}"#,
            "not json",
        ] {
            assert!(
                instance_id_from(body).is_err(),
                "{body:?} must not yield an identity"
            );
        }
    }

    /// An id control could not have minted must not become an issuer.
    ///
    /// The response is the only value in this path that comes from off-process,
    /// and it lands in a security identifier. `ServiceIssuer::parse` is what
    /// refuses it - a segment carrying a slash would otherwise deepen the path
    /// past the instance arity, and one carrying a space would produce a name no
    /// verifier resolves.
    #[test]
    fn an_instance_id_that_is_not_a_path_segment_is_refused() {
        for id in ["", "a/b", "with space", "..", "wkr_x/y"] {
            assert!(
                service_issuer(&format!("{WORKER_SERVICE_NAME}/{id}")).is_err(),
                "{id:?} must not parse as an instance issuer"
            );
        }
        // The control: a real typed id does parse, so the refusals above are
        // about the shape rather than about the format string.
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let issuer = service_issuer(&format!("{WORKER_SERVICE_NAME}/{instance_id}"))
            .expect("a typed id parses");
        assert_eq!(issuer.instance(), Some(instance_id.as_str()));
        // The principal an instance identifier yields is the ROLE's, which is
        // what control's endpoint allowlist is written against.
        assert_eq!(
            issuer.principal(),
            service_issuer(WORKER_SERVICE_NAME)
                .expect("role issuer")
                .principal()
        );
    }

    /// Accept one connection, read one HTTP/1.1 request head from it, answer
    /// it with `status`, and hand back the head's lines.
    async fn answer_one_request(listener: compio::net::TcpListener, status: &str) -> Vec<String> {
        use compio::io::{AsyncRead as _, AsyncWriteExt as _};

        let (mut stream, _) = listener.accept().await.expect("accept the retirement");
        let mut head: Vec<u8> = Vec::new();
        let end = loop {
            if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
                break end;
            }
            let compio::BufResult(read, buf) = stream.read(vec![0_u8; 1024]).await;
            let read = read.expect("read the request head");
            assert!(read > 0, "the client closed before finishing its request head");
            head.extend_from_slice(&buf[..read]);
        };
        let response = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        stream
            .write_all(response.into_bytes())
            .await
            .0
            .expect("answer the retirement");
        String::from_utf8(head[..end].to_vec())
            .expect("an ASCII request head")
            .split("\r\n")
            .map(str::to_owned)
            .collect()
    }

    /// Retirement presents the INSTANCE's own assertion, to the retirement
    /// route, and names nothing else.
    ///
    /// Control retires the instance whose key verified the call, so the
    /// issuer carried here IS the selector: an assertion minted under the
    /// enroller, or under the bare role, would retire nothing or be refused.
    #[compio::test]
    async fn retirement_presents_the_instance_assertion_to_the_retirement_route() {
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, _gateway, _public) = identity_for(&instance_id);
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (head, outcome) = futures::join!(
            answer_one_request(listener, "204 No Content"),
            retire(&auth, &url)
        );
        assert_eq!(outcome, Ok(()), "a 204 is a recorded retirement");
        assert_eq!(head[0], "POST /internal/workers/retire HTTP/1.1");
        let authorization = head
            .iter()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .expect("the retirement carries an assertion");
        let claims = claims(&authorization);
        assert_eq!(
            claims["iss"],
            format!("spiffe://zeroship.ai/svc/worker/{instance_id}")
        );
        assert_eq!(claims["aud"], "spiffe://zeroship.ai/svc/control");
    }

    /// The paired control: a refusal is reported as one, so the success above
    /// is the status code being read rather than any answer counting.
    #[compio::test]
    async fn a_refused_retirement_is_reported() {
        let (auth, _gateway, _public) =
            identity_for(&zeroship_core::typed_id::new_worker_instance_id());
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (_head, outcome) = futures::join!(
            answer_one_request(listener, "401 Unauthorized"),
            retire(&auth, &url)
        );
        let message = outcome.expect_err("a 401 is not a retirement");
        assert!(message.contains("HTTP 401"), "{message}");
    }

    /// A process with no identity has nothing to retire and says so, rather
    /// than sending an unauthenticated request.
    #[compio::test]
    async fn an_unconfigured_process_does_not_attempt_a_retirement() {
        let outcome = retire(&ServiceAuth::unconfigured(), "http://127.0.0.1:1").await;
        assert!(outcome.is_err());
    }
}
