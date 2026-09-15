//! Boot-time JOIN, lease renewal and graceful retirement: how this process
//! stops being "a worker" and becomes "worker `wkr_...`", how it stays that,
//! and how it stops being it again.
//!
//! # The worker holds no signing key on disk. It holds a TOKEN.
//!
//! A join token is a JWT a trusted signer minted, handed to this process
//! through a file. It is not a credential this process can mint anything with -
//! it cannot sign, it names a zone and a use budget rather than a principal,
//! and Control refuses it once its uses or its expiry run out. What makes it
//! usable only by the process that was handed it is the JOIN PROOF: the worker
//! draws an Ed25519 keypair at boot, IN MEMORY, NEVER ON DISK, and signs its own
//! join request with it, so presenting a token without holding that private half
//! registers nothing.
//!
//! The identity that results mints under `svc/worker/<wkr_id>` and is ADDRESSED
//! AS `svc/worker`, because the gateway dispatches over a hash ring that holds
//! the role name only. Everything after joining uses it: the app reads, the
//! lease renewals, and the retirement.
//!
//! No worker holds a `svc/worker` ROLE key, and none is needed: Control refuses
//! a role-arity `svc/worker` assertion outright, so every grant the worker role
//! carries is reachable only by a joined, live instance.
//!
//! # The instance identity EXPIRES, and this process renews it
//!
//! Control admits an instance with a lease and refuses an expired one exactly
//! as it refuses a retired one. That is what makes revocation stop being the
//! only way a credential ever dies: a worker that is killed, crashes, or is
//! simply forgotten stops authenticating on its own.
//!
//! [`renew_forever`] therefore runs for the life of the process, on an interval
//! DERIVED from the lease rather than chosen beside it
//! (`zeroship_core::worker_join::instance_renewal_interval`), so several
//! attempts fit inside one lease and a transient refusal costs nothing. No join
//! token is involved: possession was proved at join, and needing a fresh token
//! every few minutes would make a use-capped token useless.
//!
//! A renewal Control REFUSES is terminal - the identity lapsed or was
//! retired, and neither can be revived - so the loop stops rather than
//! hammering an endpoint that will never say yes again. A renewal that could
//! not be DELIVERED is not: the lease is still live and the next attempt is
//! inside the margin.
//!
//! # Retirement
//!
//! A worker that shuts down GRACEFULLY calls [`retire`] once its server has
//! drained, declaring its own instance `gone`, so the instance key it is about
//! to discard stops authenticating immediately. It is the instance speaking for
//! itself, never an observation about it: a worker that crashes never calls
//! it, and its lease is what closes the row instead.
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
    service_issuer, InstanceSigningKey, ServiceAuth, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME,
};
use zeroship_core::user_envelope::UserEnvelopeVerifier;
use zeroship_core::worker_join::{instance_renewal_interval, join_proof_message};

/// How long one join attempt may take.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the retirement call may take.
///
/// Bounded tighter than joining because it runs inside the shutdown the
/// orchestrator is timing: a control plane that does not answer must not hold
/// a drained worker past the point where it gets killed anyway.
const RETIREMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one renewal call may take.
///
/// Bounded well inside the renewal interval, so a control plane that hangs
/// costs one attempt rather than the whole margin the schedule leaves.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to keep retrying while the control plane is UNREACHABLE.
///
/// Only that case retries, and the distinction is the whole of it: a control
/// plane that has not finished starting has not refused anything. `deploy/compose/
/// docker-compose.yml` orders the worker after `control: condition:
/// service_started`, and Kubernetes offers no ordering at all, so a single
/// attempt would turn ordinary boot ordering into a refusal. Every other
/// outcome - a 503 naming an undeclared envelope, a 403 naming an exhausted
/// token, a body that does not parse - is terminal on the first answer, because
/// retrying a deployment fault only makes it a slow one.
const UNREACHABLE_BUDGET: Duration = Duration::from_secs(60);

