//! Where a service's own signing key and its peers' public keys come from.
//!
//! A service that mints a [service assertion](crate::service_assertion) needs
//! two facts it cannot compute: its own ed25519 private key, and the public
//! half of every peer whose assertions it verifies. This module is the ONLY
//! place either is loaded, so both ends of every internal edge read the same
//! shapes and derive the same `kid`.
//!
//! # Why the peer keys are CONFIGURED and not FETCHED
//!
//! The obvious alternative is a JWKS document each service publishes and its
//! peers poll, riding the route-table poll the gateway already runs. It is
//! refused here, for the reasons [`crate::service_assertion::ServiceTrustBundle`]
//! already records against its own index, plus two that are specific to this
//! deployment:
//!
//! - **The transport a fetched document would ride is the one the assertions
//!   exist to stop trusting.** Internal hops are cleartext HTTP here (neither
//!   the gateway nor the worker declares rustls), so a polled JWKS is
//!   attacker-substitutable by anyone who can already reach the network - which
//!   is exactly the adversary an asymmetric peer credential is for.
//! - **The only feed that reaches both the gateway and the worker is served by
//!   the control plane.** Distributing the GATEWAY's identity key over CONTROL's
//!   feed would make control able to substitute the gateway's identity, moving a
//!   trust root onto a process the design does not put it on. A file the
//!   operator writes keeps each key's authority with the operator.
//!
//! The cost is unattended rotation, and it is real. Rotation is still
//! expressible without downtime because a bundle may carry several keys for one
//! issuer at once: publish the new public half everywhere, then swap the private
//! half, then drop the old public half.
//!
//! # The document
//!
//! JWKS-shaped, with one non-standard member:
//!
//! ```json
//! { "keys": [
//!     { "kty": "OKP", "crv": "Ed25519", "iss": "spiffe://zeroship.ai/svc/gateway",
//!       "x": "<base64url 32-byte public key>" }
//! ] }
//! ```
//!
//! `iss` is the addition, and it is not decoration: RFC 8725 section 3.8
//! requires the verification key to be resolved FROM the issuer, and a bare
//! JWKS array carries no issuer at all. A flat pool of every service's key means
//! any service's key validates any service's assertion - the Storm-0558 shape.
//!
//! `kid` is DERIVED, not carried: it is the RFC 7638 thumbprint
//! ([`crate::service_assertion::thumbprint_key_id`]) of the key itself. A
//! document MAY still state one, in which case a disagreement is an error rather
//! than a silently preferred value - an operator who edits a `kid` by hand is
//! saying something about which key this is, and the two answers must agree.
//!
//! # NO KEY IS SHARED, AND BOTH HALVES OF THAT ARE CHECKED AT STARTUP
//!
//! The per-issuer index is the security property, so a key reachable under two
//! issuers is the property's absence. Two independent refusals hold it, and
//! NEITHER SUBSUMES THE OTHER - they cut the space along different axes, and a
//! deployment can hit either one without the other:
//!
//! - [`load_peer_bundle`] refuses a DOCUMENT that publishes one public key
//!   under two issuers, whoever loads it. It is a property of the file alone,
//!   so every process that reads the file refuses, including the ones whose own
//!   key is not involved.
//! - [`ServiceKeyring::from_parts`] refuses a PAIR whose private key's public
//!   half is published under any issuer but this process's own. It is a
//!   property of the private key against the document, and no reader of the
//!   document alone can evaluate it: a document naming that key exactly once,
//!   under a foreign issuer, is well formed and duplicate-free.
//!
//! The second is what fence F4 needs and the first cannot supply. `iss` never
//! travels on an identity envelope - only a thumbprint `kid` does, derived by
//! signer and verifier alike from the public bytes - so a document filing the
//! WORKER's own key under the GATEWAY's issuer makes the worker's signer stamp
//! exactly the `kid` its own verifier resolves, with one entry and nothing
//! duplicated. The first is what the wider blast radius needs and the second
//! cannot supply: one key shared between two OTHER services is invisible to
//! every process except those two, and the assertion verifier resolves its key
//! from the issuer parsed out of the assertion it was handed, so a shared key
//! is the ability to present as either of them.
//!
//! Both refuse the BOOT, for the reason the next section gives, and neither has
//! an override. A check with a bypass flag is the shape this design replaced.
//!
//! # A missing or unparseable document REFUSES STARTUP
//!
//! Both paths are mandatory in [`ServiceKeyring::load`], and the empty string
//! each setting defaults to is refused there rather than in the three `main`s
//! that call it. Fence F4 of
//! `docs/proposals/2026-09-05-auth-foundation-redesign.md` words the rule for
//! the worker - "absent a configured gateway public key the worker refuses to
//! start" - and it holds for every binary that mints or verifies, because the
//! failure it names has nothing to do with which service is holding the
//! material: a process that boots and then refuses every guarded edge is
//! indistinguishable from a healthy one until traffic arrives.
//!
//! [`ServiceAuth::unconfigured`] is unchanged and still refuses at request
//! time. The two are not alternatives - one is loud at deploy time and the
//! other at request time - and the request-time one is now unreachable from any
//! binary.
//!
//! ONE document is handed to every service. That grants nothing extra: a
//! verified identity still has to pass `aud` equality with the identifier the
//! callee is ADDRESSED by ([`ServiceKeyring::audience`]) and the endpoint
//! allowlist in [`crate::service_identity::authorize`], so holding a peer's
//! PUBLIC key is the ability to check that peer's signature and nothing else.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;

