//! Worker JOIN: the trusted-signer import, join-token verification, the row
//! Control writes, the instance lease, and the instance's own retirement.
//!
//! ONE ROW IS ONE LIVE WORKER PROCESS. The worker generates an Ed25519 instance
//! keypair at boot, in memory, never on disk, and presents the public half with
//! a JOIN TOKEN a trusted signer minted, plus a signature over the request made
//! by that very key. Control writes the row; the worker holds no privilege on
//! the table. The schema and the reasons for each of its columns are in
//! `db/migrations-ts/20260907000300_worker_instances.ts`,
//! `db/migrations-ts/20260914000400_execution_zones_and_join_signers.ts` and
//! `db/migrations-ts/20260914000500_worker_join_bindings.ts`.
//!
//! Control learns signers from ONE place: the operator's import file,
//! `control.join_signers_file`, read at startup by [`import_join_signers`]. The
//! import only ever ADDS: it inserts signers Control has not recorded, never
//! reactivates a revoked one, and refuses the whole file when any entry
//! disagrees with what is recorded - including a different set of permitted
//! zones, because a signer's zones ARE its authority and widening them is
//! provisioning a new signer. Revocation is the other direction and is not
//! configuration at all: `zeroship.rotate_worker_join_signer` stops future
//! tokens, `zeroship.purge_worker_join_signer` additionally retires everything
//! the signer admitted, and neither has a runtime EXECUTE grant. See
//! `docs/runbooks/worker-join-signers.md`.
//!
//! # The order of the checks, and why each is separate
//!
//! [`join`] runs them in exactly this order, each as its own statement with its
//! own refusal reason, so an operator reading a refused boot learns which one
//! fired and a mutation of any one of them fails exactly one test:
//!
//! 1. resolve the ACTIVE signer the token names, with its permitted zones;
//! 2. the token's SIGNATURE under that signer's recorded key;
//! 3. the AUDIENCE - this control plane and no other;
//! 4. EXPIRY, under the same skew bounds every service assertion uses;
//! 5. the token's `zone` is one this signer may mint for;
//! 6. one USE of that token id, consumed atomically;
//! 7. the request's SELF-SIGNATURE, which binds the presented public key, and
//!    equals `cnf` when the token carries one;
//! 8. the ADVERTISE ADDRESS, inside the operator's declared envelope.
//!
//! Only then is the instance minted. Steps 2 through 5 are
//! `zeroship_core::worker_join::verify_join_token`, which takes key material
//! rather than a registry; step 5's registry half and step 6 are here.
//!
//! WHAT THIS BUYS, STATED SO NOTHING HERE OVERSELLS IT. A captured token admits
//! workers the captor controls - up to the uses that remain, until its expiry,
//! in the one zone it names - and nothing more, because step 7 means a key
//! whose private half the presenter does not hold cannot be registered. The
//! instance identity that results is per-process and its private half exists
//! only in that process's memory, so retiring one instance IS a boundary here
//! rather than only attribution.
//!
//! # The two decisions this module exists to enforce
//!
//! **The ring key is control's, and the registrant contributes nothing to it.**
//! `HashRing::new` in `crates/zeroship-gateway/src/proxy.rs` derives ring
//! position from the worker URL. A registrant able to influence its own position
//! would GRIND its address until it landed beside a target app, and the
//! placement fence becomes a lottery the attacker plays until it wins.
//! [`mint_ring_key`] reads the OS CSPRNG and nothing else.
//!
//! **The address is derived from the join connection.** The worker contributes
//! only its listening port, and even that is covered by the join proof. Control
//! takes the host from the observed peer address and validates the pair against
//! [`EnrolmentEnvelope`]. There is deliberately NO fallback to a caller-supplied
//! host: that fallback is the vulnerability, not a convenience.
//! `collect_forwarded_headers` in
//! `crates/zeroship-gateway/src/router/dispatch.rs` strips a named header set
//! and COOKIE IS NOT IN IT, and `forward_dispatch` posts the full request -
//! body, cookies, and the gateway-signed user envelope - to whatever address the
//! ring returns. A registrant-supplied address would therefore let a token
//! holder intercept and impersonate end-user sessions under the app's own
//! origin, which is worse than the exposure the registry exists to reduce.
//!
//! Derivation is also what makes the design deployable: a per-process address
//! setting has no producer, because compose replicas share one environment block
//! and a Kubernetes Deployment is one pod spec for N pods, so every replica would
//! present the same address.
//!
//! # Joining IS idempotent, on the instance's public key
//!
//! [`join`] mints a candidate instance id and ring key up front, then spends
//! them inside `zeroship.join_worker_instance`
//! (`db/migrations-ts/20260914000500_worker_join_bindings.ts`), which conflicts
//! on `worker_instances.public_key`. Three consequences:
//!
//! 1. A worker restart is a NEW instance by construction - the keypair is
//!    generated at boot in memory, so there is nothing to deduplicate across
//!    restarts. A worker that exited gracefully has already retired its old
//!    row through [`retire`]; one that crashed leaves a row that stops being
//!    live when its LEASE runs out, which is what replaced the sweep this
//!    module used to say it did not have.
//! 2. A retried join - the response was lost, the worker asks again with the
//!    SAME instance key - returns the id of the row the first attempt (or a
//!    concurrent racing replica) already committed, rather than minting a
//!    second row for one process. The use accounting is idempotent on the same
//!    pair, so the retry costs no second use either.
//! 3. The SAME public key presented under a DIFFERENT signer or token is a
//!    conflict, refused rather than silently reassigned.
//!
//! # The signer row is locked, and the lock is what makes revocation exact
//!
//! `join_worker_instance` first takes a guarded no-op update lock on the named
//! signer's row, conditioned on `status = 'active'`. A concurrent rotate or
//! purge call's own first UPDATE targets that same row and queues behind this
//! lock, so revocation always observes every join that committed before it. A
//! signer found `revoked` at lock time refuses before any instance row is
//! written - the race a compromised signer's in-flight joins lose.
//!
//! # The rows are READ as well as written, and the read is where revocation lives
//!
//! [`active_instance_public_key`] and [`trusted_join_signer`] are their tables'
//! readers. Control resolves the first before verifying an assertion whose `iss`
//! names a worker instance (`crate::internal::resolve_instance_public_key`),
//! because no peer document has ever carried an instance key: the keypair is
//! drawn in memory at boot. The instance read filters on `status` AND on the
//! LEASE, and both halves are load-bearing - the status filter is what makes
//! retirement and purge mean anything, and the lease is what makes an abandoned
//! credential stop working with nobody acting.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::PUBLIC_KEY_LENGTH;
use ipnet::IpNet;
use ntex::web;
use rand::RngCore as _;
use serde::Deserialize;

use zeroship_core::worker_join::{
    parse_join_signer_import, verify_join_proof, verify_join_token, JoinSignerRecord,
    JoinTokenRefusal, VerifiedJoinToken, INSTANCE_LEASE_TTL,
};

use crate::{AppState, Registry};

/// Width of the ring key control mints, in bytes.
///
/// CONTROL'S DECISION, not a protocol constant, and deliberately not a CHECK in
/// the schema either - `worker_instances_ring_key_present` fences a row that
/// carries no key at all and says nothing about width, because the width is
/// whatever the minting side chooses. It is wide enough that the key space is
/// not the weak term in any placement argument.
pub const RING_KEY_BYTES: usize = 32;

/// The status control writes on an admitted join, and on an imported signer.
///
/// Of the instance column's other two members, `gone` has exactly two writers,
/// `zeroship.purge_worker_join_signer` (an operator's database operation) and
/// [`retire`] (the instance declaring its own exit), and `draining` has none.
const ENROLLED_STATUS: &str = "active";

