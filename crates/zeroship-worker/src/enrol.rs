//! Boot-time enrolment: how this process stops being "a worker" and becomes
//! "worker `wkr_…`".
//!
//! # Two keyrings, and which is used where is the design
//!
//! The worker holds the operator's `svc/worker` ROLE key on disk. That key is
//! shared by every worker replica, so an assertion minted under it names a
//! FLEET rather than a process. Enrolment exchanges it, once, for an identity
//! that names one process:
//!
//! - The **role keyring** authenticates the ENROLMENT CALL and nothing else. At
//!   that moment no instance exists, so there is nothing else it could be. This
//!   module takes it BY VALUE ([`enrol`]) and drops it, so "the enrolment call
//!   only" is a fact the compiler holds rather than a convention a later edit
//!   can quietly widen.
//! - The **instance keyring** is built on a keypair drawn at boot from the
//!   operating system's CSPRNG, in memory, NEVER ON DISK. It mints under
//!   `svc/worker/<wkr_id>` and is ADDRESSED AS `svc/worker`, because the gateway
//!   dispatches over a hash ring that holds the role name only. Everything after
//!   enrolment uses it.
//!
//! Two keyrings rather than one relabelled keyring is FORCED.
//! `ServiceKeyring::from_parts` builds the minter from the issuer at
//! construction, so the issuer cannot change afterwards; and keeping the role
//! key while renaming the issuer is refused by that same constructor as
//! `OwnKeyUnderForeignIssuer`, correctly - the peer bundle publishes that key
//! under `svc/worker`, so every holder of the bundle would accept an instance's
//! signature as the role's.
//!
//! # Per-instance identity is a DISTINGUISHER, not a boundary
//!
//! Enrolment authenticates with the SHARED role key, so whoever holds that key
//! can enrol as many instances as they like and every one of them is as genuine
//! as the last. Nothing here narrows what an instance may do. What it buys is
//! attribution, per-instance revocation (control's `status` filter on the row it
//! wrote), and a countable event.
//!
//! # The `UserEnvelopeSigner` this creates on the instance key, and why it stays
//!
//! `ServiceKeyring` holds a [`zeroship_core::user_envelope::UserEnvelopeSigner`]
//! unconditionally, so the keyring built here has one on the boot-generated key.
//! It is INERT - the worker builds its verifier for the GATEWAY issuer alone, so
//! an envelope signed under any other key resolves to no key and is refused -
//! and `instance_signer_cannot_forge_an_identity_the_worker_accepts` in this
//! module's tests is what says so, for the instance key specifically rather than
//! by analogy with the role key.
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
/// role keyring -> instance keyring -> inbound verifier.
#[allow(missing_debug_implementations)]
pub struct RoleMaterial {
    /// The `svc/worker` keyring. Spent by [`enrol`] and unreachable afterwards.
    role: ServiceKeyring,
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

impl RoleMaterial {
    /// Assemble the material. The caller has already validated every part.
    #[must_use]
    pub const fn new(
        role: ServiceKeyring,
        bundle: ServiceTrustBundle,
        user_envelope: UserEnvelopeVerifier,
        role_issuer: ServiceIssuer,
    ) -> Self {
        Self {
            role,
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
/// see the caller in `main.rs`. A worker that enrolled and then carried on
/// without the identity would mint under the shared role name while a row in
/// control's registry claimed a process that does not exist.
pub async fn enrol(
    material: RoleMaterial,
    control_url: &str,
    listening_port: u16,
) -> Result<ServiceAuth, String> {
    let RoleMaterial {
        role,
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
    let instance_id = ask_control(&role, &control, control_url, request).await?;
    // The ROLE KEYRING DIES HERE. It was moved in, it is not returned, and
    // nothing below can reach it - which is how "the role key authenticates the
    // enrolment call and nothing else" is enforced rather than asserted.
    drop(role);

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

/// POST the enrolment and return the instance id control minted.
///
/// A fresh assertion per attempt: `jti` is single use, so a retried attempt
/// presenting the first one would be refused by control's replay store and the
/// retry would report a credential fault instead of the transport fault it was
/// retrying.
async fn ask_control(
    role: &ServiceKeyring,
    control: &ServiceIssuer,
    control_url: &str,
    request: String,
) -> Result<String, String> {
    let url = format!("{control_url}/internal/workers/enrol");
    let deadline = std::time::Instant::now() + UNREACHABLE_BUDGET;
    loop {
        let assertion = role
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
    // the envelope is undeclared or their port is outside it.
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

    /// A bundle publishing one key for each named service, none of them the
    /// instance's.
    fn peer_bundle(gateway: &ServiceSigningKey, worker: &ServiceSigningKey) -> ServiceTrustBundle {
        let mut bundle = ServiceTrustBundle::new();
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        bundle
            .trust_signing_key(&gateway_issuer, gateway.key_id(), gateway)
            .expect("trust the gateway key");
        bundle
            .trust_signing_key(&worker_issuer, worker.key_id(), worker)
            .expect("trust the worker role key");
        bundle
    }

    /// The role material this process would load, plus the GATEWAY keyring that
    /// dispatches to it - handed back rather than discarded, because the
    /// audience split is only checkable against a real caller.
    fn material() -> (RoleMaterial, ServiceKeyring) {
        let gateway_key = ServiceSigningKey::generate();
        let worker_key = ServiceSigningKey::generate();
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let bundle = peer_bundle(&gateway_key, &worker_key);
        let mut role = ServiceKeyring::from_parts(worker_issuer.clone(), worker_key, bundle)
            .expect("the role keyring loads");
        let bundle = role.take_bundle().expect("the role keyring carries a bundle");
        let user_envelope =
            UserEnvelopeVerifier::for_issuer(&bundle, &gateway_issuer).expect("gateway verifier");
        let gateway =
            ServiceKeyring::from_parts(gateway_issuer, gateway_key, ServiceTrustBundle::new())
                .expect("the gateway keyring loads");
        (
            RoleMaterial::new(role, bundle, user_envelope, worker_issuer),
            gateway,
        )
    }

    /// The shape [`enrol`] builds, without the HTTP hop. Kept in step with
    /// `enrol` by construction: it is the same three calls in the same order.
    fn identity_for(instance_id: &str) -> (ServiceAuth, ServiceKeyring, [u8; 32]) {
        let (material, gateway) = material();
        let RoleMaterial {
            role,
            bundle,
            user_envelope,
            role_issuer,
        } = material;
        drop(role);
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

    #[test]
    fn the_instance_mints_under_its_own_name_and_is_addressed_by_the_role() {
        let (auth, _gateway, _public) = identity_for("wkr_0000000000000000000000001");
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let header = auth
            .authorization_for(&control)
            .expect("the instance identity mints");
        let claims = header
            .strip_prefix("Bearer ")
            .and_then(|assertion| assertion.split('.').nth(1))
            .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .expect("the minted assertion has a readable payload");
        assert_eq!(
            claims["iss"], "spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000001",
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

        let (auth, gateway, _public) = identity_for("wkr_0000000000000000000000006");
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
        let instance = service_issuer(&format!("{WORKER_SERVICE_NAME}/wkr_0000000000000000000006"))
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
    /// instance key rather than inferred from the role key's behaviour.
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
        let worker_key = ServiceSigningKey::generate();
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let bundle = peer_bundle(&gateway_key, &worker_key);
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
        let _ = worker_issuer;
    }

    /// The boot-drawn private half must not be reachable from any formatter.
    ///
    /// `InstanceSigningKey` has a hand-written `Debug`; this rules on the whole
    /// chain the boot path actually formats, which is the identity the key ends
    /// up inside.
    #[test]
    fn no_formatter_on_the_boot_path_can_reach_the_private_half() {
        let (auth, _gateway, public) = identity_for("wkr_0000000000000000000000003");
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
            rendered.contains("spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000003"),
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
        let issuer = service_issuer(&format!("{WORKER_SERVICE_NAME}/wkr_0000000000000000000005"))
            .expect("a typed id parses");
        assert_eq!(issuer.instance(), Some("wkr_0000000000000000000000005"));
        // The principal an instance identifier yields is the ROLE's, which is
        // what control's endpoint allowlist is written against.
        assert_eq!(
            issuer.principal(),
            service_issuer(WORKER_SERVICE_NAME)
                .expect("role issuer")
                .principal()
        );
    }
}