use crate::service_assertion::{
    thumbprint_key_id, AssertionError, ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey,
    ServiceTrustBundle,
};
use crate::service_identity::{
    verify_service_call, AuthError, IdentityVerifier, ServiceEndpoint, ServiceIdentity,
};
use crate::user_envelope::{EnvelopeKeyError, UserEnvelopeSigner, UserEnvelopeVerifier};

/// The trust domain every platform service issuer sits in.
pub const SERVICE_TRUST_DOMAIN: &str = "zeroship.ai";

/// The hierarchical name of the gateway's service identity.
pub const GATEWAY_SERVICE_NAME: &str = "svc/gateway";
/// The hierarchical name of the worker's service identity.
pub const WORKER_SERVICE_NAME: &str = "svc/worker";
/// The hierarchical name of the control plane's service identity.
pub const CONTROL_SERVICE_NAME: &str = "svc/control";
/// The hierarchical name of the auth service's identity.
pub const AUTH_SERVICE_NAME: &str = "svc/auth";

/// Build the issuer identifier of a platform service by name.
///
/// # Panics
///
/// Never for the constants in this module: the names are checked by
/// `every_named_service_parses_as_an_issuer`. A caller passing an arbitrary
/// string gets [`AssertionError::MalformedIssuer`] instead.
///
/// # Errors
///
/// Returns [`AssertionError::MalformedIssuer`] when `name` is not a well-formed
/// service path.
pub fn service_issuer(name: &str) -> Result<ServiceIssuer, AssertionError> {
    ServiceIssuer::parse(&format!("spiffe://{SERVICE_TRUST_DOMAIN}/{name}"))
}