/// The status an instance declares when it retires itself. Terminal: a `gone`
/// row never authenticates again, and nothing moves it back.
const RETIRED_STATUS: &str = "gone";

/// The status the two signer verbs write. Terminal.
const REVOKED_STATUS: &str = "revoked";

/// The status of an execution zone this deployment declares, the only member
/// `execution_zones_status_check` admits.
const DECLARED_ZONE_STATUS: &str = "active";

/// Mint a ring key from the operating system's CSPRNG.
///
/// The registrant contributes NOTHING to this value - not a seed, not a nonce,
/// not a length. See the module header for why that is load-bearing rather than
/// tidy.
#[must_use]
pub fn mint_ring_key() -> [u8; RING_KEY_BYTES] {
    let mut key = [0_u8; RING_KEY_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// Why an enrolment was refused.
///
/// Every variant refuses. They differ in what an operator should DO, which is
/// why the two deployment-configuration states are not folded into the four
/// address verdicts: one is "this control plane cannot enrol anyone", the other
/// is "this caller may not enrol from there".
///
/// The reason travels to the caller. That leaks nothing a probe could not
/// already learn from success-versus-failure, and by the time any of these are
/// reachable the caller has already presented a token a trusted signer minted
/// and proved possession of the key it is registering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrolmentRefusal {
    /// No envelope was declared. ABSENCE REFUSES; it does not default open.
    EnvelopeUnset,
    /// This control plane is fronted by a trusted proxy, so every peer address
    /// it observes is the proxy's and the derivation would collapse to
    /// "everything is the proxy".
    ProxyFronted,
    /// The transport exposed no peer address. There is no fallback to a
    /// caller-supplied host, by design.
    PeerAddressUnobservable,
    /// The peer is the unspecified address.
    PeerIsUnspecified,
    /// The peer address is outside every declared network.
    PeerOutsideEnvelope,
    /// The claimed listening port is outside the declared range.
    PortOutsideEnvelope,
}

impl EnrolmentRefusal {
    /// The stable machine-readable reason, carried in the response body.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnvelopeUnset => "envelope_unset",
            Self::ProxyFronted => "proxy_fronted",
            Self::PeerAddressUnobservable => "peer_address_unobservable",
            Self::PeerIsUnspecified => "peer_is_unspecified",
            Self::PeerOutsideEnvelope => "peer_outside_envelope",
            Self::PortOutsideEnvelope => "port_outside_envelope",
        }
    }

    /// Whether this refusal is about the DEPLOYMENT rather than the caller.
    ///
    /// The two that are answer 503, so an operator reading a worker's logs sees
    /// "this control plane is not configured to enrol" rather than a credential
    /// verdict they would go and rotate something over.
    #[must_use]
    pub const fn is_deployment_state(self) -> bool {
        matches!(self, Self::EnvelopeUnset | Self::ProxyFronted)
    }
}

/// The operator-declared bound on where a worker instance may enrol FROM, and
/// on what port it may claim to be listening.
///
/// Held in CONTROL'S OWN config. It is not negotiable by the registrant and
/// nothing on the wire can widen it.
///
/// # An unset envelope refuses
///
/// [`Self::closed`] - and a declaration whose network list or port range is
/// empty - admits nothing. That follows the inversion this platform already
/// took for service identity: a process with no key material serves no guarded
/// edge, rather than skipping the check (`ServiceAuth::unconfigured` in
/// `crates/zeroship-core/src/service_peers.rs`, and the rustdoc on
/// `AppState::service_auth`). Absence refuses; it never disables.
///
/// It refuses AT THE REQUEST, not at boot, and that is the same precedent
/// rather than a softening of it. `service_key_file` refuses the boot because a
/// control plane without it can serve no guarded edge at all; an undeclared
/// enrolment envelope disables exactly one route. Refusing the boot for it
/// would take down every deployment that has no workers to enrol yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrolmentEnvelope {
    networks: Vec<IpNet>,
    ports: Option<RangeInclusive<u16>>,
    proxy_fronted: bool,
}

impl EnrolmentEnvelope {
    /// The envelope that admits nothing. What an undeclared deployment gets.
    #[must_use]
    pub const fn closed() -> Self {
        Self {
            networks: Vec::new(),
            ports: None,
            proxy_fronted: false,
        }
    }

    /// Parse the operator's declaration.
    ///
    /// `networks` is a comma-separated CIDR list; `ports` is `<low>-<high>` or a
    /// single port. Either being empty yields an envelope that admits nothing.
    ///
    /// `proxy_fronted` is control's own `trust_proxy`. It is folded in here so
    /// [`Self::derive_address`] has ONE policy input and no call site can
    /// forget the check.
    ///
    /// # Errors
    ///
    /// Returns a message naming the offending token when a network is not a
    /// CIDR, when a network is a default route, or when the port range is
    /// malformed, empty or contains port zero.
    pub fn parse(networks: &str, ports: &str, proxy_fronted: bool) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for token in networks.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let net: IpNet = token
                .parse()
                .map_err(|e| format!("worker enrolment network {token:?} is not a CIDR: {e}"))?;
            // A default route is not an envelope, it is the absence of one
            // spelled in a way that reads as a declaration. Refuse it here so
            // the setting cannot be turned off by widening it - the same
            // reasoning `MIN_ACCEPT_PREFIX_V4` records in
            // `crates/zeroship-core/src/net_policy.rs`, narrowed to the one
            // prefix that means "everything".
            if net.prefix_len() == 0 {
                return Err(format!(
                    "worker enrolment network {token:?} is a default route, which declares no bound"
                ));
            }
            // Canonicalise for the same reason the egress destination does:
            // `10.0.0.1/24` and `10.0.0.0/24` are one range and should read as
            // one range.
            parsed.push(net.trunc());
        }

        let ports = Self::parse_ports(ports)?;
        Ok(Self {
            networks: parsed,
            ports,
            proxy_fronted,
        })
    }

    fn parse_ports(raw: &str) -> Result<Option<RangeInclusive<u16>>, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let (low_text, high_text) = match trimmed.split_once('-') {
            Some((low, high)) => (low.trim(), high.trim()),
            None => (trimmed, trimmed),
        };
        let low: u16 = low_text
            .parse()
            .map_err(|e| format!("worker enrolment port {low_text:?} is not a port: {e}"))?;
        let high: u16 = high_text
            .parse()
            .map_err(|e| format!("worker enrolment port {high_text:?} is not a port: {e}"))?;
        if low == 0 {
            return Err("worker enrolment port range must not include port 0".to_string());
        }
        if high < low {
            return Err(format!(
                "worker enrolment port range {trimmed:?} ends below where it starts"
            ));
        }
        Ok(Some(low..=high))
    }

    /// Whether the operator declared an envelope at all.
    #[must_use]
    pub const fn is_declared(&self) -> bool {
        !self.networks.is_empty() && self.ports.is_some()
    }

    /// Derive the address to advertise for this enrolment, or refuse.
    ///
    /// `peer` is the address the TRANSPORT observed. The caller must pass what
    /// the connection reported and nothing else - passing a header-derived or
    /// body-derived value here reintroduces exactly the defect this function
    /// exists to prevent. `claimed_port` is the one thing the registrant
    /// contributes.
    ///
    /// An IPv4-mapped IPv6 peer is canonicalised BEFORE the network comparison,
    /// in both directions. A dual-stack listener reports `::ffff:10.7.3.9` for a
    /// v4 client, which `contains` would not find in a v4 network, and the
    /// derived host must be the v4 spelling or the `inet` column and every URL
    /// built from it carry a mapped form of an address the operator declared in
    /// v4.
    ///
    /// # Loopback is ruled on by the declared networks, not by a fence above them
    ///
    /// There is no standalone loopback refusal, and there was one until
    /// 2026-09-07. It sat ABOVE the network comparison, so declaring
    /// `127.0.0.0/8` could not admit a loopback peer and no single-host
    /// deployment could enrol - which is every developer machine and every
    /// harness in `tests/` that launches a worker. A fence whose declared input
    /// cannot express a configuration the operator states outright is a defect,
    /// not a policy.
    ///
    /// Admitting a declared loopback costs nothing the derivation was protecting.
    /// The address is still OBSERVED rather than claimed, so a registrant cannot
    /// choose it; ring position is minted by control and never derived from the
    /// address, so there is nothing to grind toward; and a same-host caller can
    /// already read whatever join token the deployment put on that host, so it
    /// gains no reach it lacked.
    /// The collapse case - every peer looking identical because a proxy sits in
    /// front - is a DIFFERENT arm, `ProxyFronted`, which stays unconditional and
    /// stays first.
    ///
    /// # Errors
    ///
    /// Returns the [`EnrolmentRefusal`] that fired. The order of checks is
    /// deployment state, then observability, then the peer, then the port, so
    /// the reason names the outermost thing that is wrong.
    pub fn derive_address(
        &self,
        peer: Option<SocketAddr>,
        claimed_port: u16,
    ) -> Result<SocketAddr, EnrolmentRefusal> {
        if !self.is_declared() {
            return Err(EnrolmentRefusal::EnvelopeUnset);
        }
        if self.proxy_fronted {
            return Err(EnrolmentRefusal::ProxyFronted);
        }
        let observed = peer.ok_or(EnrolmentRefusal::PeerAddressUnobservable)?;
        let host = observed.ip().to_canonical();
        if host.is_unspecified() {
            return Err(EnrolmentRefusal::PeerIsUnspecified);
        }
        if !self.networks.iter().any(|net| net.contains(&host)) {
            return Err(EnrolmentRefusal::PeerOutsideEnvelope);
        }
        let ports = self.ports.as_ref().ok_or(EnrolmentRefusal::EnvelopeUnset)?;
        if !ports.contains(&claimed_port) {
            return Err(EnrolmentRefusal::PortOutsideEnvelope);
        }
        Ok(SocketAddr::new(host, claimed_port))
    }
}

