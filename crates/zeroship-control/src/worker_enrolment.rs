//! Worker-instance enrolment: the operator-declared address envelope, the
//! address derivation, the ring-key mint, and the row control writes.
//!
//! ONE ROW IS ONE LIVE WORKER PROCESS. The worker generates an Ed25519 instance
//! keypair at boot, in memory, never on disk, and enrols the public half over
//! HTTP authenticated by the mounted enroller key of its deployment unit (a
//! host or pool in exactly one execution zone), under the issuer
//! `svc/worker-enroller/<wen_id>`. Control writes the row; the worker holds no
//! privilege on the table. The schema and the reasons for each of its columns
//! are in `db/migrations-ts/20260907000300_worker_instances.ts` and
//! `db/migrations-ts/20260914000000_execution_zones_and_worker_enrollers.ts` /
//! `db/migrations-ts/20260914000100_worker_instances_enroller_binding.ts`.
//!
//! WHAT THIS BUYS, STATED SO NOTHING HERE OVERSELLS IT (option 1A of the
//! worker-enrollment-bootstrap design). Enrolment authenticates with a key
//! SHARED BY THE DEPLOYMENT UNIT, so a holder of it can enrol many instances in
//! that unit and zone. Per-instance identity is a DISTINGUISHER against a
//! unit-key holder, NOT a boundary WITHIN the unit. What it buys is
//! attribution, per-instance revocation, a countable event, and - because
//! every instance carries the `enroller_id` of the unit that admitted it -
//! revoking that ONE enroller row (`zeroship.revoke_worker_enroller`, an
//! explicit operator database operation with no runtime EXECUTE grant) retires
//! every instance it ever enrolled in one transaction. A revoked unit cannot
//! regain equivalent authority by enrolling a fresh instance identity: the
//! enroller row itself is what `enrol_worker_instance` locks and checks, and a
//! revoked one refuses before any row is written.
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
//! **The address is derived from the enrolment connection.** The worker
//! contributes only its listening port. Control takes the host from the observed
//! peer address and validates the pair against [`EnrolmentEnvelope`].
//! There is deliberately NO fallback to a caller-supplied host: that fallback is
//! the vulnerability, not a convenience. `collect_forwarded_headers` in
//! `crates/zeroship-gateway/src/router/dispatch.rs` strips a named header set
//! and COOKIE IS NOT IN IT, and `forward_dispatch` posts the full request -
//! body, cookies, and the gateway-signed user envelope - to whatever address the
//! ring returns. A registrant-supplied address would therefore let a role-key
//! holder intercept and impersonate end-user sessions under the app's own
//! origin, which is worse than the exposure the registry exists to reduce.
//!
//! Derivation is also what makes the design deployable: a per-process address
//! setting has no producer, because compose replicas share one environment block
//! and a Kubernetes Deployment is one pod spec for N pods, so every replica would
//! present the same address.
//!
//! # Enrolment IS idempotent, on the instance's public key
//!
//! [`enrol`] mints a candidate instance id and ring key up front, then spends
//! them inside `zeroship.enrol_worker_instance`
//! (`db/migrations-ts/20260914000100_worker_instances_enroller_binding.ts`),
//! which conflicts on `worker_instances.public_key`. Three consequences:
//!
//! 1. A worker restart is a NEW instance by construction - the keypair is
//!    generated at boot in memory, so there is nothing to deduplicate across
//!    restarts. The old row stays `active` with no process behind it, exactly
//!    as before; nothing here adds a reaper or a liveness sweep.
//! 2. A retried enrolment inside one boot - the response was lost, the worker
//!    asks again with the SAME instance key - returns the id of the row the
//!    first attempt (or a concurrent racing attempt) already committed,
//!    rather than minting a second row for one process. This closes the
//!    double-row-per-lost-reply gap this module used to record here as an
//!    open cost.
//! 3. The SAME public key presented under a DIFFERENT enroller is a conflict,
//!    refused rather than silently reassigned: a public key names exactly one
//!    enroller for its life, so row surgery or a restored enroller cannot
//!    transfer an existing instance's authority to itself.
//!
//! # The enroller row is locked, and the lock is what makes revocation exact
//!
//! `enrol_worker_instance` first takes a guarded no-op update lock on the
//! calling enroller's row, conditioned on `status = 'active'`. A concurrent
//! `zeroship.revoke_worker_enroller` call's own first UPDATE targets that same
//! row and queues behind this lock, so revocation always observes every
//! enrolment that committed before it and marks the resulting instance `gone`
//! in the same operator transaction. An enroller found `revoked` at lock time
//! refuses before any instance row is written - the race a compromised or
//! decommissioned unit's in-flight enrolments lose.
//!
//! # The rows are READ as well as written, and the read is where revocation lives
//!
//! [`active_instance_public_key`] and [`active_enroller_public_key`] are their
//! tables' readers. Control resolves one of them before verifying an assertion
//! whose `iss` names an instance (`crate::internal::resolve_instance_public_key`),
//! because no peer document has ever carried an instance or enroller key: the
//! keypair is drawn in memory (the worker's, at boot) or mounted as a file
//! outside any document control loads (the enroller's, by the operator). The
//! `status` filter on each read is the ONLY thing that makes marking a row
//! `draining`/`gone`/`revoked` mean anything, which is why it is stated on the
//! reader rather than left to the caller.