/// Failure to load service key material.
///
/// Distinct from [`AssertionError`] and from
/// [`crate::service_identity::AuthError`]: these are provisioning faults raised
/// while a process is starting, never verdicts about a credential a peer
/// presented.
#[derive(Debug, thiserror::Error)]
pub enum PeerKeyError {
    /// The setting naming this file was left empty, so there is nothing to
    /// read.
    ///
    /// SEPARATE from [`PeerKeyError::Read`], and the separation is what makes
    /// the refusal actionable. An empty path reaches the filesystem as `""`
    /// and comes back as a not-found naming no file at all, which is the least
    /// useful sentence a boot log can carry: the operator has to be told that
    /// a SETTING is unset, not that a nameless file is absent.
    #[error("no {which} is configured, and this binary cannot run without one")]
    NotConfigured {
        /// Which of the two documents was never configured, in the operator's
        /// vocabulary.
        which: &'static str,
    },
    /// The file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The file is group- or world-accessible.
    #[error("{path} is group/world accessible (mode {mode:o}); chmod 600 it")]
    InsecurePermissions {
        /// The offending path.
        path: String,
        /// The mode as reported by the filesystem.
        mode: u32,
    },
    /// The peer document did not parse, or an entry was malformed.
    #[error("service peer document {path}: {reason}")]
    Document {
        /// The offending path.
        path: String,
        /// What was wrong with it.
        reason: String,
    },
    /// One public key is published under two different issuers.
    ///
    /// Refused for whoever loads the document, without reference to which
    /// service is loading it: two issuers resolving to one key means possession
    /// of that single private half is the ability to present as either of them,
    /// and possession is not something a reader of the document can check.
    #[error(
        "service peer document {path} publishes the same public key under {first_issuer} and \
         {second_issuer}: whoever holds that one private key can present as both. Give each \
         issuer a key of its own."
    )]
    KeyUnderTwoIssuers {
        /// The offending path.
        path: String,
        /// The issuer the key was first published under.
        first_issuer: String,
        /// The issuer that republished it.
        second_issuer: String,
    },
    /// This process's own public key is published under some other issuer.
    ///
    /// SEPARATE from [`PeerKeyError::KeyUnderTwoIssuers`] because it is a fact
    /// about the PAIR - the private key this process holds and the document it
    /// was given - rather than about the document, and a document naming this
    /// key exactly once is well formed on its own terms.
    #[error(
        "this process mints as {own_issuer}, but {document} publishes its own public key under \
         {foreign_issuer}: every peer holding that document would accept this process's \
         signature as {foreign_issuer}. Publish this key under {own_issuer} only, and give \
         {foreign_issuer} a key of its own."
    )]
    OwnKeyUnderForeignIssuer {
        /// The issuer this process mints under.
        own_issuer: String,
        /// The issuer the same key is also published under.
        foreign_issuer: String,
        /// Where the two halves came from, in the operator's terms.
        ///
        /// NOT named `source`: `thiserror` reads a field of that name as the
        /// error's cause and requires it to implement `Error`, which a
        /// provenance string is not.
        document: String,
    },
    /// The key material was rejected by the assertion mechanism.
    #[error("service key material: {0}")]
    Material(#[from] AssertionError),
    /// The key material was rejected by the identity-envelope mechanism.
    #[error("identity envelope key material: {0}")]
    Envelope(#[from] EnvelopeKeyError),
}

impl PeerKeyError {
    /// Name the two files the material came from, once they are known.
    ///
    /// [`ServiceKeyring::from_parts`] holds material rather than paths, so the
    /// refusal it raises can only describe the bundle generically.
    /// [`ServiceKeyring::load`] knows both real paths and substitutes them here,
    /// which is why the check itself does not move to `load`: putting it where
    /// the paths are would leave `from_parts` - a public constructor - as the
    /// way around it.
    fn naming_document(self, key_path: &Path, peers_path: &Path) -> Self {
        match self {
            Self::OwnKeyUnderForeignIssuer {
                own_issuer,
                foreign_issuer,
                ..
            } => Self::OwnKeyUnderForeignIssuer {
                own_issuer,
                foreign_issuer,
                document: format!(
                    "{} (read against the private key in {})",
                    peers_path.display(),
                    key_path.display()
                ),
            },
            other => other,
        }
    }
}

/// One published peer key.
#[derive(Debug, Deserialize)]
struct PeerKeyEntry {
    #[serde(default)]
    kty: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    iss: String,
    x: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PeerKeyDocument {
    keys: Vec<PeerKeyEntry>,
}

/// Everything one process needs to speak and verify service assertions.
///
/// Held whole rather than as two loose values so a service cannot end up able
/// to verify its peers but unable to name itself, or the reverse - which is the
/// shape that produces a hop authenticated in one direction only.
#[derive(Debug)]
pub struct ServiceKeyring {
    issuer: ServiceIssuer,
    audience: ServiceIssuer,
    minter: ServiceAssertionMinter,
    envelope: UserEnvelopeSigner,
    bundle: Option<ServiceTrustBundle>,
}

impl ServiceKeyring {
    /// Load this service's private key and the peer bundle from two files.
    ///
    /// # An unconfigured path is a REFUSAL, not a mode
    ///
    /// Both paths are required, and the empty path each setting defaults to is
    /// refused here rather than handled by the caller. That is fence F4 of
    /// `docs/proposals/2026-09-05-auth-foundation-redesign.md`, and the reason
    /// it lives in the loader is that the alternative shape is the defect the
    /// fence exists to remove: every `main` that reads two paths would need its
    /// own "neither was configured" branch, and the natural body of that branch
    /// is to carry on with no key material. A process that starts and then
    /// refuses every guarded edge looks healthy to an orchestrator, answers a
    /// liveness probe, and fails only where an end user sees it.
    ///
    /// [`ServiceAuth::unconfigured`] still refuses at request time. That is the
    /// SECOND fence, not this one, and no binary reaches it: the difference is
    /// that this one is loud at deploy time, when someone is watching.
    ///
    /// [`ServiceKeyring::from_parts`] takes material already in memory and so
    /// has no path to judge. It is the test and generator door, and it is the
    /// one way to a keyring that does not pass through this refusal.
    ///
    /// # Errors
    ///
    /// Returns [`PeerKeyError::NotConfigured`] when either path is empty, and
    /// [`PeerKeyError`] otherwise when either file is unreadable, insecurely
    /// permissioned, or does not hold what it claims to.
    pub fn load(
        issuer: ServiceIssuer,
        key_path: &Path,
        peers_path: &Path,
    ) -> Result<Self, PeerKeyError> {
        require_configured(key_path, "service key file")?;
        require_configured(peers_path, "service peer document")?;
        Self::from_parts(
            issuer,
            load_signing_key(key_path)?,
            load_peer_bundle(peers_path)?,
        )
        .map_err(|error| error.naming_document(key_path, peers_path))
    }

    /// Build a keyring from material already in memory.
    ///
    /// The one place the `kid` is derived, so [`ServiceKeyring::load`] and a
    /// test fixture cannot end up minting under different ones. A caller
    /// holding a key it did not read from disk is a TEST or a generator; the
    /// production path is `load`, which is what applies the permission refusal.
    ///
    /// # This service's own key may appear under its OWN issuer and no other
    ///
    /// The one place that holds both the private key and the bundle, so the one
    /// place that can compare them - and until it did, nothing in this stack
    /// ever did. Neither loader compared the two, the envelope wire format
    /// carries a thumbprint `kid` and no issuer, and both the signer and the
    /// verifier derive that `kid` from the public bytes. A document publishing
    /// this process's own public half under the GATEWAY's issuer therefore made
    /// this process's signer stamp exactly the `kid` its own verifier resolves,
    /// and fence F4 of `docs/proposals/2026-09-05-auth-foundation-redesign.md` -
    /// "a worker must not be able to mint an envelope it would then accept" -
    /// became a configuration choice.
    ///
    /// This is NOT the same check as the document-only one in
    /// [`load_peer_bundle`], and neither subsumes the other. A document naming
    /// this key once, under a foreign issuer, has nothing duplicated in it and
    /// passes there; a document sharing one key between two issuers neither of
    /// which is this process's passes here. The pair is what closes the shape.
    ///
    /// # Errors
    ///
    /// Returns [`PeerKeyError::OwnKeyUnderForeignIssuer`] when the bundle
    /// publishes this key under any issuer other than `issuer`, and
    /// [`PeerKeyError::Material`] when the key cannot be encoded for signing.
    pub fn from_parts(
        issuer: ServiceIssuer,
        signing_key: ServiceSigningKey,
        bundle: ServiceTrustBundle,
    ) -> Result<Self, PeerKeyError> {
        // Derived from the PRIVATE half, which is what makes this a comparison
        // of the two documents rather than of the bundle against itself.
        let own_public = signing_key.verifying_key_bytes();
        if let Some(foreign) = bundle
            .issuers_publishing(&own_public)
            .into_iter()
            .find(|candidate| *candidate != issuer.as_str())
        {
            return Err(PeerKeyError::OwnKeyUnderForeignIssuer {
                own_issuer: issuer.as_str().to_owned(),
                foreign_issuer: foreign.to_owned(),
                document: "the peer trust bundle handed to this process".to_owned(),
            });
        }
        let minter =
            ServiceAssertionMinter::new(issuer.clone(), signing_key.key_id(), &signing_key)?;
        // The key is MOVED in rather than borrowed, because this process signs
        // two different things with it: service assertions (the minter) and, at
        // the gateway, the `ZeroShip-User` identity envelope. Loading the file
        // twice to get two owners is how the two would drift onto different
        // key material after a rotation.
        let envelope = UserEnvelopeSigner::new(signing_key)?;
        Ok(Self {
            audience: issuer.clone(),
            issuer,
            minter,
            envelope,
            bundle: Some(bundle),
        })
    }

    /// The issuer identifier this service MINTS under.
    ///
    /// Not necessarily the identifier it is ADDRESSED by; that is
    /// [`ServiceKeyring::audience`], and the two were one value until a worker
    /// instance needed a name of its own.
    #[must_use]
    pub const fn issuer(&self) -> &ServiceIssuer {
        &self.issuer
    }

    /// The identifier a caller must put in `aud` to reach this service.
    ///
    /// Equal to [`ServiceKeyring::issuer`] for every process whose identity IS
    /// its role name, which is why a keyring that says nothing gets that. They
    /// come apart where a process mints under a FINER name than its callers
    /// hold: a worker instance mints as `svc/worker/<instance>` so its outbound
    /// calls are attributable to one process, while the gateway dispatches over
    /// a hash ring holding only the role name `svc/worker`. Requiring the
    /// minting name as the audience would refuse every caller.
    ///
    /// It is one identifier compared for equality, never a set: separating the
    /// two decides WHICH name is admitted, not how many.
    #[must_use]
    pub const fn audience(&self) -> &ServiceIssuer {
        &self.audience
    }

    /// Declare the identifier callers address this service as, when it is not
    /// the one it mints under.
    ///
    /// The whole of what "per-instance identity" costs the inbound side. A
    /// process that does not call this requires its own issuer, so no existing
    /// service changes behaviour by the separation existing.
    #[must_use]
    pub fn addressed_as(mut self, audience: ServiceIssuer) -> Self {
        self.audience = audience;
        self
    }

    /// Mint one assertion naming `audience`, valid from now.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::Signing`] when the JWT cannot be signed.
    pub fn mint_for(&self, audience: &ServiceIssuer) -> Result<String, AssertionError> {
        self.minter.mint(audience)
    }

    /// This service's signer for `ZeroShip-User` identity envelopes.
    ///
    /// Every keyring can build one, and only the GATEWAY's envelopes are
    /// trusted anywhere: a callee's [`UserEnvelopeVerifier`] is built for the
    /// gateway issuer alone, so an envelope another service signed resolves to
    /// no key and is refused. Withholding the signer here would look like a
    /// second fence and would not be one - the fence is on the verifying side,
    /// where it can be checked.
    #[must_use]
    pub const fn user_envelope_signer(&self) -> &UserEnvelopeSigner {
        &self.envelope
    }

    /// Take the peer trust bundle, leaving the minter behind.
    ///
    /// A verifier CONSUMES its bundle, and a process builds exactly one
    /// verifier, so this hands the bundle over once. A second call returns
    /// `None` rather than an empty bundle: an empty bundle trusts nobody and
    /// would look like a working verifier that refuses every caller, which is
    /// the failure that is hardest to read in a log.
    #[must_use]
    pub fn take_bundle(&mut self) -> Option<ServiceTrustBundle> {
        self.bundle.take()
    }
}

/// One process's whole service-assertion capability: mint outbound, verify
/// inbound, or neither.
///
/// Held by each service's shared state so a handler asks one object rather than
/// assembling an issuer, a verifier and an endpoint at each call site. The
/// verifier is type-erased because the PROFILE differs per service - the worker
/// takes the transport-only one on its dispatch hop, control and the gateway
/// take the full one on their per-app-load and per-advance edges - while the
/// guard is identical.
///
/// # Absence refuses; it does not disable
///
/// [`ServiceAuth::unconfigured`] holds no key material. Every
/// [`ServiceAuth::verify`] then returns a refusal and every
/// [`ServiceAuth::authorization_for`] returns `None`, so it serves no guarded
/// edge and reaches no guarded peer. That is the opposite of the shared secrets
/// this replaces, whose empty value turned the check OFF - and it is the whole
/// reason the unconfigured state is a named constructor rather than two
/// `Option` fields a call site might forget to check.
///
/// **NO BINARY REACHES IT.** It described what a `main` held when neither key
/// file was configured until fence F4 landed the startup refusal in
/// [`ServiceKeyring::load`]; a process with no key material now exits instead
/// of booting into this state. What survives is the request-time half, which is
/// still worth having and is still exercised - the gateway's advance edge and
/// the worker's dispatch edge each have a test that hands them exactly this
/// value and requires a refusal, so the two fences are independent rather than
/// one resting on the other.
pub struct ServiceAuth {
    keyring: Option<ServiceKeyring>,
    verifier: Option<Arc<dyn IdentityVerifier + Send + Sync>>,
    user_envelope: Option<UserEnvelopeVerifier>,
}

impl fmt::Debug for ServiceAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceAuth")
            .field(
                "issuer",
                &self.keyring.as_ref().map(|k| k.issuer().as_str()),
            )
            .field(
                "audience",
                &self.keyring.as_ref().map(|k| k.audience().as_str()),
            )
            .field("can_verify", &self.verifier.is_some())
            .field("user_envelope", &self.user_envelope)
            .finish()
    }
}