/// What a worker sends.
///
/// Its listening PORT, its instance PUBLIC KEY, and a PROOF that it holds the
/// private half. There is no host field on purpose, and adding one is the
/// vulnerability the module header describes. There is no ZONE field either,
/// and that is the same kind of absence: the zone is the token's claim, so a
/// field here would be a second source for a fact that must have one.
#[derive(Debug, Deserialize)]
pub struct WorkerJoinRequest {
    /// The port the worker is listening on.
    pub port: u16,
    /// The raw Ed25519 public key, base64url without padding.
    pub public_key: String,
    /// The detached Ed25519 signature over
    /// `zeroship_core::worker_join::join_proof_message`, base64url without
    /// padding, made with the private half of `public_key`.
    pub proof: String,
}

/// What control returns on an admitted join.
#[derive(Debug, serde::Serialize)]
pub struct WorkerJoinAccepted {
    /// The minted `wkr_` instance id. The worker mints under
    /// `svc/worker/<instance_id>` and is still ADDRESSED as `svc/worker`.
    pub instance_id: String,
    /// Seconds until this identity lapses unless renewed. The worker derives
    /// its renewal schedule from the shared constants rather than from this
    /// value; it is reported so an operator reading a boot log sees the lease
    /// the control plane actually granted.
    pub lease_seconds: u64,
}

/// What control returns on a renewal.
#[derive(Debug, serde::Serialize)]
pub struct WorkerLeaseRenewed {
    /// Seconds until the extended identity lapses.
    pub lease_seconds: u64,
}

/// Serve one join.
///
/// Split from the ntex handler on exactly one seam: `peer` is passed in rather
/// than read here. That is not a testability concession dressed up as design -
/// the handler's whole job on this axis is `req.peer_addr()`, and a seam that
/// takes `Option<SocketAddr>` cannot be handed a header. ntex's
/// `TestRequest::peer_addr` is dropped by `to_request` (its own unit test in
/// `web/test.rs` asserts `req.peer_addr() == None`), so an in-process handler
/// test can only ever exercise the unobservable-peer arm; the accepted arms are
/// driven here, and the handler's real read of the transport is bound by a
/// live-server arm that gets a genuine loopback peer and is refused BY NAME as
/// outside the declared envelope rather than for having no peer.
///
/// `token` is the raw bearer this request presented. It is passed rather than
/// re-read for the same reason: the proof is signed over the EXACT bytes, so
/// there must be one reading of them.
pub async fn join(
    state: &AppState,
    peer: Option<SocketAddr>,
    token: &str,
    request: WorkerJoinRequest,
) -> web::HttpResponse {
    let public_key = match decode_instance_public_key(&request.public_key) {
        Ok(key) => key,
        Err(message) => {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({"error": message}));
        }
    };
    let proof = match decode_join_proof(&request.proof) {
        Ok(proof) => proof,
        Err(message) => {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({"error": message}));
        }
    };

    // 1. The signer. Resolved from the id the UNVERIFIED token claims, which
    //    selects a key and grants nothing: the signature below still has to
    //    hold under it.
    let Some(signer_id) = zeroship_core::worker_join::unverified_join_signer_id(token) else {
        return join_refused(JoinTokenRefusal::SignerMalformed.as_str());
    };
    let signer = match trusted_join_signer(&state.control_pg, &signer_id).await {
        Ok(Some(signer)) => signer,
        Ok(None) => {
            tracing::warn!(
                signer_id,
                "control-internal: join refused - no ACTIVE signer is recorded under this id"
            );
            return join_refused("signer_unknown");
        }
        Err(error) => {
            tracing::error!(%error, signer_id, "control-internal: the signer registry could not be read");
            return web::HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "service unavailable"}));
        }
    };

    // 2-4. Signature, audience, expiry. One call, but each of them is its own
    //      statement inside it with its own reason.
    let audience = match crate::internal::control_service_issuer() {
        Ok(issuer) => issuer,
        Err(error) => {
            tracing::error!(%error, "control-internal: this control plane's own issuer is malformed");
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "internal error"}));
        }
    };
    let verified = match verify_join_token(
        token,
        &signer.public_key,
        &audience,
        std::time::SystemTime::now(),
    ) {
        Ok(verified) => verified,
        Err(refusal) => {
            tracing::warn!(
                signer_id,
                reason = refusal.as_str(),
                "control-internal: join refused - the token did not verify"
            );
            return join_refused(refusal.as_str());
        }
    };

    // 5. The zone must be one this signer may mint for. Checked here against
    //    the recorded set, and AGAIN inside the insert under the signer's row
    //    lock; the two refusals carry different reasons so neither can hide
    //    the other's absence.
    let Some(zone_id) = signer.zones.get(&verified.zone).cloned() else {
        tracing::warn!(
            signer_id,
            zone = verified.zone.as_str(),
            "control-internal: join refused - the signer may not mint for this zone"
        );
        return join_refused("zone_not_permitted");
    };

    // 6. One use of this token, consumed atomically. A repeat presentation of
    //    the same joining key is the lost-reply retry and costs nothing.
    let joining_key = zeroship_core::service_assertion::thumbprint_key_id(&public_key);
    match consume_join_use(&state.control_pg, &verified, &joining_key).await {
        Ok(()) => {}
        Err(UseFailure::Exhausted) => {
            tracing::warn!(
                signer_id,
                token_id = verified.token_id.as_str(),
                "control-internal: join refused - the token has no uses left"
            );
            return join_refused("token_exhausted");
        }
        Err(UseFailure::Database(error)) => {
            tracing::error!(%error, signer_id, "control-internal: join use accounting failed");
            return web::HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "service unavailable"}));
        }
    }

    // 7. Proof of possession. Without it a token is a bearer credential over
    //    ANY public key, so a captor could register a key it does not hold and
    //    make Control attribute a worker to somebody else's material.
    if !verify_join_proof(&public_key, token, request.port, &proof) {
        tracing::warn!(
            signer_id,
            token_id = verified.token_id.as_str(),
            "control-internal: join refused - the request was not signed by the presented key"
        );
        return join_refused("proof_invalid");
    }
    if let Some(expected) = verified.confirmation.as_deref() {
        if expected != joining_key {
            tracing::warn!(
                signer_id,
                token_id = verified.token_id.as_str(),
                "control-internal: join refused - the presented key is not the confirmed one"
            );
            return join_refused("confirmation_mismatch");
        }
    }

    // 8. The address, derived from the connection and held to the envelope.
    let address = match state.worker_enrolment.derive_address(peer, request.port) {
        Ok(address) => address,
        Err(refusal) => {
            tracing::warn!(
                peer = ?peer,
                claimed_port = request.port,
                reason = refusal.as_str(),
                "control-internal: worker join refused"
            );
            let body = serde_json::json!({
                "error": "join refused",
                "reason": refusal.as_str(),
            });
            return if refusal.is_deployment_state() {
                web::HttpResponse::ServiceUnavailable().json(&body)
            } else {
                web::HttpResponse::Forbidden().json(&body)
            };
        }
    };

    match join_instance(&state.control_pg, &verified, &zone_id, address, &public_key).await {
        Ok(instance_id) => {
            tracing::info!(
                signer_id,
                token_id = verified.token_id.as_str(),
                zone = verified.zone.as_str(),
                instance_id = %instance_id,
                advertise_host = %address.ip(),
                advertise_port = address.port(),
                "control-internal: worker instance joined"
            );
            web::HttpResponse::Created().json(&WorkerJoinAccepted {
                instance_id,
                lease_seconds: INSTANCE_LEASE_TTL.as_secs(),
            })
        }
        Err(JoinFailure::SignerInactive) => {
            tracing::warn!(
                signer_id,
                "control-internal: join refused - the signer stopped being active mid-join"
            );
            join_refused("signer_inactive")
        }
        Err(JoinFailure::ZoneNotPermitted) => {
            tracing::warn!(
                signer_id,
                zone = verified.zone.as_str(),
                "control-internal: join refused by the registry - signer may not mint for this zone"
            );
            join_refused("zone_not_permitted_by_registry")
        }
        Err(JoinFailure::PublicKeyConflict) => {
            tracing::warn!(
                signer_id,
                "control-internal: join refused - public key joined under another signer or token"
            );
            web::HttpResponse::Conflict().json(&serde_json::json!({
                "error": "join refused",
                "reason": "public_key_conflict",
            }))
        }
        Err(JoinFailure::Database(error)) => {
            tracing::error!(
                signer_id,
                error = %error,
                advertise_host = %address.ip(),
                advertise_port = address.port(),
                "control-internal: worker join insert failed"
            );
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "internal error"}))
        }
    }
}

