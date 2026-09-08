//! Worker-instance enrolment: the operator-declared address envelope, the
//! address derivation, the ring-key mint, and the row control writes.
//!
//! ONE ROW IS ONE LIVE WORKER PROCESS. The worker generates an Ed25519 instance
//! keypair at boot, in memory, never on disk, and enrols the public half over
//! HTTP authenticated by the `svc/worker` role key it already holds. Control
//! writes the row; the worker holds no privilege on the table. The schema and
//! the reasons for each of its columns are in
//! `db/migrations-ts/20260907000300_worker_instances.ts`.
//!
//! WHAT THIS BUYS, STATED SO NOTHING HERE OVERSELLS IT. Enrolment authenticates
//! with the SHARED role key, so a holder of that key can enrol many instances.
//! Per-instance identity is a DISTINGUISHER against a role-key holder, NOT a
//! boundary. What it buys is attribution, per-instance revocation, a countable
//! event, and it is what makes a per-app placement fence writable at all.
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
//! # Enrolment is NOT idempotent, and here is what that costs
//!
//! Every admitted enrolment mints a fresh instance id and inserts a fresh row.
//! There is no upsert and no lookup-by-key first. Three consequences, all real:
//!
//! 1. A worker restart is a NEW instance by construction - the keypair is
//!    generated at boot in memory, so there is nothing to be idempotent about
//!    across restarts. The old row stays `active` with no process behind it.
//! 2. NOTHING marks that row `gone`. There is no reaper and no liveness sweep,
//!    and neither is designed. Rows accumulate. Do not read the `gone` status as
//!    evidence that something transitions to it; today only a human does.
//! 3. A retried enrolment inside one boot - the response was lost, the worker
//!    asks again - mints a SECOND row for one process, and control cannot tell
//!    that pair from two processes.
//!
//! Making it idempotent needs a uniqueness key the table deliberately does not
//! carry, and choosing that key is a lifecycle decision this step does not make:
//! a unique `(advertise_host, advertise_port)` would let one `gone` row block the
//! same worker re-registering after a restart. A read-then-write dedupe would be
//! racy and unenforced, which is the shape this codebase records as "claims that
//! read as protection". So: not idempotent, said plainly, rather than idempotent
//! in appearance.

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
    /// The peer is a loopback address.
    PeerIsLoopback,
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
            Self::PeerIsLoopback => "peer_is_loopback",
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
    /// An IPv4-mapped IPv6 peer is canonicalised BEFORE the loopback check.
    /// Without that, `::ffff:127.0.0.1` answers `false` to
    /// `Ipv6Addr::is_loopback` and walks straight past the loopback fence.
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
        if host.is_loopback() {
            return Err(EnrolmentRefusal::PeerIsLoopback);
        }
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
/// live-server arm that gets a genuine loopback peer and is refused BY NAME for
/// being loopback rather than for having no peer.
pub async fn enrol(
    state: &AppState,
    peer: Option<SocketAddr>,
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

    match insert_instance(&state.control_pg, address, &public_key).await {
        Ok(instance_id) => {
            tracing::info!(
                instance_id = %instance_id,
                advertise_host = %address.ip(),
                advertise_port = address.port(),
                "control-internal: worker instance enrolled"
            );
            web::HttpResponse::Created().json(&WorkerEnrolmentAccepted { instance_id })
        }
        Err(error) => {
            tracing::error!(
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

/// Mint the id and the ring key, and write the row.
///
/// Both mints happen HERE, after the address was derived and admitted, so
/// nothing the registrant sent has reached either of them.
async fn insert_instance(
    pg: &compio_postgres::Client,
    address: SocketAddr,
    public_key: &[u8; PUBLIC_KEY_LENGTH],
) -> Result<String, compio_postgres::Error> {
    let instance_id = zeroship_core::typed_id::new_worker_instance_id();
    let ring_key = mint_ring_key();
    let host = address.ip();
    let port = i32::from(address.port());
    pg.execute(
        "INSERT INTO zeroship.worker_instances \
         (id, ring_key, public_key, advertise_host, advertise_port, status) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &instance_id,
            &ring_key.as_slice(),
            &public_key.as_slice(),
            &host,
            &port,
            &ENROLLED_STATUS,
        ],
    )
    .await?;
    Ok(instance_id)
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
    fn a_loopback_peer_is_refused() {
        for text in ["127.0.0.1:51314", "[::1]:51314"] {
            assert_eq!(
                envelope().derive_address(peer(text), 8080),
                Err(EnrolmentRefusal::PeerIsLoopback),
                "{text} must be refused as loopback"
            );
        }
    }

    #[test]
    fn an_ipv4_mapped_loopback_peer_is_refused_as_loopback() {
        // `Ipv6Addr::is_loopback` answers FALSE for `::ffff:127.0.0.1`, so
        // without the canonicalisation this walks past the fence. A dual-stack
        // listener reports exactly this spelling for a v4 client.
        assert_eq!(
            envelope().derive_address(peer("[::ffff:127.0.0.1]:51314"), 8080),
            Err(EnrolmentRefusal::PeerIsLoopback)
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