/// The pause between attempts while control is unreachable.
const UNREACHABLE_RETRY_PAUSE: Duration = Duration::from_secs(2);

/// What a worker carries into the join, read once before anything else in the
/// boot can fail.
///
/// Held as one value rather than four locals so the boot path cannot end up
/// with a token and no verifier, or a verifier built from a DIFFERENT read of
/// the peer document than the join was checked against.
#[allow(missing_debug_implementations)]
pub struct JoinMaterial {
    /// The join token, as read from its file.
    ///
    /// A `String` rather than a parsed value: the JOIN PROOF is a signature
    /// over the EXACT token bytes, so this process must present what it read
    /// rather than a re-serialization of what it understood.
    token: String,
    /// The peer document.
    bundle: ServiceTrustBundle,
    /// The inbound `ZeroShip-User` verifier, built for the GATEWAY issuer.
    ///
    /// Built while the material is loaded rather than after joining, so fence
    /// F4's "the peer document must publish the gateway's key" refusal stays
    /// unconditional. Deferring it would make the one check the worker cannot
    /// serve a request without depend on the control plane being reachable.
    user_envelope: UserEnvelopeVerifier,
    /// `svc/worker`: the name callers address this process by, whatever it
    /// mints under.
    role_issuer: ServiceIssuer,
}

impl JoinMaterial {
    /// Assemble the material. The caller has already validated every part.
    #[must_use]
    pub const fn new(
        token: String,
        bundle: ServiceTrustBundle,
        user_envelope: UserEnvelopeVerifier,
        role_issuer: ServiceIssuer,
    ) -> Self {
        Self {
            token,
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
/// address, and even that is covered by the join proof. Control derives the
/// host from the peer socket it observed and refuses anything outside the
/// operator's declared envelope; there is no host field on the request and no
/// way to add one. There is no ZONE field either: the zone is the token's
/// claim.
///
/// # Errors
///
/// Returns a message naming what failed. EVERY failure is fatal to the boot -
/// see the caller in `main.rs`. A worker that carried on without an instance
/// identity would have nothing to verify dispatch with and nothing Control
/// would accept an app read from.
pub async fn join(
    material: JoinMaterial,
    control_url: &str,
    listening_port: u16,
) -> Result<ServiceAuth, String> {
    let JoinMaterial {
        token,
        bundle,
        user_envelope,
        role_issuer,
    } = material;

    // Drawn BEFORE the call, so the public half in the request body, the half
    // that signs the proof, and the half this process signs with afterwards are
    // three uses of one keypair by construction rather than by matching values
    // later.
    let instance_key = InstanceSigningKey::generate();
    let public = *instance_key.public_key();
    let proof = instance_key.sign_join_proof(&join_proof_message(&token, &public, listening_port));
    let request = serde_json::json!({
        "port": listening_port,
        "public_key": URL_SAFE_NO_PAD.encode(public),
        "proof": URL_SAFE_NO_PAD.encode(proof),
    })
    .to_string();

    let instance_id = ask_control(&token, control_url, request).await?;
    // THE TOKEN DIES HERE. It was moved in, it is not returned, and nothing
    // below can reach it - which is how "the token buys one join and nothing
    // else" is enforced rather than asserted. Renewal uses the instance
    // identity, so nothing later needs it.
    drop(token);

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
        "worker: joined; every outbound assertion from here is minted under the instance issuer"
    );
    Ok(
        ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(bundle)))
            .verifying_user_envelopes(user_envelope),
    )
}