use std::net::SocketAddr;
use std::ops::RangeInclusive;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::PUBLIC_KEY_LENGTH;
use ipnet::IpNet;
use ntex::web;
use rand::RngCore as _;
use serde::Deserialize;

use crate::AppState;

/// Width of the ring key control mints, in bytes.
///
/// CONTROL'S DECISION, not a protocol constant, and deliberately not a CHECK in
/// the schema either - `worker_instances_ring_key_present` fences a row that
/// carries no key at all and says nothing about width, because the width is
/// whatever the minting side chooses. It is wide enough that the key space is
/// not the weak term in any placement argument.
pub const RING_KEY_BYTES: usize = 32;

/// The status control writes on an admitted enrolment.
///
/// The other two members of the column's closed set (`draining`, `gone`) have no
/// writer in this tree. See the module header.
const ENROLLED_STATUS: &str = "active";

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
/// already learn from success-versus-failure, and the caller here has already
/// proved it holds `svc/worker` before any of these are reachable.
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
    pub fn closed() -> Self {
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
    pub fn is_declared(&self) -> bool {
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
    /// address, so there is nothing to grind toward; and a same-host caller
    /// already reads the `svc/worker` key file, so it gains no reach it lacked.
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

/// What a worker sends. Its listening PORT and its instance PUBLIC KEY, and
/// nothing else - there is no host field on purpose, and adding one is the
/// vulnerability the module header describes.
#[derive(Debug, Deserialize)]
pub struct WorkerEnrolmentRequest {
    /// The port the worker is listening on.
    pub port: u16,
    /// The raw Ed25519 public key, base64url without padding.
    pub public_key: String,
}

/// What control returns on an admitted enrolment.
#[derive(Debug, serde::Serialize)]
pub struct WorkerEnrolmentAccepted {
    /// The minted `wkr_` instance id. The worker mints under
    /// `svc/worker/<instance_id>` and is still ADDRESSED as `svc/worker`.
    pub instance_id: String,
}

/// Serve one enrolment.
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
/// outside the declared envelope rather than for having no peer. The two
/// refusals being distinct is what gives that arm its power: it proves the
/// handler read an address, not merely that it failed.
pub async fn enrol(
    state: &AppState,
    peer: Option<SocketAddr>,
    enroller_id: &str,
    request: WorkerEnrolmentRequest,
) -> web::HttpResponse {
    let public_key = match decode_instance_public_key(&request.public_key) {
        Ok(key) => key,
        Err(message) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": message}));
        }
    };

    let address = match state
        .worker_enrolment
        .derive_address(peer, request.port)
    {
        Ok(address) => address,
        Err(refusal) => {
            tracing::warn!(
                peer = ?peer,
                claimed_port = request.port,
                reason = refusal.as_str(),
                "control-internal: worker enrolment refused"
            );
            let body = serde_json::json!({
                "error": "enrolment refused",
                "reason": refusal.as_str(),
            });
            return if refusal.is_deployment_state() {
                web::HttpResponse::ServiceUnavailable().json(&body)
            } else {
                web::HttpResponse::Forbidden().json(&body)
            };
        }
    };

    match enrol_instance(&state.control_pg, enroller_id, address, &public_key).await {
        Ok(instance_id) => {
            tracing::info!(
                enroller_id,
                instance_id = %instance_id,
                advertise_host = %address.ip(),
                advertise_port = address.port(),
                "control-internal: worker instance enrolled"
            );
            web::HttpResponse::Created().json(&WorkerEnrolmentAccepted { instance_id })
        }
        Err(EnrolmentFailure::EnrollerInactive) => {
            tracing::warn!(
                enroller_id,
                "control-internal: worker enrolment refused - enroller is not active"
            );
            web::HttpResponse::Forbidden().json(&serde_json::json!({
                "error": "enrolment refused",
                "reason": "enroller_inactive",
            }))
        }
        Err(EnrolmentFailure::PublicKeyConflict) => {
            tracing::warn!(
                enroller_id,
                "control-internal: worker enrolment refused - public key enrolled under another enroller"
            );
            web::HttpResponse::Conflict().json(&serde_json::json!({
                "error": "enrolment refused",
                "reason": "public_key_conflict",
            }))
        }
        Err(EnrolmentFailure::Database(error)) => {
            tracing::error!(
                enroller_id,
                error = %error,
                advertise_host = %address.ip(),
                advertise_port = address.port(),
                "control-internal: worker enrolment insert failed"
            );
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "internal error"}))
        }
    }
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