/// The one 403 body shape every join refusal uses.
///
/// The reason travels to the caller. That leaks nothing a probe could not
/// already learn from success-versus-failure, and an operator reading a refused
/// worker boot has no other way to tell an unknown signer from an exhausted
/// token from a zone the signer may not mint for.
fn join_refused(reason: &str) -> web::HttpResponse {
    web::HttpResponse::Forbidden().json(&serde_json::json!({
        "error": "join refused",
        "reason": reason,
    }))
}

/// Decode and width-check the instance public key.
///
/// The width comes from `ed25519_dalek::PUBLIC_KEY_LENGTH` rather than a literal
/// here, and the database repeats it as `worker_instances_public_key_shape`.
/// Two independent statements of one protocol fact, neither of which is a number
/// this file invented.
fn decode_instance_public_key(encoded: &str) -> Result<[u8; PUBLIC_KEY_LENGTH], &'static str> {
    let raw = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|_| "public_key is not base64url")?;
    <[u8; PUBLIC_KEY_LENGTH]>::try_from(raw.as_slice())
        .map_err(|_| "public_key is not a raw ed25519 public key")
}

/// Decode and width-check the join proof.
fn decode_join_proof(encoded: &str) -> Result<[u8; 64], &'static str> {
    let raw = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|_| "proof is not base64url")?;
    <[u8; 64]>::try_from(raw.as_slice()).map_err(|_| "proof is not a raw ed25519 signature")
}

/// A trusted signer as Control recorded it: its key and the zones it may mint
/// for, by NAME to zone id.
#[derive(Clone, Debug)]
pub struct TrustedSigner {
    /// The signer's verification key.
    pub public_key: [u8; PUBLIC_KEY_LENGTH],
    /// Zone NAME to zone id, for every zone this signer may mint for.
    pub zones: BTreeMap<String, String>,
}

/// The recorded key and permitted zones of an ACTIVE signer, or nothing.
///
/// THE `status` FILTER IS SIGNER REVOCATION, AND IT IS THE WHOLE OF IT. Both
/// operator verbs move the row to `revoked` and this read is what makes that
/// mean something; drop the filter and rotating a leaked key changes nothing at
/// all while looking exactly like a mechanism that ran and approved.
///
/// The zones come back with the key rather than from a second call, because the
/// two are one question - "may this signer mint this token" - and answering it
/// in two reads is how a caller ends up answering half of it.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read. A caller must
/// refuse on that rather than fall through: control that cannot reach the
/// registry has not established that this signer is trusted.
pub async fn trusted_join_signer(
    pg: &compio_postgres::Client,
    signer_id: &str,
) -> Result<Option<TrustedSigner>, compio_postgres::Error> {
    let rows = pg
        .query(
            "SELECT s.public_key, z.name, z.id \
               FROM zeroship.worker_join_signers s \
               JOIN zeroship.worker_join_signer_zones sz ON sz.signer_id = s.id \
               JOIN zeroship.execution_zones z ON z.id = sz.execution_zone_id \
              WHERE s.id = $1 AND s.status = $2",
            &[&signer_id, &ENROLLED_STATUS],
        )
        .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let stored: &[u8] = first.get(0);
    // `worker_join_signers_public_key_shape` already refuses every other width,
    // so this arm cannot fire on a row this platform wrote. It is here because
    // the alternative is an unwrap inside an authentication path over a value
    // read from a table.
    let Ok(public_key) = <[u8; PUBLIC_KEY_LENGTH]>::try_from(stored) else {
        return Ok(None);
    };
    let mut zones = BTreeMap::new();
    for row in &rows {
        zones.insert(row.get::<_, String>(1), row.get::<_, String>(2));
    }
    Ok(Some(TrustedSigner { public_key, zones }))
}

/// Why consuming a use failed.
#[derive(Debug)]
enum UseFailure {
    /// Every use this token was minted with has gone to some other key.
    Exhausted,
    /// The accounting could not be reached at all.
    Database(compio_postgres::Error),
}