/// Read the join token this worker was handed.
///
/// The file is the ONE thing a worker carries, and it is a bearer artifact: a
/// group- or world-readable one is refused here rather than used, because
/// whoever can read it can join a worker in that zone for as long as the token
/// lives.
///
/// # Errors
///
/// Returns a message naming the path when it is unset, unreadable, readable by
/// other local users, or does not hold something shaped like a token.
pub fn read_join_token(path: &std::path::Path) -> Result<String, String> {
    if path.as_os_str().is_empty() {
        return Err("worker.join_token_file is not set; a worker joins with a token".to_owned());
    }
    let raw = std::fs::read(path)
        .map_err(|error| format!("join token file {}: read: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .map_err(|error| format!("join token file {}: stat: {error}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "join token file {} is readable by other local users (mode {:o}); a join token \
                 is a bearer credential",
                path.display(),
                mode & 0o777
            ));
        }
    }
    let token = String::from_utf8(raw)
        .map_err(|_| format!("join token file {}: not UTF-8", path.display()))?
        .trim()
        .to_owned();
    // The shape check is deliberately thin: this process cannot verify the
    // token (it holds no signer key) and must not pretend to. What it CAN say
    // is that an empty file or a stray log line is not a JWT, which is the
    // difference between a boot that names the file and one that reports a
    // refusal from Control naming nothing.
    if token.split('.').count() != 3 || token.split('.').any(str::is_empty) {
        return Err(format!(
            "join token file {} does not hold a join token",
            path.display()
        ));
    }
    Ok(token)
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
/// retirement that did not land leaves the row `active` until its LEASE runs
/// out, which is the same place a crash leaves it, and no worse.
///
/// # Errors
///
/// Returns a message naming what failed. The caller logs it and exits anyway.
pub async fn retire(auth: &ServiceAuth, control_url: &str) -> Result<(), String> {
    match post_signed(auth, control_url, "/internal/workers/retire", RETIREMENT_TIMEOUT).await {
        Ok((204, _)) => Ok(()),
        Ok((status, body)) => Err(format!("control refused the retirement: HTTP {status} {body}")),
        Err(message) => Err(message),
    }
}

/// Extend this instance's lease once.
///
/// # Errors
///
/// Returns [`RenewalRefused`] when Control answered and the answer was not a
/// renewal - the identity lapsed or was retired, and neither can be revived -
/// and [`RenewalUnreachable`] when nothing answered.
async fn renew_once(auth: &ServiceAuth, control_url: &str) -> Result<(), RenewalFailure> {
    match post_signed(auth, control_url, "/internal/workers/renew", RENEWAL_TIMEOUT).await {
        Ok((200, _)) => Ok(()),
        Ok((status, body)) => Err(RenewalFailure::Refused(format!("HTTP {status} {body}"))),
        Err(message) => Err(RenewalFailure::Unreachable(message)),
    }
}

/// Why one renewal attempt did not extend the lease.
///
/// The split is what lets the loop stop on one and carry on through the other.
/// Folding them together would either give up on a control plane that is merely
/// restarting, or spin forever against an identity that can never be renewed.
#[derive(Debug)]
enum RenewalFailure {
    /// Control answered, and the answer was not a renewal. Terminal.
    Refused(String),
    /// Nothing answered. The lease is still live; try again.
    Unreachable(String),
}

/// Keep this instance's identity alive for as long as the process runs.
///
/// Returns when the identity can no longer be renewed, which is a fact about
/// this process rather than about the control plane: the row is retired or the
/// lease has already lapsed, and rejoining - which needs a token this process no
/// longer holds - is the only way back. The caller decides what to do about it;
/// this function does not exit the process on its own.
pub async fn renew_forever(auth: Arc<ServiceAuth>, control_url: String) {
    let interval = instance_renewal_interval();
    loop {
        compio::time::sleep(interval).await;
        match renew_once(&auth, &control_url).await {
            Ok(()) => tracing::debug!("worker: instance lease renewed"),
            Err(RenewalFailure::Unreachable(message)) => tracing::warn!(
                error = %message,
                "worker: could not reach control to renew the instance lease; the lease is \
                 still live and the next attempt is inside its margin"
            ),
            Err(RenewalFailure::Refused(message)) => {
                tracing::error!(
                    error = %message,
                    "worker: control refused to renew this instance; the identity has lapsed \
                     or been retired and cannot be revived"
                );
                return;
            }
        }
    }
}