impl ServiceAuth {
    /// Build a capability from a loaded keyring and the verifier for its tier.
    #[must_use]
    pub fn new(
        keyring: ServiceKeyring,
        verifier: Arc<dyn IdentityVerifier + Send + Sync>,
    ) -> Self {
        Self {
            keyring: Some(keyring),
            verifier: Some(verifier),
            user_envelope: None,
        }
    }

    /// Declare that this process also verifies `ZeroShip-User` identity
    /// envelopes issued by whichever service `verifier` was built for.
    ///
    /// Opt-in per process, and taken only by the WORKER. A callee that does not
    /// declare it refuses every envelope, which is the right answer for a
    /// service no identity is forwarded to: a verifier nobody asked for would
    /// be an accepting path nobody reviewed.
    #[must_use]
    pub fn verifying_user_envelopes(mut self, verifier: UserEnvelopeVerifier) -> Self {
        self.user_envelope = Some(verifier);
        self
    }

    /// The state of a process that was given no service key material.
    #[must_use]
    pub const fn unconfigured() -> Self {
        Self {
            keyring: None,
            verifier: None,
            user_envelope: None,
        }
    }

    /// Whether this process can mint and verify at all.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.keyring.is_some() && self.verifier.is_some()
    }

    /// The `Authorization` header value naming `audience`, or `None` when this
    /// process holds no key.
    ///
    /// Returning the whole header value rather than the bare assertion keeps
    /// the `Bearer ` prefix in ONE place; three call sites spelling it
    /// themselves is three chances to send a header the peer's extractor drops.
    #[must_use]
    pub fn authorization_for(&self, audience: &ServiceIssuer) -> Option<String> {
        let keyring = self.keyring.as_ref()?;
        match keyring.mint_for(audience) {
            Ok(assertion) => Some(format!("Bearer {assertion}")),
            Err(error) => {
                tracing::error!(%error, audience = audience.as_str(), "service assertion mint failed");
                None
            }
        }
    }

    /// This process's signer for `ZeroShip-User` identity envelopes, or `None`
    /// when it holds no key.
    ///
    /// `None` means EMIT NOTHING, never emit unsigned. A caller that cannot get
    /// a signer must fail the request rather than forward an identity the
    /// callee has no way to check - which is the same rule as
    /// [`ServiceAuth::authorization_for`], for the same reason.
    #[must_use]
    pub fn user_envelope_signer(&self) -> Option<&UserEnvelopeSigner> {
        self.keyring.as_ref().map(ServiceKeyring::user_envelope_signer)
    }

    /// The verifier for inbound `ZeroShip-User` identity envelopes, or `None`
    /// when this process was not configured to accept any.
    #[must_use]
    pub const fn user_envelope_verifier(&self) -> Option<&UserEnvelopeVerifier> {
        self.user_envelope.as_ref()
    }

    /// Verify an inbound caller's credential and its grant on `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] exactly as [`verify_service_call`] does, and
    /// [`AuthError::CredentialRejected`] when this process holds no key
    /// material at all.
    pub async fn verify(
        &self,
        authorization: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<ServiceIdentity, AuthError> {
        let (Some(keyring), Some(verifier)) = (self.keyring.as_ref(), self.verifier.as_ref())
        else {
            // Loud for the operator, opaque to the caller. An unconfigured
            // process refuses every caller identically to a bad credential, so
            // the response is not an oracle for whether the deployment has
            // keys; the log line names the missing configuration because that
            // is the only thing an operator can act on.
            tracing::error!(
                destination = endpoint.destination(),
                path = endpoint.path_template(),
                "refusing an internal call: no service key material is configured \
                 (set the service key and peer files for this binary)"
            );
            return Err(AuthError::CredentialRejected);
        };
        // The AUDIENCE, not the issuer. A process requires callers to address
        // the name they hold for it, which is not always the name it mints
        // under - see [`ServiceKeyring::audience`].
        verify_service_call(
            verifier.as_ref(),
            authorization,
            keyring.audience().as_str(),
            endpoint,
        )
        .await
    }
}