/// Consume one use of `token` for `joining_key`, or refuse.
///
/// One server-side statement, so one transaction: see
/// `zeroship.claim_worker_join_use` for why the claim and the counter are two
/// guarded writes rather than a read followed by a write, and why an exhausted
/// token has to roll the claim back.
///
/// The key is `<signer issuer>|<token id>`, scoped by issuer exactly as the
/// service-assertion replay key is, so one signer cannot burn another's token
/// id and one table is safe to share across every signer.
async fn consume_join_use(
    pg: &compio_postgres::Client,
    token: &VerifiedJoinToken,
    joining_key: &str,
) -> Result<(), UseFailure> {
    let key = format!(
        "spiffe://{}/{}/{}|{}",
        zeroship_core::service_peers::SERVICE_TRUST_DOMAIN,
        zeroship_core::service_peers::WORKER_JOIN_SIGNER_SERVICE_NAME,
        token.signer_id,
        token.token_id
    );
    let uses = i32::try_from(token.uses).unwrap_or(i32::MAX);
    let expires_at: std::time::SystemTime = token.expires_at;
    pg.query_one(
        "SELECT zeroship.claim_worker_join_use($1, $2, $3, $4)",
        &[&key, &joining_key, &uses, &expires_at],
    )
    .await
    .map(|_| ())
    .map_err(|error| {
        use compio_postgres::error::SqlState;
        if error.code() == Some(&SqlState::INSUFFICIENT_RESOURCES) {
            UseFailure::Exhausted
        } else {
            UseFailure::Database(error)
        }
    })
}

/// Why [`join_instance`] refused, or could not tell.
///
/// A closed set over the SQLSTATEs `zeroship.join_worker_instance` raises, plus
/// the store-unavailable case every other registry read/write in this module
/// carries. Distinguishing the first three from a bare database error is what
/// lets [`join`] answer 403/409 rather than 500 for outcomes the design names.
#[derive(Debug)]
enum JoinFailure {
    /// The signer was not `active` when this call took its row lock: rotated or
    /// purged, possibly by a call that was waiting on this exact lock.
    SignerInactive,
    /// The registry does not record this signer for this zone. Reachable only
    /// if the Rust check above it stopped running, which is why it has a reason
    /// of its own.
    ZoneNotPermitted,
    /// The presented public key already names an instance joined under a
    /// DIFFERENT signer or token.
    PublicKeyConflict,
    /// The registry could not be consulted at all.
    Database(compio_postgres::Error),
}

/// Mint the id and the ring key, then spend them inside the server-side join
/// critical section.
///
/// Both mints happen HERE, after every check has passed, so nothing the
/// registrant sent has reached either of them. The lock-then-insert sequence
/// runs inside `zeroship.join_worker_instance` as ONE statement rather than a
/// client-driven transaction: `pg` is the process-wide shared `control_pg`
/// client every internal handler borrows concurrently (`&self`-taking calls
/// only), and compio-postgres's `Client::transaction` needs exclusive
/// (`&mut self`) access this call site does not have.
async fn join_instance(
    pg: &compio_postgres::Client,
    token: &VerifiedJoinToken,
    zone_id: &str,
    address: SocketAddr,
    public_key: &[u8; PUBLIC_KEY_LENGTH],
) -> Result<String, JoinFailure> {
    let instance_id = zeroship_core::typed_id::new_worker_instance_id();
    let ring_key = mint_ring_key();
    let host = address.ip();
    let port = i32::from(address.port());
    let lease = i32::try_from(INSTANCE_LEASE_TTL.as_secs()).unwrap_or(i32::MAX);
    let row = pg
        .query_one(
            "SELECT zeroship.join_worker_instance($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &token.signer_id,
                &token.token_id,
                &zone_id,
                &instance_id,
                &ring_key.as_slice(),
                &public_key.as_slice(),
                &host,
                &port,
                &lease,
            ],
        )
        .await
        .map_err(classify_join_error)?;
    Ok(row.get(0))
}

/// Route the function call's SQLSTATE to the outcome it names.
///
/// `insufficient_privilege`, `invalid_parameter_value` and `unique_violation`
/// are RAISED by `zeroship.join_worker_instance` itself for exactly the three
/// refusal cases it distinguishes; every other error - including a genuine
/// constraint violation this function did not anticipate - is a store failure
/// the caller cannot make sense of and must refuse on rather than guess at.
fn classify_join_error(error: compio_postgres::Error) -> JoinFailure {
    use compio_postgres::error::SqlState;
    match error.code() {
        Some(code) if code == &SqlState::INSUFFICIENT_PRIVILEGE => JoinFailure::SignerInactive,
        Some(code) if code == &SqlState::INVALID_PARAMETER_VALUE => JoinFailure::ZoneNotPermitted,
        Some(code) if code == &SqlState::UNIQUE_VIOLATION => JoinFailure::PublicKeyConflict,
        _ => JoinFailure::Database(error),
    }
}