/// Why [`enrol_instance`] refused, or could not tell.
///
/// A closed set over the SQLSTATEs `zeroship.enrol_worker_instance` raises
/// (`db/migrations-ts/20260914000100_worker_instances_enroller_binding.ts`),
/// plus the store-unavailable case every other registry read/write in this
/// module carries. Distinguishing the first two from a bare database error is
/// what lets [`enrol`] answer 403/409 rather than 500 for the two outcomes the
/// design specifically names.
#[derive(Debug)]
enum EnrolmentFailure {
    /// The enroller was not `active` when this call took its row lock: either
    /// it was never enrolled, or it was revoked - possibly by a
    /// `zeroship.revoke_worker_enroller` call that was waiting on this exact
    /// lock and proceeded the instant this call released it.
    EnrollerInactive,
    /// The presented public key already names an instance enrolled under a
    /// DIFFERENT enroller. A public key names exactly one enroller for its
    /// life; this is not the lost-reply retry case, which returns `Ok` with
    /// the existing instance id instead.
    PublicKeyConflict,
    /// The registry could not be consulted at all.
    Database(compio_postgres::Error),
}

/// Mint the id and the ring key, then spend them inside the server-side
/// enrolment critical section.
///
/// Both mints happen HERE, after the address was derived and admitted, so
/// nothing the registrant sent has reached either of them. The lock-then-
/// insert sequence itself runs inside `zeroship.enrol_worker_instance` as ONE
/// statement rather than a client-driven multi-statement transaction: `pg` is
/// the process-wide shared `control_pg` client every internal handler borrows
/// concurrently (`&self`-taking calls only), and compio-postgres's
/// `Client::transaction` needs exclusive (`&mut self`) access this call site
/// does not have. See the function's own migration-file comment for why the
/// lock and the insert have to be one round trip for the race in option 1A's
/// PoC (revoke-during-enrol) to be judged correctly rather than by a sleep.
///
/// # Errors
///
/// Returns [`EnrolmentFailure`] on refusal or when the registry could not be
/// read at all.
async fn enrol_instance(
    pg: &compio_postgres::Client,
    enroller_id: &str,
    address: SocketAddr,
    public_key: &[u8; PUBLIC_KEY_LENGTH],
) -> Result<String, EnrolmentFailure> {
    let instance_id = zeroship_core::typed_id::new_worker_instance_id();
    let ring_key = mint_ring_key();
    let host = address.ip();
    let port = i32::from(address.port());
    let row = pg
        .query_one(
            "SELECT zeroship.enrol_worker_instance($1, $2, $3, $4, $5, $6)",
            &[
                &enroller_id,
                &instance_id,
                &ring_key.as_slice(),
                &public_key.as_slice(),
                &host,
                &port,
            ],
        )
        .await
        .map_err(classify_enrolment_error)?;
    Ok(row.get(0))
}