/// Load an ed25519 signing key from a PKCS#8 PEM or DER file.
///
/// The format is sniffed on the PEM armor, exactly as the gateway's
/// session-cookie key loader does, so one `openssl genpkey -algorithm ed25519`
/// recipe produces every key in this stack.
///
/// # Errors
///
/// Returns [`PeerKeyError`] when the file cannot be read, is group- or
/// world-accessible, or does not hold an ed25519 private key.
pub fn load_signing_key(path: &Path) -> Result<ServiceSigningKey, PeerKeyError> {
    let bytes = read_file(path)?;
    reject_insecure_permissions(path)?;
    if let Ok(text) = std::str::from_utf8(&bytes) {
        if text.contains("-----BEGIN PRIVATE KEY-----") {
            let der = pem_body(text).ok_or_else(|| PeerKeyError::Document {
                path: path.display().to_string(),
                reason: "PEM armor present but the body did not decode".to_owned(),
            })?;
            return Ok(ServiceSigningKey::from_pkcs8_der(&der)?);
        }
    }
    Ok(ServiceSigningKey::from_pkcs8_der(&bytes)?)
}

/// Load the peer trust bundle from a JWKS-shaped document.
///
/// # ONE KEY, ONE ISSUER
///
/// A document that publishes the same public key under two issuers is refused
/// here, whoever is loading it and whichever two issuers they are. The
/// per-issuer index in [`ServiceTrustBundle`] exists so a key resolves for one
/// issuer and no other; two entries carrying one key put that property back in
/// the operator's hands, and there is nothing in a document that says who holds
/// the matching private half. `ServiceTrustBundle::trust` cannot see this - its
/// duplicate check is scoped PER ISSUER by construction, so a second issuer is
/// a fresh, empty entry every time.
///
/// The realistic producer is not a hand-edited file. `SERVICE_KEY_FILES` in
/// `crates/zeroship-cli/src/dev.rs` states the rule in its own rustdoc - four
/// keys, not one shared file - and a secret manager or compose override mapping
/// one secret onto the four `*_SERVICE_KEY_FILE` mounts satisfies every check
/// the generator makes. The document it then publishes has one key under all
/// four issuers, which is a shared bearer secret with no shared secret visible
/// to notice: possession of any one private half becomes the ability to present
/// as every service, because the assertion verifier resolves its key from the
/// issuer parsed out of the assertion it was handed.
///
/// The same key repeated under the SAME issuer is not this, and still loads:
/// that is idempotent re-publication, not a second identity.
///
/// # Errors
///
/// Returns [`PeerKeyError`] when the file cannot be read, does not parse, holds
/// an entry that is not an ed25519 public key, states a `kid` that disagrees
/// with the key's own thumbprint, or publishes one key under two issuers.
pub fn load_peer_bundle(path: &Path) -> Result<ServiceTrustBundle, PeerKeyError> {
    let bytes = read_file(path)?;
    let document: PeerKeyDocument =
        serde_json::from_slice(&bytes).map_err(|error| PeerKeyError::Document {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    let fault = |reason: String| PeerKeyError::Document {
        path: path.display().to_string(),
        reason,
    };
    if document.keys.is_empty() {
        return Err(fault("no keys".to_owned()));
    }
    let mut bundle = ServiceTrustBundle::new();
    // Which issuer first published each key. Kept beside the bundle rather than
    // asked of it afterwards so the refusal can name the issuer that CLAIMED
    // the key first, which is the one an operator has to decide about.
    let mut first_issuer_of: BTreeMap<[u8; 32], String> = BTreeMap::new();
    for entry in &document.keys {
        if let Some(kty) = entry.kty.as_deref() {
            if kty != "OKP" {
                return Err(fault(format!("{}: kty is {kty}, not OKP", entry.iss)));
            }
        }
        if let Some(crv) = entry.crv.as_deref() {
            if crv != "Ed25519" {
                return Err(fault(format!("{}: crv is {crv}, not Ed25519", entry.iss)));
            }
        }
        let issuer = ServiceIssuer::parse(&entry.iss)
            .map_err(|_| fault(format!("{} is not an issuer identifier", entry.iss)))?;
        let raw = URL_SAFE_NO_PAD
            .decode(entry.x.as_bytes())
            .map_err(|error| fault(format!("{}: x is not base64url: {error}", entry.iss)))?;
        let public: [u8; 32] = raw
            .try_into()
            .map_err(|_| fault(format!("{}: x is not 32 bytes", entry.iss)))?;
        let key_id = thumbprint_key_id(&public);
        if let Some(stated) = entry.kid.as_deref() {
            if stated != key_id {
                return Err(fault(format!(
                    "{}: stated kid does not match the key's own thumbprint",
                    entry.iss
                )));
            }
        }
        match first_issuer_of.get(&public) {
            Some(first) if first != issuer.as_str() => {
                return Err(PeerKeyError::KeyUnderTwoIssuers {
                    path: path.display().to_string(),
                    first_issuer: first.clone(),
                    second_issuer: issuer.as_str().to_owned(),
                });
            }
            Some(_) => {}
            None => {
                first_issuer_of.insert(public, issuer.as_str().to_owned());
            }
        }
        bundle.trust(&issuer, key_id, public)?;
    }
    Ok(bundle)
}

/// Refuse a path whose setting was never filled in.
///
/// Takes `which` as a `&'static str` rather than deriving it from the issuer:
/// the issuer is a security identity and the trust domain travels with the name
/// for a reason, so spelling a config word out of it would couple the operator
/// vocabulary to the SPIFFE path and read a name out of its scope to do it.
fn require_configured(path: &Path, which: &'static str) -> Result<(), PeerKeyError> {
    if path.as_os_str().is_empty() {
        return Err(PeerKeyError::NotConfigured { which });
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>, PeerKeyError> {
    std::fs::read(path).map_err(|source| PeerKeyError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// Decode the base64 body of a single-block PKCS#8 PEM file.
fn pem_body(text: &str) -> Option<Vec<u8>> {
    let body: String = text
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN PRIVATE KEY-----"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END PRIVATE KEY-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .ok()
}

/// Refuse a private key any other local user can read.
///
/// The peer document is deliberately NOT subject to this: it holds public keys
/// and an operator may well want it world-readable. Applying a secret's
/// permission rule to a non-secret is how a check stops being believed.
#[cfg(unix)]
fn reject_insecure_permissions(path: &Path) -> Result<(), PeerKeyError> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(path).map_err(|source| PeerKeyError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(PeerKeyError::InsecurePermissions {
            path: path.display().to_string(),
            mode: mode & 0o777,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path) -> Result<(), PeerKeyError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_service_parses_as_an_issuer() {
        let names = [
            GATEWAY_SERVICE_NAME,
            WORKER_SERVICE_NAME,
            CONTROL_SERVICE_NAME,
            AUTH_SERVICE_NAME,
        ];
        // The floor is the whole constant list, not a sample: a name added
        // without an issuer that parses would be an edge nobody can address.
        assert_eq!(names.len(), 4);
        for name in names {
            let issuer = service_issuer(name).expect("named service issuer parses");
            assert_eq!(issuer.as_str(), format!("spiffe://zeroship.ai/{name}"));
        }
    }

    #[test]
    fn a_thumbprint_kid_round_trips_between_the_minter_and_the_bundle() {
        let key = ServiceSigningKey::generate();
        let public = key.verifying_key_bytes();
        assert_eq!(key.key_id(), thumbprint_key_id(&public));
    }
}