/// The verification key a LIVE instance's assertions are checked under, or
/// nothing.
///
/// TWO FILTERS, AND EACH IS A DIFFERENT WAY A CREDENTIAL STOPS WORKING.
///
/// `status` is retirement and purge. Resolve the key without it and marking a
/// row `gone` changes nothing at all, while looking exactly like a revocation
/// mechanism that ran and approved.
///
/// `expires_at` is the LEASE, and it is what makes revocation stop being the
/// only way a credential ever dies. A worker that crashed, was killed, or was
/// simply forgotten leaves an `active` row with no process behind it; without
/// this comparison that row authenticates forever and only an operator noticing
/// would stop it. The comparison is against the DATABASE's clock, which is also
/// what keeps the answer the same across Control replicas whose clocks differ.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read. A caller must
/// refuse on that rather than fall through to anything else: control that
/// cannot reach the registry has not established that this instance is live.
pub async fn active_instance_public_key(
    pg: &compio_postgres::Client,
    instance_id: &str,
) -> Result<Option<[u8; PUBLIC_KEY_LENGTH]>, compio_postgres::Error> {
    let rows = pg
        .query(
            "SELECT public_key FROM zeroship.worker_instances \
              WHERE id = $1 AND status = $2 AND expires_at > now()",
            &[&instance_id, &ENROLLED_STATUS],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let stored: &[u8] = row.get(0);
    // `worker_instances_public_key_shape` already refuses every other width, so
    // this arm cannot fire on a row this platform wrote. It is here because the
    // alternative is an unwrap inside an authentication path over a value read
    // from a table: a width the database somehow holds must refuse the CALLER,
    // not the process.
    Ok(<[u8; PUBLIC_KEY_LENGTH]>::try_from(stored).ok())
}

// ---------------------------------------------------------------------------
// Renewal: an instance extending its own lease
// ---------------------------------------------------------------------------

/// Serve one lease renewal for the calling instance.
///
/// `instance_id` is the instance segment of the issuer the caller VERIFIED as,
/// never a value from a body, so no caller can renew another's identity. No
/// join token is involved and none would help: possession of the instance key
/// was proved at join, and demanding a fresh token here would make a use-capped
/// token useless.
///
/// A renewal that finds nothing to extend answers 403 rather than 204, and the
/// distinction matters to the worker: an identity that lapsed or was retired
/// cannot be revived, so the worker must stop rather than keep trying.
pub async fn renew(state: &AppState, instance_id: &str) -> web::HttpResponse {
    match renew_instance(&state.control_pg, instance_id).await {
        Ok(true) => {
            tracing::debug!(instance_id, "control-internal: worker instance lease renewed");
            web::HttpResponse::Ok().json(&WorkerLeaseRenewed {
                lease_seconds: INSTANCE_LEASE_TTL.as_secs(),
            })
        }
        Ok(false) => {
            tracing::warn!(
                instance_id,
                "control-internal: lease renewal refused - the instance is retired or lapsed"
            );
            web::HttpResponse::Forbidden().json(&serde_json::json!({
                "error": "renewal refused",
                "reason": "instance_not_live",
            }))
        }
        Err(error) => {
            tracing::error!(
                instance_id,
                %error,
                "control-internal: worker instance lease could not be renewed"
            );
            web::HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error":"service unavailable"}))
        }
    }
}

/// Extend one live instance's lease, returning whether it was extended.
///
/// `GREATEST` rather than an assignment, and that is the whole of the
/// concurrency story: several Control replicas serve renewals for one fleet,
/// and a slow replica whose statement commits after a newer one's would
/// otherwise move the expiry BACKWARDS to the window it computed before it
/// waited. Taking the later of the two makes the column monotonic whatever
/// order the writes land in.
///
/// The guard is the same predicate [`active_instance_public_key`] reads with,
/// so an identity that has lapsed cannot be renewed: expiry is terminal in the
/// way retirement is, and rejoining - which needs a token - is the way back.
async fn renew_instance(
    pg: &compio_postgres::Client,
    instance_id: &str,
) -> Result<bool, compio_postgres::Error> {
    let lease = f64::from(u32::try_from(INSTANCE_LEASE_TTL.as_secs()).unwrap_or(u32::MAX));
    let moved = pg
        .execute(
            "UPDATE zeroship.worker_instances \
                SET expires_at = GREATEST(expires_at, now() + make_interval(secs => $3)) \
              WHERE id = $1 AND status = $2 AND expires_at > now()",
            &[&instance_id, &ENROLLED_STATUS, &lease],
        )
        .await?;
    Ok(moved == 1)
}


// ---------------------------------------------------------------------------
// Retirement: an instance declaring its own exit
// ---------------------------------------------------------------------------

/// Serve one self-retirement: mark the calling instance `gone`.
///
/// `instance_id` is the instance segment of the issuer the caller VERIFIED as
/// (`crate::internal::retire_worker_instance`), never a value from a body, so
/// no caller can name another instance here.
///
/// Answers 204 whether or not this call was the one that moved the row: the
/// only way the update finds nothing to change is that the row stopped being
/// live between verification and now - retired by a purge of the signer that
/// admitted it, or by a racing call from the same process - and in either case
/// the instance is exactly as retired as the caller asked.
pub async fn retire(state: &AppState, instance_id: &str) -> web::HttpResponse {
    match retire_instance(&state.control_pg, instance_id).await {
        Ok(moved) => {
            tracing::info!(
                instance_id,
                moved,
                "control-internal: worker instance retired itself"
            );
            web::HttpResponse::NoContent().finish()
        }
        Err(error) => {
            // Retryable infrastructure, not a verdict on the caller: the row
            // is untouched and still `active`.
            tracing::error!(
                instance_id,
                %error,
                "control-internal: worker instance retirement could not be recorded"
            );
            web::HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error":"service unavailable"}))
        }
    }
}

/// Move one instance to `gone`, returning whether this statement moved it.
///
/// `gone` is terminal and the frozen-columns trigger lets only `status` change,
/// so this touches nothing but the one column the instance may speak for.
async fn retire_instance(
    pg: &compio_postgres::Client,
    instance_id: &str,
) -> Result<bool, compio_postgres::Error> {
    let moved = pg
        .execute(
            "UPDATE zeroship.worker_instances SET status = $2 WHERE id = $1 AND status <> $2",
            &[&instance_id, &RETIRED_STATUS],
        )
        .await?;
    Ok(moved == 1)
}

// ---------------------------------------------------------------------------
// The operator's trusted-signer import
// ---------------------------------------------------------------------------

// The document's shape and its validation are `zeroship_core::worker_join`,
// shared with `zeroship dev init`, which writes it. Each permitted zone is an
// execution zone's NAME (`zeroship.execution_zones.name`), the word an operator
// provisions by; the id it resolves to is Control's, and resolving it is the
// import's job.

/// What one import pass found, entry by entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JoinSignerImportReport {
    /// Entries Control had not recorded. Inserted `active`.
    pub inserted: usize,
    /// Entries already recorded exactly as the file states them, and active.
    pub unchanged: usize,
    /// Entries already recorded exactly as the file states them, and revoked.
    /// They STAY revoked: the import never writes `status` on a recorded row.
    pub revoked: usize,
}

/// Import the operator's trusted-signer file, or refuse it.
///
/// An empty `path` - the setting's default - imports nothing and returns
/// `None`: a deployment that trusts no signer has none, and every join it
/// receives is refused because no signer resolves.
///
/// # What the import may do, which is only ever to ADD
///
/// - An entry Control has not recorded is inserted `active`, with a row per
///   permitted zone.
/// - An entry recorded with the same key and the same zone set is left exactly
///   as it is. A REVOKED one stays revoked: revocation is terminal, and leaving
///   a rotated signer's line in the file must not restore it on the next
///   restart. That is what makes revocation survive a file nobody edited.
/// - An entry that DISAGREES with what is recorded refuses the whole file: the
///   same id under another key, the same key under another id, or the same id
///   with a different set of permitted zones. A key names exactly one signer
///   for its life, and a signer's zones ARE its authority - widening them is
///   provisioning a new signer, never re-keying or re-scoping a row.
/// - Recorded signers the file no longer names are left alone. Removing a line
///   revokes nothing; `zeroship.rotate_worker_join_signer` does.
///
/// Every entry is decided inside ONE transaction and nothing commits unless
/// every entry is admissible, so a refused file writes no row. Several Control
/// replicas importing concurrently converge, because the only write is an add:
/// the loser of an insert race finds the winner's row and judges it like any
/// other recorded row. A file that CONTRADICTS the record refuses only the
/// replica that read it, which during a rolling deploy means replicas fail one
/// at a time with a message naming the entries rather than a fleet that
/// half-believes a new file.
///
/// # Errors
///
/// Returns a message naming every refused entry when the file is unreadable,
/// malformed, internally inconsistent, names an unknown zone, or conflicts with
/// a recorded signer, and when the database cannot be reached. Callers treat
/// this as fatal: a boot that skipped its signers would refuse every join while
/// looking configured.
pub async fn import_join_signers(
    registry: &Registry,
    path: &Path,
) -> Result<Option<JoinSignerImportReport>, String> {
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    let signers = read_signer_file(path)?;
    let mut conn = registry
        .conn()
        .await
        .map_err(|error| format!("join signer import: connect: {error}"))?;
    let tx = conn
        .transaction()
        .await
        .map_err(|error| format!("join signer import: begin: {error}"))?;

    let zones = resolve_zones(&tx, &signers).await?;
    let mut report = JoinSignerImportReport::default();
    let mut refusals = Vec::new();
    for signer in &signers {
        match import_one(&tx, signer, &zones).await? {
            Imported::Inserted => report.inserted += 1,
            Imported::Unchanged => report.unchanged += 1,
            Imported::Revoked => {
                tracing::warn!(
                    signer_id = signer.id.as_str(),
                    "control: the join signer file still names a REVOKED signer; it stays \
                     revoked - remove the line, and provision a new signer"
                );
                report.revoked += 1;
            }
            Imported::Conflict(reason) => refusals.push(format!("{}: {reason}", signer.id)),
        }
    }
    if !refusals.is_empty() {
        // Dropping the transaction rolls it back; this is explicit so a refused
        // file is visibly a write-nothing outcome rather than one by omission.
        tx.rollback()
            .await
            .map_err(|error| format!("join signer import: rollback: {error}"))?;
        return Err(format!(
            "join signer file {} conflicts with recorded signers, and nothing was imported: {}",
            path.display(),
            refusals.join("; ")
        ));
    }
    tx.commit()
        .await
        .map_err(|error| format!("join signer import: commit: {error}"))?;
    Ok(Some(report))
}

/// Read and validate the import file, before any database work.
///
/// The document holds PUBLIC keys, so it is deliberately not held to a private
/// file's permission rule - the same call as the peer document.
fn read_signer_file(path: &Path) -> Result<Vec<JoinSignerRecord>, String> {
    let display = path.display();
    let bytes = std::fs::read(path)
        .map_err(|error| format!("join signer file {display}: read: {error}"))?;
    parse_join_signer_import(&bytes)
        .map_err(|reason| format!("join signer file {display}: {reason}"))
}