/// POST to one of the instance-authenticated control routes, and report the
/// status and body.
///
/// ONE implementation for retirement and renewal, because they differ in the
/// path and in nothing else: both carry no body, no selector and one assertion
/// minted under the instance identity, and Control acts on the instance whose
/// key verified the call.
async fn post_signed(
    auth: &ServiceAuth,
    control_url: &str,
    path: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let control = service_issuer(CONTROL_SERVICE_NAME)
        .map_err(|error| format!("control service issuer is malformed: {error}"))?;
    let authorization = auth
        .authorization_for(&control)
        .ok_or_else(|| "this process holds no instance identity".to_string())?;
    let url = format!("{control_url}{path}");
    let client = cyper::Client::new();
    let builder = client
        .post(&url)
        .map_err(|error| format!("invalid control URL {url}: {error}"))?
        .header("authorization", authorization)
        .map_err(|error| format!("invalid auth header: {error}"))?;
    let response = compio::time::timeout(timeout, builder.send())
        .await
        .map_err(|_| format!("control did not answer {path} within {}s", timeout.as_secs()))?
        .map_err(|error| format!("{path} transport: {error}"))?;
    let status = response.status().as_u16();
    let body = response.bytes().await.unwrap_or_default();
    let snippet: String = String::from_utf8_lossy(&body).chars().take(400).collect();
    Ok((status, snippet))
}