/// Route the function call's SQLSTATE to the outcome it names.
///
/// `insufficient_privilege` and `unique_violation` are RAISED by
/// `zeroship.enrol_worker_instance` itself for exactly the two refusal cases
/// it distinguishes; every other error - including a genuine constraint
/// violation this function did not anticipate - is a store failure the caller
/// cannot make sense of and must refuse on rather than guess at.
fn classify_enrolment_error(error: compio_postgres::Error) -> EnrolmentFailure {
    use compio_postgres::error::SqlState;
    match error.code() {
        Some(code) if code == &SqlState::INSUFFICIENT_PRIVILEGE => {
            EnrolmentFailure::EnrollerInactive
        }
        Some(code) if code == &SqlState::UNIQUE_VIOLATION => EnrolmentFailure::PublicKeyConflict,
        _ => EnrolmentFailure::Database(error),
    }
}

/// The verification key an ACTIVE instance's assertions are checked under, or
/// nothing.
///
/// THE `status` FILTER IS PER-INSTANCE REVOCATION, AND IT IS THE WHOLE OF IT.
/// The registry buys attribution, a countable event, and the ability to retire
/// one process without touching the enroller key its whole deployment unit
/// shares; the third is bought HERE and nowhere else. Resolve the key without
/// the filter and marking a row `gone` changes nothing at all, while looking
/// exactly like a revocation mechanism that ran and approved.
///
/// It admits exactly `ENROLLED_STATUS`. The write side moved into
/// `zeroship.enrol_worker_instance` (a literal `'active'` in its own migration
/// file) when enrolment became a single server-side statement, so this is now
/// a SECOND spelling of that string rather than a shared Rust constant - the
/// two must be kept in agreement by convention, and `worker_instances_status_check`
/// / `worker_enrollers_status_check` are what would catch either one drifting
/// to a value the other does not recognise. The other two members of the
/// column's closed set authenticate nothing.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read. A caller must
/// refuse on that rather than fall through to anything else: control that
/// cannot reach the registry has not established that this instance is live.
pub(crate) async fn active_instance_public_key(
    pg: &compio_postgres::Client,
    instance_id: &str,
) -> Result<Option<[u8; PUBLIC_KEY_LENGTH]>, compio_postgres::Error> {
    let rows = pg
        .query(
            "SELECT public_key FROM zeroship.worker_instances WHERE id = $1 AND status = $2",
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

/// The verification key an ACTIVE enroller's assertions are checked under, or
/// nothing.
///
/// The enroller-table twin of [`active_instance_public_key`], read by the same
/// `status = 'active'` predicate its own table uses, and by nothing else: an
/// enroller found `revoked` here holds no credential, exactly as an instance
/// found `draining`/`gone` does not. `zeroship.revoke_worker_enroller`
/// (`db/migrations-ts/20260914000100_worker_instances_enroller_binding.ts`) is
/// the row's only writer of `status`, and it is an explicit operator database
/// operation with no runtime EXECUTE grant - Control never calls it.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read, for the same
/// reason [`active_instance_public_key`] does: a store that cannot answer has
/// not established that this enroller is live.
pub(crate) async fn active_enroller_public_key(
    pg: &compio_postgres::Client,
    enroller_id: &str,
) -> Result<Option<[u8; PUBLIC_KEY_LENGTH]>, compio_postgres::Error> {
    let rows = pg
        .query(
            "SELECT public_key FROM zeroship.worker_enrollers WHERE id = $1 AND status = $2",
            &[&enroller_id, &ENROLLED_STATUS],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let stored: &[u8] = row.get(0);
    // `worker_enrollers_public_key_shape` already refuses every other width;
    // see the parallel comment on `active_instance_public_key`.
    Ok(<[u8; PUBLIC_KEY_LENGTH]>::try_from(stored).ok())
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

    fn peer(text: &str) -> Option<SocketAddr> {
        Some(text.parse().expect("peer socket parses"))
    }

    /// THE CONTROL. Without it every refusal below passes against an envelope
    /// that refuses everything, which is the failure mode a suite of refusals
    /// cannot detect on its own.
    #[test]
    fn an_in_envelope_peer_on_a_permitted_port_is_admitted() {
        assert_eq!(
            envelope().derive_address(peer("10.7.3.9:51314"), 8080),
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
            .derive_address(peer("10.7.3.9:51314"), 8085)
            .expect("admitted");
        assert_eq!(derived.port(), 8085);
        assert_eq!(derived.ip(), "10.7.3.9".parse::<IpAddr>().expect("host"));
    }

    #[test]
    fn a_peer_outside_the_envelope_is_refused() {
        assert_eq!(
            envelope().derive_address(peer("203.0.113.9:51314"), 8080),
            Err(EnrolmentRefusal::PeerOutsideEnvelope)
        );
    }

    #[test]
    fn a_loopback_peer_is_refused_when_the_envelope_does_not_declare_it() {
        for text in ["127.0.0.1:51314", "[::1]:51314", "[::ffff:127.0.0.1]:51314"] {
            assert_eq!(
                envelope().derive_address(peer(text), 8080),
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
                .derive_address(peer(text), 8085)
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
            containing.derive_address(peer("0.0.0.0:51314"), 8080),
            Err(EnrolmentRefusal::PeerIsUnspecified)
        );
        // The paired control: a NON-unspecified address in that same network is
        // admitted, so the refusal above is about the address and not about the
        // declaration being rejected somewhere upstream.
        assert!(
            containing.derive_address(peer("0.1.2.3:51314"), 8080).is_ok(),
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
            .derive_address(peer("[::ffff:10.7.3.9]:51314"), 8080)
            .expect("admitted");
        assert_eq!(derived.ip(), "10.7.3.9".parse::<IpAddr>().expect("host"));
    }

    #[test]
    fn a_port_outside_the_range_is_refused_at_both_ends() {
        for port in [8079, 8091] {
            assert_eq!(
                envelope().derive_address(peer("10.7.3.9:51314"), port),
                Err(EnrolmentRefusal::PortOutsideEnvelope),
                "port {port} must be refused"
            );
        }
        // Both ends of the range itself are admitted, so the refusals above are
        // the bound and not an off-by-one that refuses everything.
        for port in [8080, 8090] {
            assert!(
                envelope().derive_address(peer("10.7.3.9:51314"), port).is_ok(),
                "port {port} is inside the declared range"
            );
        }
    }

    #[test]
    fn an_undeclared_envelope_refuses_rather_than_defaulting_open() {
        let peer = peer("10.7.3.9:51314");
        assert_eq!(
            EnrolmentEnvelope::closed().derive_address(peer, 8080),
            Err(EnrolmentRefusal::EnvelopeUnset)
        );
        // Half a declaration is not a declaration: either half missing refuses.
        for (networks, ports) in [("10.7.0.0/16", ""), ("", "8080-8090")] {
            let envelope =
                EnrolmentEnvelope::parse(networks, ports, false).expect("halves parse");
            assert_eq!(
                envelope.derive_address(peer, 8080),
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
            envelope.derive_address(peer("10.7.3.9:51314"), 8080),
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
            .derive_address(peer("10.7.99.4:1"), 8080)
            .is_ok());
        assert!(envelope
            .derive_address(peer("[fd00::9]:1"), 8080)
            .is_ok());
        assert_eq!(
            envelope.derive_address(peer("[fd01::9]:1"), 8080),
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