/// Map every zone name the file uses to its id, or refuse the file.
async fn resolve_zones(
    tx: &compio_postgres::Transaction<'_>,
    signers: &[JoinSignerRecord],
) -> Result<BTreeMap<String, String>, String> {
    let mut zones = BTreeMap::new();
    for signer in signers {
        for name in &signer.zones {
            if zones.contains_key(name.as_str()) {
                continue;
            }
            let row = tx
                .query_opt(
                    "SELECT id FROM zeroship.execution_zones WHERE name = $1 AND status = $2",
                    &[&name.as_str(), &DECLARED_ZONE_STATUS],
                )
                .await
                .map_err(|error| format!("join signer import: read execution zones: {error}"))?;
            let Some(row) = row else {
                return Err(format!(
                    "join signer file names execution zone {name:?}, which this deployment \
                     does not declare"
                ));
            };
            zones.insert(name.clone(), row.get::<_, String>(0));
        }
    }
    Ok(zones)
}

/// How one entry was judged.
enum Imported {
    Inserted,
    Unchanged,
    Revoked,
    Conflict(String),
}

/// Insert one entry if nothing claims its id or key, else judge what does.
///
/// `ON CONFLICT DO NOTHING` names no target on purpose: the id and the public
/// key are both unique, and a clash on EITHER must fall through to the
/// comparison below rather than abort the transaction.
async fn import_one(
    tx: &compio_postgres::Transaction<'_>,
    signer: &JoinSignerRecord,
    zones: &BTreeMap<String, String>,
) -> Result<Imported, String> {
    let fault =
        |error: compio_postgres::Error| format!("join signer import: signer {}: {error}", signer.id);
    let inserted = tx
        .execute(
            "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            &[&signer.id, &signer.public_key.as_slice(), &ENROLLED_STATUS],
        )
        .await
        .map_err(fault)?;
    if inserted == 1 {
        for zone in &signer.zones {
            tx.execute(
                "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
                 VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&signer.id, &zones[zone]],
            )
            .await
            .map_err(fault)?;
        }
        return Ok(Imported::Inserted);
    }
    let rows = tx
        .query(
            "SELECT id, public_key, status FROM zeroship.worker_join_signers \
             WHERE id = $1 OR public_key = $2",
            &[&signer.id, &signer.public_key.as_slice()],
        )
        .await
        .map_err(fault)?;
    let mut verdict = None;
    for row in &rows {
        let id: String = row.get(0);
        let public_key: &[u8] = row.get(1);
        let status: String = row.get(2);
        if id != signer.id {
            return Ok(Imported::Conflict(format!(
                "its public key is already recorded for signer {id}"
            )));
        }
        if public_key != signer.public_key.as_slice() {
            return Ok(Imported::Conflict(
                "it is already recorded with a different public key; a changed key is a new \
                 signer with a new id"
                    .to_owned(),
            ));
        }
        let recorded = recorded_zone_names(tx, &signer.id).await?;
        if recorded != signer.zones {
            return Ok(Imported::Conflict(format!(
                "it is already recorded for zones {}, not {}; a signer's zones are its \
                 authority, so widening them is a new signer with a new id",
                recorded.join(", "),
                signer.zones.join(", ")
            )));
        }
        verdict = Some(if status == REVOKED_STATUS {
            Imported::Revoked
        } else {
            Imported::Unchanged
        });
    }
    // The insert found a clash, so a row must have come back. If none did, a
    // concurrent writer removed it between the two statements - which nothing
    // in this tree does - and the honest answer is to refuse, not to guess.
    Ok(verdict.unwrap_or_else(|| {
        Imported::Conflict("a conflicting row disappeared during the import".to_owned())
    }))
}

/// The zone NAMES a recorded signer is permitted, sorted so the comparison
/// against the file's canonical list is order-insensitive at both ends.
async fn recorded_zone_names(
    tx: &compio_postgres::Transaction<'_>,
    signer_id: &str,
) -> Result<Vec<String>, String> {
    let rows = tx
        .query(
            "SELECT z.name FROM zeroship.worker_join_signer_zones sz \
               JOIN zeroship.execution_zones z ON z.id = sz.execution_zone_id \
              WHERE sz.signer_id = $1 ORDER BY z.name",
            &[&signer_id],
        )
        .await
        .map_err(|error| format!("join signer import: read recorded zones: {error}"))?;
    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}