/// POST the join and return the instance id control minted.
async fn ask_control(token: &str, control_url: &str, request: String) -> Result<String, String> {
    let url = format!("{control_url}/internal/workers/join");
    let authorization = format!("Bearer {token}");
    let deadline = std::time::Instant::now() + UNREACHABLE_BUDGET;
    loop {
        match post_join(&url, &authorization, request.clone()).await {
            Ok(body) => return instance_id_from(&body),
            Err(JoinFailure::Answered(message)) => return Err(message),
            Err(JoinFailure::Unreachable(message)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "the control plane at {control_url} never answered the join: {message}"
                    ));
                }
                tracing::warn!(
                    control_url = %control_url,
                    error = %message,
                    "worker: control plane unreachable for joining; retrying until the boot budget runs out"
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
enum JoinFailure {
    /// Nothing answered.
    Unreachable(String),
    /// Control answered, and the answer was not an admitted join.
    Answered(String),
}

async fn post_join(url: &str, authorization: &str, body: String) -> Result<String, JoinFailure> {
    let client = cyper::Client::new();
    let builder = client
        .post(url)
        .map_err(|error| JoinFailure::Answered(format!("invalid control URL {url}: {error}")))?
        .header("content-type", "application/json")
        .map_err(|error| JoinFailure::Answered(format!("invalid content-type header: {error}")))?
        .header("authorization", authorization)
        .map_err(|error| JoinFailure::Answered(format!("invalid auth header: {error}")))?;

    let response = compio::time::timeout(JOIN_TIMEOUT, builder.body(body).send())
        .await
        .map_err(|_| {
            JoinFailure::Unreachable(format!("no answer within {}s", JOIN_TIMEOUT.as_secs()))
        })?
        .map_err(|error| JoinFailure::Unreachable(format!("transport: {error}")))?;

    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| JoinFailure::Answered(format!("read body: {error}")))?;
    // The body carries a machine-readable `reason` and no credential, so it is
    // surfaced whole: an operator reading a refused boot needs to know whether
    // the envelope is undeclared, their port is outside it, the token is
    // exhausted, or the signer was revoked.
    let snippet: String = String::from_utf8_lossy(&bytes).chars().take(400).collect();
    if status == 201 {
        Ok(snippet)
    } else {
        Err(JoinFailure::Answered(format!(
            "control refused the join: HTTP {status} {snippet}"
        )))
    }
}

fn instance_id_from(body: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("join response is not JSON: {error}"))?;
    parsed
        .get("instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("join response carried no instance_id: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::service_assertion::{thumbprint_key_id, ServiceSigningKey};
    use zeroship_core::service_peers::ServiceKeyring;
    use zeroship_core::worker_join::{
        mint_join_token, verify_join_proof, JoinTokenGrant, DEFAULT_EXECUTION_ZONE,
    };

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

    /// One join token, minted the way an operator's signer would mint it.
    ///
    /// The worker never verifies it, so the key here is thrown away: what the
    /// tests below rule on is what this process DOES with a token, not whether
    /// it can judge one.
    fn a_join_token() -> String {
        let audience = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        mint_join_token(
            &zeroship_core::typed_id::new_join_signer_id(),
            &ServiceSigningKey::generate(),
            &audience,
            &JoinTokenGrant {
                zone: DEFAULT_EXECUTION_ZONE.to_owned(),
                lifetime: Duration::from_secs(300),
                uses: 4,
                confirm: None,
            },
        )
        .expect("the grant mints")
    }

    /// The material this process would load, plus the GATEWAY keyring that
    /// dispatches to it - handed back rather than discarded, because the
    /// audience split is only checkable against a real caller.
    fn material(token: String) -> (JoinMaterial, ServiceKeyring) {
        let gateway_key = ServiceSigningKey::generate();
        let worker_issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let gateway_issuer = service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME)
            .expect("gateway issuer");
        let bundle = peer_bundle(&gateway_key);
        let user_envelope =
            UserEnvelopeVerifier::for_issuer(&bundle, &gateway_issuer).expect("gateway verifier");
        let gateway =
            ServiceKeyring::from_parts(gateway_issuer, gateway_key, ServiceTrustBundle::new())
                .expect("the gateway keyring loads");
        (
            JoinMaterial::new(token, bundle, user_envelope, worker_issuer),
            gateway,
        )
    }

    /// The shape [`join`] builds, without the HTTP hop. Kept in step with
    /// `join` by construction: it is the same three calls in the same order.
    fn identity_for(instance_id: &str) -> (ServiceAuth, ServiceKeyring, [u8; 32]) {
        let (material, gateway) = material(a_join_token());
        let JoinMaterial {
            token,
            bundle,
            user_envelope,
            role_issuer,
        } = material;
        drop(token);
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

    /// Write `contents` to a fresh path under the temporary directory, at
    /// `mode`, and hand back the path. The caller removes it.
    fn token_file(contents: &str, mode: u32) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zeroship-join-token-{}",
            zeroship_core::typed_id::new_worker_instance_id()
        ));
        std::fs::write(&path, contents).expect("write the token file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("set the mode");
        }
        let _ = mode;
        path
    }

    /// THE CONTROL for every refusal below: a well-formed, owner-only token
    /// file is read and handed back verbatim.
    ///
    /// Verbatim is the load-bearing half. The JOIN PROOF signs the exact token
    /// bytes, so a reader that normalised, re-encoded or truncated what it read
    /// would produce a proof Control reconstructs differently and refuses.
    #[test]
    fn an_owner_only_token_file_is_read_verbatim() {
        let token = a_join_token();
        let path = token_file(&format!("{token}\n"), 0o600);
        assert_eq!(read_join_token(&path), Ok(token));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unset_token_path_refuses_rather_than_joining_without_one() {
        let refusal = read_join_token(std::path::Path::new("")).expect_err("unset must refuse");
        assert!(refusal.contains("join_token_file"), "{refusal}");
    }

    /// A join token is a BEARER credential: whoever reads the file can join a
    /// worker in that zone for as long as the token lives. A file other local
    /// users can read is refused rather than used.
    #[cfg(unix)]
    #[test]
    fn a_token_file_other_local_users_can_read_is_refused() {
        for mode in [0o640, 0o604, 0o644] {
            let path = token_file(&a_join_token(), mode);
            let refusal = read_join_token(&path)
                .expect_err(&format!("mode {mode:o} must be refused"));
            assert!(refusal.contains("bearer credential"), "{refusal}");
            std::fs::remove_file(&path).ok();
        }
        // THE PAIRED CONTROL, differing in the mode and nothing else: the same
        // token at 0600 is read, so the refusals above are the permission check
        // rather than a reader that refuses every file.
        let path = token_file(&a_join_token(), 0o600);
        assert!(read_join_token(&path).is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_file_that_does_not_hold_a_token_is_refused() {
        for contents in ["", "\n", "not-a-token", "a.b", "a.b.c.d", "a..c"] {
            let path = token_file(contents, 0o600);
            assert!(
                read_join_token(&path).is_err(),
                "{contents:?} must not be read as a join token"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn a_missing_token_file_is_refused() {
        let path = std::env::temp_dir().join(format!(
            "zeroship-absent-{}",
            zeroship_core::typed_id::new_worker_instance_id()
        ));
        assert!(read_join_token(&path).is_err());
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
    /// minting name as the audience, the worker would join successfully and
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
    fn a_join_response_without_an_instance_id_is_refused() {
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

    /// One HTTP/1.1 request as a stand-in control plane read it.
    struct Received {
        /// The request line and headers, one entry per line.
        head: Vec<String>,
        /// The body, as sent.
        body: String,
    }

    impl Received {
        /// The value of one header, case-insensitively.
        fn header(&self, name: &str) -> Option<String> {
            self.head.iter().find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim().to_owned())
            })
        }
    }

    /// Accept one connection, read one whole HTTP/1.1 request from it, answer
    /// with `status` and `body`, and hand back what was received.
    async fn answer_one_request(
        listener: compio::net::TcpListener,
        status: &str,
        body: &str,
    ) -> Received {
        use compio::io::{AsyncRead as _, AsyncWriteExt as _};

        let (mut stream, _) = listener.accept().await.expect("accept the request");
        let mut raw: Vec<u8> = Vec::new();
        let head_end = loop {
            if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break end;
            }
            let compio::BufResult(read, buf) = stream.read(vec![0_u8; 1024]).await;
            let read = read.expect("read the request head");
            assert!(read > 0, "the client closed before finishing its request head");
            raw.extend_from_slice(&buf[..read]);
        };
        let head: Vec<String> = String::from_utf8(raw[..head_end].to_vec())
            .expect("an ASCII request head")
            .split("\r\n")
            .map(str::to_owned)
            .collect();
        let wanted: usize = head
            .iter()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse().ok())
            })
            .unwrap_or(0);
        let mut payload = raw[head_end + 4..].to_vec();
        while payload.len() < wanted {
            let compio::BufResult(read, buf) = stream.read(vec![0_u8; 1024]).await;
            let read = read.expect("read the request body");
            assert!(read > 0, "the client closed before finishing its body");
            payload.extend_from_slice(&buf[..read]);
        }
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.into_bytes())
            .await
            .0
            .expect("answer the request");
        Received {
            head,
            body: String::from_utf8(payload).expect("a UTF-8 body"),
        }
    }

    /// THE PROOF OF POSSESSION, measured on the wire this process writes.
    ///
    /// A join carries the token as a bearer, the public half of a keypair drawn
    /// in this process, and a signature over the exact token, that key and the
    /// port. Verifying the proof HERE, with the same function Control verifies
    /// it with, is what says the request a captor could copy is useless without
    /// the private half - which never left this process and is not in the body.
    #[compio::test]
    async fn the_join_presents_the_token_and_a_proof_over_the_key_it_registers() {
        let token = a_join_token();
        let (material, _gateway) = material(token.clone());
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let admitted = format!(r#"{{"instance_id":"{instance_id}","lease_seconds":600}}"#);

        let (received, outcome) = futures::join!(
            answer_one_request(listener, "201 Created", &admitted),
            join(material, &url, 8085)
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(received.head[0], "POST /internal/workers/join HTTP/1.1");
        assert_eq!(
            received.header("authorization"),
            Some(format!("Bearer {token}")),
            "the token travels as the bearer, byte for byte as it was read"
        );

        let body: serde_json::Value =
            serde_json::from_str(&received.body).expect("the join body is JSON");
        assert_eq!(body["port"], 8085);
        // NO HOST AND NO ZONE. Control derives the host from the peer socket
        // and reads the zone off the token, so a field here would be a second
        // source for a fact that must have one.
        assert!(body.get("host").is_none(), "{body}");
        assert!(body.get("zone").is_none(), "{body}");

        let public: [u8; 32] = URL_SAFE_NO_PAD
            .decode(body["public_key"].as_str().expect("a public key"))
            .expect("base64url")
            .try_into()
            .expect("an ed25519 public key");
        let proof: [u8; 64] = URL_SAFE_NO_PAD
            .decode(body["proof"].as_str().expect("a proof"))
            .expect("base64url")
            .try_into()
            .expect("an ed25519 signature");
        assert!(
            verify_join_proof(&public, &token, 8085, &proof),
            "the request must be signed by the key it registers"
        );

        // THE THREE ONE-VARIABLE CONTROLS. Each changes exactly one of the
        // three things the proof binds, and each must refuse - otherwise the
        // acceptance above is a signature over less than it claims.
        assert!(
            !verify_join_proof(&public, &a_join_token(), 8085, &proof),
            "a proof must not verify against a different token"
        );
        assert!(
            !verify_join_proof(&public, &token, 8086, &proof),
            "a proof must not verify against a different port"
        );
        let other = InstanceSigningKey::generate();
        assert!(
            !verify_join_proof(other.public_key(), &token, 8085, &proof),
            "a proof must not verify against a key it was not made for"
        );
    }

    /// A refused join is fatal on the FIRST answer, and the refusal reason
    /// travels so an operator reading a worker's logs learns which check fired.
    #[compio::test]
    async fn a_refused_join_reports_control_s_reason_and_does_not_retry() {
        let (material, _gateway) = material(a_join_token());
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (_received, outcome) = futures::join!(
            answer_one_request(
                listener,
                "403 Forbidden",
                r#"{"error":"join refused","reason":"token_exhausted"}"#
            ),
            join(material, &url, 8085)
        );
        let message = outcome.expect_err("a 403 is not a join");
        assert!(message.contains("HTTP 403"), "{message}");
        assert!(message.contains("token_exhausted"), "{message}");
    }

    /// Retirement presents the INSTANCE's own assertion, to the retirement
    /// route, and names nothing else.
    ///
    /// Control retires the instance whose key verified the call, so the issuer
    /// carried here IS the selector: an assertion minted under the bare role
    /// would be refused for having no instance arity at all.
    #[compio::test]
    async fn retirement_presents_the_instance_assertion_to_the_retirement_route() {
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, _gateway, _public) = identity_for(&instance_id);
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (received, outcome) = futures::join!(
            answer_one_request(listener, "204 No Content", ""),
            retire(&auth, &url)
        );
        assert_eq!(outcome, Ok(()), "a 204 is a recorded retirement");
        assert_eq!(received.head[0], "POST /internal/workers/retire HTTP/1.1");
        let claims = claims(
            &received
                .header("authorization")
                .expect("the retirement carries an assertion"),
        );
        assert_eq!(
            claims["iss"],
            format!("spiffe://zeroship.ai/svc/worker/{instance_id}")
        );
        assert_eq!(claims["aud"], "spiffe://zeroship.ai/svc/control");
        // NO SELECTOR. The instance retired is the one whose key signed, so a
        // body naming another instance is not a thing this call can carry.
        assert!(received.body.is_empty(), "{:?}", received.body);
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

        let (_received, outcome) = futures::join!(
            answer_one_request(listener, "401 Unauthorized", ""),
            retire(&auth, &url)
        );
        let message = outcome.expect_err("a 401 is not a retirement");
        assert!(message.contains("HTTP 401"), "{message}");
    }

    /// A renewal carries the INSTANCE's assertion, to the renewal route, and
    /// NO JOIN TOKEN.
    ///
    /// The absent token is the point: possession of the instance key was proved
    /// at join, and demanding a fresh token every interval would burn a use per
    /// worker per interval and make a use-capped token useless.
    #[compio::test]
    async fn a_renewal_presents_the_instance_assertion_and_no_join_token() {
        let instance_id = zeroship_core::typed_id::new_worker_instance_id();
        let (auth, _gateway, _public) = identity_for(&instance_id);
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (received, outcome) = futures::join!(
            answer_one_request(listener, "200 OK", r#"{"lease_seconds":600}"#),
            renew_once(&auth, &url)
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(received.head[0], "POST /internal/workers/renew HTTP/1.1");
        let authorization = received
            .header("authorization")
            .expect("the renewal carries an assertion");
        let claims = claims(&authorization);
        assert_eq!(
            claims["iss"],
            format!("spiffe://zeroship.ai/svc/worker/{instance_id}"),
            "the instance renewed is the one whose key signed"
        );
        // The assertion is a service assertion, not a join token: its `typ` is
        // the service-assertion one, so Control's join verifier cannot be fed
        // this and its assertion verifier cannot be fed a token.
        assert_ne!(
            claims["typ"],
            serde_json::Value::from(zeroship_core::worker_join::JOIN_TOKEN_TYP)
        );
        assert!(received.body.is_empty(), "{:?}", received.body);
    }

    /// A REFUSED renewal is terminal and an UNREACHABLE one is not, and the
    /// split is what lets the loop stop on the first and carry on through the
    /// second.
    #[compio::test]
    async fn a_refused_renewal_is_terminal_and_an_unreachable_one_is_not() {
        let (auth, _gateway, _public) =
            identity_for(&zeroship_core::typed_id::new_worker_instance_id());
        let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a stand-in control plane");
        let url = format!("http://{}", listener.local_addr().expect("local address"));

        let (_received, outcome) = futures::join!(
            answer_one_request(
                listener,
                "403 Forbidden",
                r#"{"error":"renewal refused","reason":"instance_not_live"}"#
            ),
            renew_once(&auth, &url)
        );
        assert!(
            matches!(outcome, Err(RenewalFailure::Refused(_))),
            "an answered refusal must be terminal: {outcome:?}"
        );

        // THE ONE-VARIABLE CONTROL: the same call against an address nothing
        // is listening on. Folding the two together would either spin forever
        // against an identity that can never be renewed, or give up on a
        // control plane that is merely restarting.
        let nowhere = compio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind to learn a free port");
        let closed = format!("http://{}", nowhere.local_addr().expect("local address"));
        drop(nowhere);
        assert!(
            matches!(
                renew_once(&auth, &closed).await,
                Err(RenewalFailure::Unreachable(_))
            ),
            "nothing answered, so the lease is still live and the loop must carry on"
        );
    }

    /// A process with no identity has nothing to retire or renew and says so,
    /// rather than sending an unauthenticated request.
    #[compio::test]
    async fn an_unconfigured_process_does_not_attempt_a_retirement_or_a_renewal() {
        assert!(retire(&ServiceAuth::unconfigured(), "http://127.0.0.1:1")
            .await
            .is_err());
        assert!(renew_once(&ServiceAuth::unconfigured(), "http://127.0.0.1:1")
            .await
            .is_err());
    }
}