#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    /// The one declaration every arm below shares, so a refusal and its control
    /// differ in exactly one variable.
    fn envelope() -> EnrolmentEnvelope {
        EnrolmentEnvelope::parse("10.7.0.0/16", "8080-8090", false).expect("declaration parses")
    }

    fn peer(text: &str) -> SocketAddr {
        text.parse().expect("peer socket parses")
    }

    /// THE CONTROL. Without it every refusal below passes against an envelope
    /// that refuses everything, which is the failure mode a suite of refusals
    /// cannot detect on its own.
    #[test]
    fn an_in_envelope_peer_on_a_permitted_port_is_admitted() {
        assert_eq!(
            envelope().derive_address(Some(peer("10.7.3.9:51314")), 8080),
            Ok("10.7.3.9:8080".parse().expect("expected socket"))
        );
    }

    #[test]
    fn the_advertised_port_is_the_claimed_one_not_the_observed_one() {
        // The worker's EPHEMERAL source port is not where it listens. Deriving
        // the host from the connection and the port from the caller is the whole
        // split, and reading the port off the socket would advertise a port
        // nothing is bound to.
        let derived = envelope()
            .derive_address(Some(peer("10.7.3.9:51314")), 8085)
            .expect("admitted");
        assert_eq!(derived.port(), 8085);
        assert_eq!(derived.ip(), "10.7.3.9".parse::<IpAddr>().expect("host"));
    }

    #[test]
    fn a_peer_outside_the_envelope_is_refused() {
        assert_eq!(
            envelope().derive_address(Some(peer("203.0.113.9:51314")), 8080),
            Err(EnrolmentRefusal::PeerOutsideEnvelope)
        );
    }

    #[test]
    fn a_loopback_peer_is_refused_when_the_envelope_does_not_declare_it() {
        for text in ["127.0.0.1:51314", "[::1]:51314", "[::ffff:127.0.0.1]:51314"] {
            assert_eq!(
                envelope().derive_address(Some(peer(text)), 8080),
                Err(EnrolmentRefusal::PeerOutsideEnvelope),
                "{text} is outside 10.7.0.0/16 and must be refused"
            );
        }
    }

    /// THE PAIRED CONTROL, differing from the case above in the declared
    /// networks and NOTHING ELSE. Without it that test passes against an
    /// envelope that refuses loopback unconditionally - which is exactly what
    /// this module did until 2026-09-07 - and the suite cannot tell a rule that
    /// consults the declaration from one that ignores it.
    ///
    /// The v4-mapped spelling is in here rather than in a test of its own
    /// because canonicalisation is load-bearing in the ADMIT direction too: an
    /// uncanonicalised `::ffff:127.0.0.1` is not found in `127.0.0.0/8`, so a
    /// dual-stack control would refuse the single-host deployment its operator
    /// just declared.
    #[test]
    fn a_loopback_peer_is_admitted_when_the_envelope_declares_it() {
        let single_host =
            EnrolmentEnvelope::parse("127.0.0.0/8", "8080-8090", false).expect("declaration parses");
        for text in ["127.0.0.1:51314", "[::ffff:127.0.0.1]:51314"] {
            let derived = single_host
                .derive_address(Some(peer(text)), 8085)
                .unwrap_or_else(|refusal| panic!("{text} must be admitted, got {refusal:?}"));
            assert_eq!(derived.ip(), "127.0.0.1".parse::<IpAddr>().expect("host"));
            assert_eq!(derived.port(), 8085);
        }
    }

    /// The unspecified address stays refused EVEN WHEN A DECLARED NETWORK
    /// CONTAINS IT, which is the deliberate asymmetry with loopback. Loopback is
    /// a real destination an operator may legitimately mean; `0.0.0.0` is not a
    /// destination at all, so admitting it would write a row the dispatcher
    /// cannot use.
    ///
    /// The declaration here is `0.0.0.0/8` rather than the default route: the
    /// grammar refuses `0.0.0.0/0` outright as declaring no bound, so there is
    /// no such thing as a widest declaration to test against. `0.0.0.0/8` is the
    /// strongest one that exists AND contains the address, which is what makes
    /// this a control rather than a refusal that would have fired anyway.
    #[test]
    fn the_unspecified_peer_is_refused_even_inside_a_declared_network() {
        let containing =
            EnrolmentEnvelope::parse("0.0.0.0/8", "8080-8090", false).expect("declaration parses");
        assert_eq!(
            containing.derive_address(Some(peer("0.0.0.0:51314")), 8080),
            Err(EnrolmentRefusal::PeerIsUnspecified)
        );
        // The paired control: a NON-unspecified address in that same network is
        // admitted, so the refusal above is about the address and not about the
        // declaration being rejected somewhere upstream.
        assert!(
            containing.derive_address(Some(peer("0.1.2.3:51314")), 8080).is_ok(),
            "0.1.2.3 is inside 0.0.0.0/8 and is a real address"
        );
    }

    #[test]
    fn an_ipv4_mapped_in_envelope_peer_is_admitted_as_ipv4() {
        // The paired control for the case above, and the reason the
        // canonicalisation is not merely a refusal trick: the derived host must
        // be the v4 spelling, or the `inet` column and every URL built from it
        // carry a mapped form of an address the operator declared in v4.
        let derived = envelope()
            .derive_address(Some(peer("[::ffff:10.7.3.9]:51314")), 8080)
            .expect("admitted");
        assert_eq!(derived.ip(), "10.7.3.9".parse::<IpAddr>().expect("host"));
    }

    #[test]
    fn a_port_outside_the_range_is_refused_at_both_ends() {
        for port in [8079, 8091] {
            assert_eq!(
                envelope().derive_address(Some(peer("10.7.3.9:51314")), port),
                Err(EnrolmentRefusal::PortOutsideEnvelope),
                "port {port} must be refused"
            );
        }
        // Both ends of the range itself are admitted, so the refusals above are
        // the bound and not an off-by-one that refuses everything.
        for port in [8080, 8090] {
            assert!(
                envelope().derive_address(Some(peer("10.7.3.9:51314")), port).is_ok(),
                "port {port} is inside the declared range"
            );
        }
    }

    #[test]
    fn an_undeclared_envelope_refuses_rather_than_defaulting_open() {
        let peer = peer("10.7.3.9:51314");
        assert_eq!(
            EnrolmentEnvelope::closed().derive_address(Some(peer), 8080),
            Err(EnrolmentRefusal::EnvelopeUnset)
        );
        // Half a declaration is not a declaration: either half missing refuses.
        for (networks, ports) in [("10.7.0.0/16", ""), ("", "8080-8090")] {
            let envelope =
                EnrolmentEnvelope::parse(networks, ports, false).expect("halves parse");
            assert_eq!(
                envelope.derive_address(Some(peer), 8080),
                Err(EnrolmentRefusal::EnvelopeUnset),
                "networks={networks:?} ports={ports:?} must refuse"
            );
        }
    }

    #[test]
    fn a_proxy_fronted_control_plane_refuses_every_enrolment() {
        // Behind a trusted proxy every observed peer IS the proxy, so the
        // derivation would collapse to "everything is the proxy". Refusing is
        // the only answer that does not silently place every worker at one
        // address - and reading the forwarded header instead would be the
        // caller-supplied fallback this module exists to prevent.
        let envelope =
            EnrolmentEnvelope::parse("10.7.0.0/16", "8080-8090", true).expect("parses");
        assert_eq!(
            envelope.derive_address(Some(peer("10.7.3.9:51314")), 8080),
            Err(EnrolmentRefusal::ProxyFronted)
        );
    }

    #[test]
    fn an_unobservable_peer_refuses_with_no_fallback() {
        assert_eq!(
            envelope().derive_address(None, 8080),
            Err(EnrolmentRefusal::PeerAddressUnobservable)
        );
    }

    #[test]
    fn a_default_route_is_refused_as_a_declaration() {
        for token in ["0.0.0.0/0", "::/0"] {
            assert!(
                EnrolmentEnvelope::parse(token, "8080", false).is_err(),
                "{token} declares no bound and must be refused"
            );
        }
        // The control: one bit narrower is a declaration and parses.
        assert!(EnrolmentEnvelope::parse("0.0.0.0/1", "8080", false).is_ok());
    }

    #[test]
    fn a_malformed_declaration_is_refused_rather_than_dropped() {
        // A token that silently fails to parse would narrow or widen the
        // envelope without saying so, which is how an operator ends up trusting
        // a bound that is not the one they wrote.
        assert!(EnrolmentEnvelope::parse("10.7.0.0", "8080", false).is_err());
        assert!(EnrolmentEnvelope::parse("10.7.0.0/16", "0-90", false).is_err());
        assert!(EnrolmentEnvelope::parse("10.7.0.0/16", "9090-8080", false).is_err());
        assert!(EnrolmentEnvelope::parse("10.7.0.0/16", "http", false).is_err());
    }

    #[test]
    fn a_declaration_canonicalises_its_networks_and_accepts_several() {
        let envelope =
            EnrolmentEnvelope::parse(" 10.7.0.1/16 , fd00::5/64 ", "8080", false)
                .expect("parses");
        assert!(envelope
            .derive_address(Some(peer("10.7.99.4:1")), 8080)
            .is_ok());
        assert!(envelope
            .derive_address(Some(peer("[fd00::9]:1")), 8080)
            .is_ok());
        assert_eq!(
            envelope.derive_address(Some(peer("[fd01::9]:1")), 8080),
            Err(EnrolmentRefusal::PeerOutsideEnvelope)
        );
    }

    #[test]
    fn two_minted_ring_keys_differ() {
        // The registrant contributes nothing to this value, so two mints under
        // IDENTICAL caller input must still differ. This arm rules on the mint;
        // the end-to-end statement - two enrolments with byte-identical bodies
        // from one peer land two different ring keys in the table - is bound by
        // the live-database arm in tests/worker_enrolment_test.rs.
        let first = mint_ring_key();
        let second = mint_ring_key();
        assert_ne!(first, second);
        assert_eq!(first.len(), RING_KEY_BYTES);
        assert_ne!(first, [0_u8; RING_KEY_BYTES]);
    }

    #[test]
    fn a_public_key_of_the_wrong_width_is_refused() {
        let good = URL_SAFE_NO_PAD.encode([7_u8; PUBLIC_KEY_LENGTH]);
        assert_eq!(
            decode_instance_public_key(&good),
            Ok([7_u8; PUBLIC_KEY_LENGTH])
        );
        for bad in [
            URL_SAFE_NO_PAD.encode([7_u8; PUBLIC_KEY_LENGTH - 1]),
            URL_SAFE_NO_PAD.encode([7_u8; PUBLIC_KEY_LENGTH + 1]),
            String::new(),
            "not base64!!".to_string(),
        ] {
            assert!(
                decode_instance_public_key(&bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }
}
