# Service identity: replacing shared bearer secrets with a pluggable authenticator

**Date:** 2026-08-16
**Status:** FINAL. Section 9 is the authoritative implementation plan and is self-contained. Sections 1-8 record how the decisions were reached, including three drafts the investigations falsified; 5.1, 6.3.1, 6.5.1 and 6.6 carry corrections to the text above them.
**Scope:** authentication and authorization BETWEEN platform services. Explicitly
NOT: end-user auth, creator login, or CLI token issuance - those belong to
`docs/proposals/2026-08-16-cli-token-issuance.md`, whose section 7.5 this
document expands.

---

## 0. Reading conventions

Every claim about current behaviour carries a `file:line`. Claims are labelled:

- **VERIFIED** - I read the cited lines in this working tree at `adc8be620`.
- **INFERRED** - a conclusion drawn from verified facts, not itself read.
- **NOT CHECKED** - stated so the reader does not mistake silence for evidence.

An empty grep is reported as an empty grep, never as proof of absence.

---

## 1. Decision summary

| # | Question | Answer |
| --- | --- | --- |
| Q1 | Is a shared bearer secret adequate for service-to-service auth? | **No** - it fails all four properties in 6.1 |
| Q2 | One mechanism, or several? | **ONE shipped implementation behind a trait.** The trait is a seam, not a mode. No mechanism-selecting config key. Revised by I1; see 6.5.1 |
| Q3 | What is the default? | **RFC 7523 signed JWT assertion** - the only option that is strong AND needs no infrastructure |
| Q4 | mTLS or RFC 7523? | **7523.** mTLS added as a code change if someone needs it, not maintained speculatively. Reasoning in 6.3, corrections in 6.3.1 |
| Q5 | Is attested identity (SPIFFE) required? | No. Added when a deployer's environment can attest; the trait makes it a later contribution |
| Q6 | Is there a dev-only weak mode? | **No.** `SharedSecret` is deleted entirely, not fenced. Dev gets generated keypairs. See 6.5.1 |
| Q7 | What are the authorization principals? | **Individual services**, not classes. Classes organize policy only. Corrected by I3; see 5.1 |

The forcing constraint is that **zeroship will be open source and self-hosted
into unknown environments**. A default that imposes infrastructure (a CA, a
SPIRE cluster, a service mesh) is not deployable by a user on a single VPS.

---

## 2. What exists today (VERIFIED)

### 2.1 Three shared secrets, by service

From `deploy/compose/docker-compose.yml`:

| secret | services holding it | count |
| --- | --- | --- |
| `ZEROSHIP_CONTROL_KEY` | control, gateway, migrated, worker | 4 |
| `ZEROSHIP_WORKER_KEY` | control, gateway, worker | 3 |
| `ZEROSHIP_AUTH_PLATFORM_MINT_KEY` | auth, control | 2 |

The mint key is 2 because it was split out earlier today (merge `7de47b7f6`);
auth no longer needs `control_key` as a result. That is the shape the rest of
this document generalises.

### 2.2 What `control_key` authorizes

`crates/control/src/internal.rs` exposes, all behind the same credential:

```
healthz, readyz              harmless
get_routes                   what the gateway actually needs
get_app_env                  DECRYPTED app environment data
get_versions, get_app_version
force_reconcile              billing operation
force_spend_reconcile        billing operation
```

**INFERRED:** the gateway needs `get_routes` and holds a credential that also
opens decrypted creator secrets and billing reconciliation. There is no
per-caller authorization; possession is the whole decision.

### 2.3 It travels in cleartext

`crates/gateway/src/sync.rs:209-260` - route sync rejects schemes other than
`http`, states in its own comment that the key travels in clear, and writes the
bearer into a hand-built TCP request. (VERIFIED by the auth-flows review; I read
the finding, NOT the lines myself.)

### 2.4 What is already the right shape

Two mechanisms in the tree already do this correctly and are the model:

- **App-scoped derivation.** `derive_app_scoped_control_token(control_key, app_id)`
  = `HMAC-SHA256(control_key, app_id)` (`crates/core/src/auth/mod.rs:165`). The
  raw key "must never enter V8" (`:160`); the runtime derives a token good for
  one app only. This is a capability, not ambient authority.
- **Signed user assertion.** The gateway does not tell the worker who the end
  user is - it HMAC-signs a request-bound `ZeroShip-User` header which the worker
  verifies, and dispatch strips inbound copies as platform-reserved
  (`crates/gateway/src/router/dispatch.rs:2425`, `:2537`).

### 2.5 Pluggable-backend precedent

The codebase already solves "one interface, environment-specific implementations"
four times: `BlobStore` (`crates/bundle/src/blob.rs:55`), `StreamTransport`
(`crates/stream/src/transport.rs:38`), `ConfiguredProvider`
(`crates/core/src/auth_provider/mod.rs:40`), and `env.db` / `env.kv` dev tiers.

### 2.6 Dev-only-by-construction precedent

`docs/reference/auth-dev-tier.md:278` documents how a dev-only implementation is
prevented from reaching production **structurally** rather than by
configuration: separate module, never imported by the production entry,
deliberately not re-exported from the barrel, runtime hook inert without
`ZEROSHIP_DEV=1`, and **grep-provable with a test asserting the production
artifacts carry none of its symbols**.

---

## 3. The defect

A shared bearer secret makes authority a property of the **environment**
(who happens to hold a string) rather than of a **designated request** (this
service, calling this endpoint, now). That is ambient authority, and it is what
makes a confused deputy possible - the pattern already demonstrated at the
platform mint earlier today.

Concretely, today:

- **No identity.** Every caller presents the same string, so "who called this?"
  is unanswerable in an audit.
- **No least privilege.** The gateway can read decrypted app environments.
- **No confidentiality.** The credential crosses the network in clear.
- **Rotation is global and all-or-nothing.** At the target topology (dozens of
  points of presence) a rotation is an outage risk and a floor on incident
  response time.
- **Revocation is impossible per-node.** A compromised point of presence cannot
  be cut off without cutting off everything.

---

## 4. What the target deployment changes

**Dozens of points of presence, not running the whole stack** (operator, this
session). Edge runs gateway and worker; core runs auth, control, Postgres.

This kills the perimeter defence for shared secrets: dozens of perimeters is
none. It also makes the edge/core split the natural trust boundary - **the edge
only ever validates, and never signs.** Signing keys stay in core.

---

## 5. Identity classes

Three, derived from 4:

```
core     auth, control, migrated   may reach internal control and auth endpoints
edge     gateway                   get_routes, healthz. NOT get_app_env,
                                   NOT force_*_reconcile
worker   worker                    app-scoped only: its OWN app's env, its OWN
                                   workflow advance - because it executes
                                   untrusted creator code
```

A compromised point of presence must yield edge authority and nothing more.

### 5.1 CORRECTED BY I3: classes organize policy, they do not replace identity

The three classes above are **not sufficient as authorization principals**. I3's
finding, and it is decisive:

> Class membership should organize policy, not replace individual caller
> identity.

A shared `core` identity would let auth or migrated inherit control's authority
- which is the same ambient-authority defect one level up. The allowlist keys on
the individual service; the class is only a way to reason about it.

### 5.2 The measured allowlist (I3, VERIFIED calls / INFERRED minimums)

| identity | exact outbound grants |
| --- | --- |
| `core/control` | auth `POST /internal/platform-token`; migrated `POST /v1/apps/{app}/migrations/apply` with delegated creator `AppsDeploy`; plus worker calls |
| `core/auth` | gateway `POST /oidc/backchannel-logout`, or registered third-party BCL URIs using signed logout JWTs |
| `core/migrated` | **No credentialed platform HTTP endpoint at all** - public auth JWKS only |
| `edge/gateway` | control `GET /internal/routes`; control `POST /internal/workflows/signals/ingress`; worker `POST /dispatch/{app}`; worker workflow routes |
| `worker` | control `GET /internal/versions` (the one current global exception); control `GET /internal/apps/{app}`; app-scoped app/env reads |
| no machine authority | JWKS GETs; the gateway's auth-host reverse proxy to public/UI/OIDC routes |
| creator identity, not machine | every CLI call; control to migrated delegated creator authorization |

Two immediate consequences: **`core/migrated` needs no credential** - its
current `ZEROSHIP_CONTROL_KEY` is unused (finding 8) - and the worker's only
non-app-scoped need is `GET /internal/versions`.

### 5.3 Findings that change the design (I3, ranked)

1. **The worker's PostgreSQL superuser DSN defeats any HTTP allowlist.**
   Compose gives the worker a cluster superuser credential. No amount of
   endpoint authorization matters while it can write the tables directly. This
   independently confirms the HIGH finding the phase-0 reviewer raised, and it
   means the DB role work is a PREREQUISITE for S1, not a parallel track.
2. **The worker needs no broad control-plane credential in the target design** -
   but deleting `ZEROSHIP_CONTROL_KEY` from it TODAY breaks live paths. Its
   bytes are currently load-bearing for three raw bearer GETs, for deriving every
   app workflow bearer (`crates/plugin-workflow/src/lib.rs:74`), and for the
   runtime output-read token (`crates/worker/src/handler.rs:514`). Replacements
   must land first.
3. **Control to gateway workflow advance is UNAUTHENTICATED.**
4. **The gateway's auth-host reverse proxy forwards every path, including
   `/internal/platform-token`.** Auth still checks the mint key so it is not
   exploitable today, but the internal mint is reachable through a public
   surface - exactly the shape that turns a future gateway bug into a mint bug.
5. **`get_app_env` is any-app.** The app UUID comes entirely from the
   caller-controlled URL and the global bearer carries no app identity.
   Confirms 2.2.
6. **Several required links cannot currently be encrypted.** Gateway route sync
   actively permits only HTTP; **the PostgreSQL pools cannot enable TLS at all**;
   Redis likewise. So "TLS everywhere" (S2) has a driver-level prerequisite in
   `compio-postgres` and `compio-redis`, not just a configuration change.
7. **Migrated's authority is excessive** - an unused `ZEROSHIP_CONTROL_KEY` plus
   the broad `zeroship_control` database role.

---

## 6. How the mechanism decisions were reached (6.3.1, 6.5.1 and 6.6 carry the corrections)

### 6.1 The four properties

A mechanism is adequate to the extent it has all four. A shared bearer secret
has none:

1. **Identity, not possession** - the credential says WHICH service.
2. **Short-lived and auto-renewed** - rotation is continuous, not an event.
3. **Audience-bound** - a credential for A cannot be replayed at B.
4. **Mutual** - the caller verifies the callee too.

### 6.2 The layering, which is why "just pick one" is ambiguous

```
LAYER 3  who gets a credential, and how     SPIFFE/SPIRE, cloud workload identity
         (attestation, rotation, bootstrap)
             |
             v
LAYER 2  how identity is presented          X.509 cert (mTLS)  OR  signed JWT (7523)
         (pick exactly ONE per deployment)
             |
             v
LAYER 1  how the bytes are protected        TLS - always required
```

SPIFFE is not an alternative to mTLS: SPIRE issues SVIDs in *either* format. It
answers a question the other two leave open - where the credential came from and
who decided the workload deserved one.

### 6.3 Why RFC 7523 is the default, not mTLS

Ranked, with the structural reasons first:

1. **It survives TLS termination.** mTLS binds identity to the connection; a
   terminating load balancer in front of a core region destroys it, and the
   usual workaround (forward it in a header) is a caller-asserted claim - the
   exact shape removed from the mint today.
2. **Zero infrastructure.** Decisive for OSS: a CA is not something every
   self-deployer can operate. 7523 needs a keypair per service.
3. **It reuses machinery already owned** - `JwksCache`
   (`crates/core/src/oidc_verify.rs`), JWT signing/verification, JWKS
   publication, and key rotation with a retirement horizon (merged `adc8be620`).
4. Cert expiry is the classic mTLS outage and takes out a fleet; a 60-second
   assertion minted per call fails one request.
5. JWTs are inspectable; TLS handshake failures are opaque.

**Where mTLS genuinely wins**, stated so this is not one-sided: it rejects at the
transport layer rather than accepting the connection first; it bundles
encryption with identity; it is already present for anyone running a service
mesh; and it has **no replay window**.

**7523's real weakness:** a captured assertion is replayable within its `exp`.
Mitigations - short `exp` (60s), a `jti` nonce cache at the callee, and TLS
preventing capture - are MANDATORY, not optional. An implementation without them
is weaker than mTLS, not equivalent.

### 6.3.1 What I2 corrected (REPORTED)

Three specifics in the earlier draft of this document were wrong. All are
VERIFIED by I2 against primary sources
(`scratchpad/research-7523-abstraction.md`, 41 KB).

**(a) `aud` must be the ISSUER IDENTIFIER, not an endpoint URL.** The earlier
draft suggested `aud: "control/get_routes"`. `draft-ietf-oauth-rfc7523bis`
(RFC Editor Queue, March 2026) now MANDATES the issuer identifier as the sole
audience and FORBIDS token-endpoint URLs - a response to the 2025
"audience injection" attacks. Note the ecosystem has not caught up: Entra,
Okta and Google still mandate endpoint URLs, and Keycloak accepts five
different audience values. We are greenfield here and should follow the bis
rule.

**(b) `jti` is mandatory in the profile everyone actually deploys.** Bare
RFC 7523 lists `jti` as MAY. OIDC Core section 9 makes it REQUIRED and
single-use a MUST. So **"RFC 7523 compliant" by itself provides no replay
protection at all.** Skipping it is CVE-2020-15222 (Fosite, CVSS 8.1).

Keycloak is the reference implementation to copy: 60-second maximum lifetime,
15-second skew tolerance, and an atomic `putIfAbsent` into a **cluster-shared**
cache keyed with a TTL capped by the maximum expiry. No authority claims a
short `exp` alone suffices.

**Architectural consequence for us:** a `jti` cache MUST be shared across
replicas of the verifying service, or "single use" degrades to "single use per
replica". Our verifiers are CORE services (control, auth) in a few regions, not
the dozens of edge points of presence - so a per-region shared cache is
tractable, and `compio-redis` / `compio-postgres` already exist. This is the
single strongest argument for mTLS (see 6.3.2) and it must be designed, not
discovered.

**(c) The deadliest pitfall is key-to-issuer binding**, not algorithm confusion.
RFC 8725 section 3.8: resolve the verification key **from `iss`** before
verifying; never verify against a flat pool of all known service keys. A shared
pool means any service's key validates any service's assertion, collapsing
identity entirely. Storm-0558 is the canonical instance.

### 6.3.2 The strongest argument against 7523, restated

I2's steelman, sharpened: the `jti` cache reintroduces **shared,
strongly-consistent state on the authentication hot path** - an availability
AND correctness dependency that mTLS simply does not have. A system that needs
distributed shared state to be safe has arguably chosen the wrong primitive.

It still recommends 7523 for zeroship specifically, on four grounds: the
JWT/JWKS/rotation machinery already exists in production paths; there is no
internal CA; mTLS inside a bespoke no-tokio stack is a large audit surface; and
the two-type boundary in 6.6 makes the default swappable per environment
without touching authorization code.

**Counterfactual, for honesty:** for a single-company deployment already running
Istio or Linkerd, mTLS would be the right answer. The OSS multi-environment
requirement is what flips it.

### 6.4 The build order

| # | Step | Closes |
| --- | --- | --- |
| S1 | Per-caller identity + per-endpoint allowlist | identity, least privilege |
| S2 | TLS everywhere + RFC 7523 assertions | cleartext; a leaked string stops being sufficient |
| S3 | Attestation adapter (SPIFFE / cloud workload identity) | bootstrap |
| S4 | Verifiable user assertion on calls made for a user | the confused deputy |

**S1 is durable; S2 and S3 replace one function.** The seam:

```rust
authenticate(request) -> ServiceIdentity   // swappable: shared -> 7523 -> SVID
authorize(identity, endpoint) -> bool      // built ONCE, survives all of them
```

This is why starting now is not "building two versions and discarding both"
(AGENTS.md, pre-release scope): the authorization model is permanent and only
the presentation changes.

### 6.5 The adapter set

| implementation | deployment | infra the deployer must run |
| --- | --- | --- |
| `SharedSecret` | dev, single-node | none - **dev-only by construction**, per 2.6 |
| `JwtAssertion` (RFC 7523) | **default**, any self-hosted env | none beyond a keypair per service |
| `Mtls` | users with a CA or mesh | their CA |
| `Spiffe` | users running SPIRE | SPIRE |
| `CloudWorkloadIdentity` | AWS / GCP / Azure | provided |

**THE TABLE ABOVE IS SUPERSEDED. See 6.5.1.** I1 falsified both the
`SharedSecret` row and the implied selection key.

### 6.5.1 Modes versus seams - the revision I1 forced

I1's distinction resolves the tension:

> It is right about **modes**. It is wrong about **seams**.

A config key selecting among shipped mechanisms is a MODE: it multiplies threat
models, splits test coverage, and gives the weak arm somewhere to live. A Rust
trait with exactly ONE production implementation registered in the binary is
NOT a mode - it is a seam for testing and for the day someone genuinely needs
SPIFFE. Temporal reached this from the opposite direction: it rejected
runtime-loadable plugins for compile-time functional options, and its regret is
`GetClaimMapperFromConfig`'s `case "":` arm, not the `ClaimMapper` interface,
which survived a community OPA implementation intact.

| decision | choice |
| --- | --- |
| The trait | **KEEP**, with the two-stage split of 6.6 |
| A `service_auth.mode` config key | **DO NOT SHIP.** One implementation registered |
| `SharedSecret` | **DELETE ENTIRELY** - not dev-only, not present |
| `JwtAssertion` (RFC 7523) | the single shipped implementation |
| `Mtls` / `Spiffe` / cloud | a code change when someone asks, not maintained speculatively |

**Why delete `SharedSecret` rather than fence it:** I1 tested the
dev-only-by-construction claim across its whole survey and **every instance was
falsified by a deployment artifact** - Loki's safe code default is defeated by
Loki's own shipped config setting `auth_enabled: false`. Our precedent (2.6) is
stronger than most, being grep-provable and test-guarded. But the argument that
actually lands is different: **a keypair costs a developer nothing.**
`zeroship dev init` already generates secrets; a keypair is the same work. There
is no reason to own a weaker mechanism at all.

If a dev credential must exist for `pnpm dev`, make it **ephemeral and
per-process** (Woodpecker's model): it cannot survive a restart, cannot be
shared between two machines, and is self-punishing in proportion to how
production-like the deployment becomes.

**Rotation needs no mode either.** The scenario motivating Gitaly's
`transitioning` boolean - accepting old and new credentials mid-rollout - is
just **a JWKS with two entries**, keyed by `kid`. The tree already does
multi-key verification for user tokens. This is the strongest single argument
for the 7523 default.

### 6.5.2 The gate against silent weak defaults (prescriptive)

I1's answer to "how do you stop a user silently running the weak mode" is
Superset's post-CVE gate (CVE-2023-27524, CVSS 9.8). Copy near-verbatim:

- **A named sentinel constant**, e.g. `CHANGE_ME_ZEROSHIP_SERVICE_KEY`. Never a
  plausible-looking random string.
- **Empty and sentinel treated identically**, or you have rebuilt Gitaly's
  `if len(token) == 0 { return ctx, nil }`.
- **Refuse to boot**, with a banner naming the file, the key, and the exact
  remediation command.
- **The dev escape keyed to an explicit dev signal** - `zeroship dev`,
  `cfg!(debug_assertions)`, an explicit profile. NEVER "no config found, assume
  dev", and never a plain env var a production operator might set. Print the
  banner in dev too.
- **Per-subsystem gates**, so a single-VPS deployer is not blocked on the
  migration service's credential if they never enabled it.
- **Test the gate against `deploy/compose/docker-compose.yml`**, not against
  `Default::default()`. The compose file is what people copy, and it is exactly
  what defeated Loki's safe default.
- **Surface it in health output**, not only a boot log. Gitaly's mistake was
  making a Prometheus label the only signal.

### 6.5.3 Field evidence (VERIFIED in source by I1)

Both hypotheses in the brief were confirmed verbatim:

- **Gitaly's transitioning mode** exists and its entire safety mechanism is a
  Prometheus label: `if conf.Transitioning { err = nil }`. Worse, the *default*
  fails open silently - `if len(conf.GetToken()) == 0 { return ctx, nil }`, with
  `Config.Validate()` explicitly returning `nil` for an empty token. No log, no
  refusal.
- **Supabase self-hosted** ships working pre-signed JWTs in `.env.example`
  (`exp` = 2027-01-09) with no runtime detection. I1 could NOT find a named
  incident for that exact case and says so; Superset is the documented analogue.

**A direct warning about the abstraction we are building:** Temporal already
shipped this design as `ClaimMapper` / `Authorizer`. Its default returns
`RoleAdmin` for everybody, and decisively **it does not cover the internode
hop** - per the maintainer, "Internal traffic bypasses the ClaimMapper (i.e. it
always gets full admin claims)." The design owner calls it "a stop-gap
solution."

The internode hop is exactly what this proposal exists to secure. So: it must
be covered by construction, and **"no credential presented" must be a framework
decision, never delegable to an implementation.** Temporal's `nil`-claims hole
is precisely that mistake.

### 6.5.4 The case for mandatory-over-pluggable, at full strength

I1 reports this came back stronger than expected:

- Keycloak's optional `cache-embedded-mtls-enabled` **silently did nothing for
  two version lines** (CVE-2024-10973). Optional hardening is untested
  hardening.
- Kubernetes could not flip `--anonymous-auth` and built a second config
  surface to route around it; kube-apiserver took four years to delete one flag.
- Elastic's 8.0 secure-by-default flip **exempts every upgraded cluster**.
- Gitea's own comment admits it cannot remove a legacy sentinel.

**"A default is set once, at first boot."**

The price of mandatory is real too: CockroachDB has wanted `--insecure` gone for
six years and cannot remove it; kubeadm's 365-day certificates produce "I gave
up and rebuilt the cluster" outages.

**The concession the survey converges on:** if you ship an escape hatch, ship
exactly ONE, name it for what it does, make it deafening, and be honest that it
is permanent.

### 6.5.5 Posture documentation

Per-mechanism properties documented in the manner of
`docs/reference/sqlite-divergences.md` and `auth-dev-tier.md`: for each, what an
attacker who can reach the port can do, what an attacker who compromises one
point of presence can do, and what a worker sandbox escape gets. State the
threat model each does NOT cover, in the deployer's language. Posture also
belongs in `CheckConfigReport` so `zeroship config check` and `readyz` state it
plainly.

### 6.6 The trait shape - CORRECTED BY I2

The earlier draft proposed one trait, `authenticate(request) -> ServiceIdentity`,
and said the identity should carry "who, which trust domain, valid until".
**The validity field is wrong and one trait is wrong.**

I2 surveyed SPIFFE, Vault, Kubernetes TokenReview, Temporal and GCP workload
identity federation. All five converge on the same neutral shape, and **none
carries credential bytes; four of five carry NO validity at all** - validity is
a property of the credential, checked at the verification boundary. Vault is the
exception only because it MINTS a new credential, which is a different job.

Evidence that getting this wrong is expensive:

- **Envoy deprecated its own "neutral" `principal_name`** as a security bug.
- **Three Vault CVEs** came from comparing an identity name WITHOUT its scope.
- **go-spiffe deliberately refuses** a unified SVID type across X.509 and JWT.

**Recommended shape** (I2's synthesis; strongest precedent is Temporal's
mechanism-fat `AuthInfo` mapped by one pluggable `ClaimMapper` to
mechanism-thin `Claims`):

```rust
// Neutral OUTPUT - the only thing authorization code ever sees.
struct ServiceIdentity {
    trust_domain: TrustDomain,           // ALWAYS compared together with name
    name: ServiceName,                   // hierarchical: svc/control, svc/worker/3
    mechanism: MechanismTag,             // opaque tag only: "jwt-assertion" | "mtls" | ...
    attributes: BTreeMap<String, Value>, // mechanism facts land HERE, not as fields
    // NO exp / aud / jti / kid / cert chain / token string.
}

// Mechanism-fat INPUT - the transport fills in whatever it observed.
// May carry a token AND a peer certificate simultaneously.
struct PeerCredentials<'a> {
    bearer_assertion: Option<&'a str>,
    tls_peer: Option<TlsPeerInfo<'a>>,
    expected_audience: &'a str,          // verifier INPUT, never neutral output
}

trait IdentityVerifier {
    fn verify(&self, observed: &PeerCredentials) -> Result<ServiceIdentity, AuthError>;
}

trait CredentialSource {  // client side, SEPARATE trait
    fn credential_for(&self, target: &TrustDomain, audience: &str) -> Result<Credential, Error>;
}
```

Four rules that follow, each with a cited reason:

1. **Two types, not one.** A fat input the transport populates; a thin output
   authorization compiles against. The method is `map(observed) -> Identity`,
   never `identity() -> Identity`.
2. **`aud` belongs to the verifier's INPUT.** It is REQUIRED by JWT-SVID and
   NONEXISTENT in X.509. As an output field it would force the mTLS adapter to
   fabricate one.
3. **Trust domain and name are compared together, always.** This is the
   three-Vault-CVE lesson.
4. **Provisioning is a separate axis from identity semantics** - hence the
   second trait. This is the Envoy SDS lesson.

**And unify the trust-anchor store, not the credential** (from SPIFFE): one
bundle per trust domain, entries discriminated by a `use`-style tag (x509 root
versus JWT key), with a refresh hint and a monotonic sequence. That drops
directly onto the JWKS cache and retirement machinery merged today
(`adc8be620`) and generalises to mTLS roots later.

**Mechanism choice is static configuration.** No surveyed system negotiates it
on the wire.

---

## 7. Open, and deliberately not decided here

- Whether the worker needs `control_key` at all. It still holds it (2.1) while
  its legitimate needs (own app env, own workflow advance) are app-scoped.
  **NOT CHECKED** exhaustively.
- Whether `get_app_env` should become app-scoped, like the workflow token.
  INFERRED that it should; a worker able to read ANY app's environment is the
  same class of defect as the mint.
- Replay defence specifics: `jti` cache sizing, eviction, and whether it must be
  shared across a core region's replicas.
- Key distribution for S2: how a service's keypair is provisioned, and whether
  public keys ride the existing JWKS mechanism or a separate service JWKS.

---

## 8. The investigations

Three dispatched 2026-08-16. Each is required - as output, not as permission -
to argue against its own recommendation.

**I1 - how self-hostable OSS platforms solve this.** The multi-environment
constraint is the forcing one, so the question is what projects that ship to
unknown environments actually do: GitLab, Gitea, Supabase self-hosted,
Nextcloud, Grafana, Temporal, Harbor, Woodpecker, the HashiCorp stack. What is
their DEFAULT, what do they make optional, how do they prevent a user from
silently running the weak mode in production, and how do they document posture?

**I2 - RFC 7523 in depth, and the abstraction.** Exact claim set and validation
rules; replay defence in practice (`jti` caches, clock skew, `exp` selection);
key provisioning and rotation without a CA; how real systems abstract pluggable
service identity (what does Vault, Temporal, or Envoy's SDS actually expose);
and where the abstraction boundary should sit so mTLS and SPIFFE adapters are
not second-class.

**I3 - the internal inventory.** Every service-to-service call site in the tree:
caller, callee, endpoint, credential used, and the MINIMUM authority that call
needs. This is the concrete input to the S1 allowlist. Plus: does the worker
need `control_key` at all, and can `get_app_env` be app-scoped?

---

## 9. The implementation plan

**This section is self-contained and authoritative.** Sections 1 to 8 record how
these decisions were reached, including three drafts the investigations
falsified. Build from here; read those for the reasoning.

### 9.0 Two blocking prerequisites, both measured

Neither is optional and neither was in the original sketch:

| # | Prerequisite | Why it blocks | Evidence |
| --- | --- | --- | --- |
| P1 | The worker must lose its PostgreSQL superuser DSN | No HTTP allowlist means anything while the worker can write `zeroship.device_grants` and friends directly | I3 finding 1; independently found by the phase-0 reviewer |
| P2 | `compio-postgres` and `compio-redis` must support TLS | S2 is "TLS everywhere" | I3 finding 6, REFINED below |

**P1 IS CLEARED (2026-08-16, merged `b63e8e22a`).** `deploy/compose/docker-compose.yml`
now gives the worker `postgres://zeroship_worker:zeroship_worker@...`, a
dedicated least-privilege login, and the migration revokes the whole `zeroship`
schema plus future default privileges before restoring three read-only
projections. **S1 is therefore UNBLOCKED.**

**P2 IS SMALLER THAN I3 REPORTED.** I3 said the pools "cannot enable TLS at
all", which reads as absent infrastructure. VERIFIED at `ab93d1401`, what is
actually true:

- `libs/compio-postgres/src/tls.rs` already defines `TlsConnect` and
  `MakeTlsConnect`, and `connect_tls` performs the negotiation.
- `compio-tls` is already a declared dependency of `compio-postgres`.
- **`NoTls` is the only concrete implementation** (`tls.rs:85-90`), and the pool
  hardcodes it (`pool.rs:35`, `:51`).
- The pool **fails CLOSED**: `reject_unsatisfiable_tls` (`pool.rs:239`) errors on
  `sslmode=require` rather than silently downgrading to cleartext (`:244`).

So the work is: implement `MakeTlsConnect` over `compio-tls`/rustls and let the
pool accept a connector, NOT build TLS into a bespoke driver. The abstraction,
the negotiation path and the dependency are all present.

`libs/compio-redis` has NO `compio-tls` dependency, so it is the larger half.
**NOT CHECKED** whether Redis is on a cross-host hop in the target topology; if
it is core-local only, it may not be on S2's critical path at all.

**2026-08-18: the Postgres half of P2 is DONE, so the four bullets above are a
record of what was true at `ab93d1401`, not of the tree now.**
`libs/compio-postgres/src/tls_rustls.rs` implements `MakeTlsConnect` over
rustls, and the pool builds one for a `sslmode=require` URL under the `tls`
feature; trust anchors come from `sslrootcert`, client certificates from
`sslcert`/`sslkey`, and `tls-server-end-point` channel binding works. The pool
still fails closed, but on a narrower set: without the `tls` feature
`sslmode=require` is still rejected, and `sslnegotiation=direct` under
`sslmode=prefer` is rejected in every build. `sslmode=prefer` remains plaintext
- see the `Transport` section of the `pool` module docs for why. What remains of
P2 is `compio-redis`, and enabling the `tls` feature plus moving deployment DSNs
off `prefer` - no crate in the tree turns the feature on yet.

**2026-08-19: the paragraph above is itself now history on two points.** The
driver has all six libpq `sslmode` values, so:

- **`sslmode=prefer` is no longer plaintext-by-definition.** The pool builds a
  connector for every mode but `disable` and `prefer` attempts TLS, falling
  back by reconnecting in the clear. The reason it did not - no
  reconnect-after-handshake-failure path - was removed rather than worked
  around, so `prefer` now means one thing through the pool and through
  `Config::connect`.
- **`sslmode=require` no longer implies verification.** It is libpq's `require`:
  encrypted, and authenticated only if `sslrootcert` names trust anchors. A DSN
  that wants the server authenticated must say `verify-full` (or `verify-ca`).
  This matters for anything in this proposal that leaned on `require` as the
  strong setting - it never was one in libpq, and it is not one here.

"Moving deployment DSNs off `prefer`" therefore now means moving them to
`verify-full` with an explicit `sslrootcert`, not to `require`. Whether the
platform should *refuse* a weak `sslmode` at all is a zeroship policy question
and deliberately does not live in the driver, which is publishable and knows
nothing about zeroship.

### 9.1 Build order

```
P1  worker database authority          <- in flight
P2  driver TLS in compio-postgres/redis
     |
S1  identity + per-service allowlist   <- the DURABLE layer
     |
S2  TLS on cross-host hops + RFC 7523 assertions
     |
S3  attestation adapter                <- only when a deployer's env can attest
S4  signed user assertion for calls made on a user's behalf
```

S1 is built once and never rewritten. S2 and S3 each replace one function
behind the trait. That is why starting before the deployment footprint is
settled is not premature.

### 9.2 S1, concretely

1. Define `ServiceIdentity` and `PeerCredentials` per 6.6. Two types, mechanism
   tag only, no validity in the neutral output, `aud` in the verifier input.
2. Build `authorize(identity, endpoint)` from the measured allowlist in 5.2,
   keyed on **individual service identity**, not class.
3. Make "no credential presented" a framework decision that no implementation
   can override (6.5.3, the Temporal `nil`-claims hole).
4. Land the boot gate of 6.5.2 - named sentinel, empty treated identically,
   refuse to boot, dev escape on an explicit dev signal, per-subsystem scope,
   **tested against `deploy/compose/docker-compose.yml`** rather than
   `Default::default()`.
5. Surface the active posture in `CheckConfigReport`, `zeroship config check`
   and `readyz`.

### 9.3 What the config surface becomes (measured 2026-08-16)

Today: **9 declared config fields** across 5 services
(`auth.platform_mint_key`; `control.{control_key, auth_platform_mint_key,
worker_key}`; `gateway.{control_key, worker_key}`; `migrated.control_key`;
`worker.{control_key, worker_key}`), 12 references in compose, 9 in
`docs/reference/env-vars.md`.

| change | fields |
| --- | --- |
| deleted as auth credentials | 4 - `migrated.control_key` (measured unused), `worker.control_key`, `gateway.control_key`, `control.control_key` |
| narrowed to derivation-only | 2 - `worker_key` and `control_key` remain HMAC keys for `derive_app_scoped_control_token` (`crates/core/src/auth/mod.rs:165`), app workflow bearers (`crates/plugin-workflow/src/client.rs:163`), the runtime output-read token (`crates/worker/src/handler.rs:516`) and the `ZeroShip-User` HMAC (`crates/gateway/src/router/auth.rs:264`) |
| added | 5 signing keypair paths (one per service) + 1 trust-anchor location |
| **net** | **about 9 to 11 declared fields** |

**State this honestly: the count goes slightly UP.** The change is not about
fewer knobs. Today's nine are three shared secrets copied nine times, where
possession of any one grants broad authority. The eleven after are per-service
private keys that are never transmitted, plus one trust anchor that is public by
construction. The measure that improves is blast radius: from "one string opens
four services" to "one key impersonates exactly one service".

The derivation uses are the reason this is not a clean subtraction, and they
must be re-keyed deliberately rather than left pointing at a credential whose
authentication role has been removed.

### 9.3.1 How pluggable mTLS actually is (VERIFIED 2026-08-16)

Elsewhere this document calls mTLS "an adapter" and "a code change when someone
asks". Both are true and both understate the work. Stated precisely so nobody
plans against the optimistic reading:

**Capability today.** The services use `cyper` 0.8 and `compio-tls` 0.9, both on
`rustls` 0.23, which supports client certificates. **No client-certificate
plumbing exists anywhere in the tree** (searched for `client_cert`,
`with_client_auth`, `ClientConfig`, `Identity::from` across `crates/` and
`libs/`; the only hits were `WorkflowClientConfig`, a false positive on the
pattern). So the capability is available in the dependency and unused.

**Pluggable behind the trait:** the identity SEMANTICS. An mTLS adapter performs
no request-time verification; it reads what the handshake already established
via `PeerCredentials::tls_peer` and maps it to a `ServiceIdentity`. Small and
clean, exactly as 6.6 intends.

**NOT behind the trait, and therefore not small:**

1. **Server bind paths** - requiring a client certificate and configuring the
   CA to validate against, in each of five services' listener construction.
2. **Client connect paths** - presenting a certificate, at every `cyper` client
   construction site in the 5.2 inventory.
3. **A granularity mismatch.** mTLS identity is per CONNECTION; the trait is
   invoked per REQUEST. With keep-alive and pooling one handshake serves many
   requests. Semantically fine, but an mTLS adapter cannot do per-request
   audience binding - that must come from elsewhere. This asymmetry is itself an
   argument for the 7523 default (6.3).

**Conclusion:** the identity mapping is an adapter; the transport is a
port-level change spread across every bind and connect site. This is why 6.6
keeps `CredentialSource` as a SECOND trait - provisioning and rotation are a
different axis from identity semantics - and why mTLS should be built only when
a deployer actually needs it, not maintained speculatively.

### 9.4 Defects to fix alongside, found during investigation

| severity | defect | source |
| --- | --- | --- |
| high | worker superuser DSN forges the grants the mint trusts | I3 f1 / phase-0 review |
| high | `get_app_env` is any-app; the app UUID is caller-controlled | I3 f5 |
| medium | control to gateway workflow advance is unauthenticated | I3 f3 |
| medium | the gateway's auth-host reverse proxy forwards `/internal/platform-token` | I3 f4 |
| medium | route sync permits only plaintext HTTP | 2.3, I3 f6 |
| low | migrated holds an unused `control_key` and the broad `zeroship_control` DB role | I3 f7 |

### 9.5 Explicitly out of scope

End-user auth, creator login, and CLI token issuance belong to
`docs/proposals/2026-08-16-cli-token-issuance.md`. S4 is the seam between the two
documents: this one establishes *who is calling*, that one establishes *on whose
behalf*.

---

## 10. Changelog

- 2026-08-16: created, PENDING. Sections 1-5 verified at `adc8be620`; section 6
  records discussion decisions; section 8 lists the questions in flight.
- 2026-08-16, update 1: **I2 (RFC 7523 depth + abstraction) reported.** It
  corrected three specifics in section 6: the `aud` value (issuer identifier,
  not endpoint URL, per rfc7523bis after the 2025 audience-injection attacks),
  the mandatory nature of `jti` (REQUIRED by OIDC Core sec 9, not the MAY of
  bare 7523; CVE-2020-15222 for skipping it), and the trait shape (two types,
  no validity in the neutral output). New 6.3.1 records the corrections, 6.3.2
  restates the anti-7523 argument at full strength, and 6.6 is rewritten around
  the surveyed convergence. Architectural consequence added: the `jti` cache
  must be shared across verifier replicas, which is tractable only because
  verifiers are core services rather than edge. Still outstanding: I1 (OSS
  self-hosted practice) and I3 (internal call inventory).
- 2026-08-16, update 2: **I1 (OSS self-hosted practice, ~18 projects) reported,
  and it REVISED the plan rather than confirming it.** Section 6.5 superseded by
  6.5.1: keep the trait as a SEAM, ship exactly one implementation, do NOT ship
  a mechanism-selecting config key, and **delete `SharedSecret` entirely**
  rather than fencing it dev-only - every dev-only-by-construction claim in the
  survey was falsified by a shipped deployment artifact, and a generated keypair
  costs a developer nothing. Rotation needs no mode: it is a JWKS with two
  entries keyed by `kid`. New 6.5.2 is a prescriptive boot gate copied from
  Superset's post-CVE fix. New 6.5.3 records field evidence, including a direct
  warning that Temporal's equivalent abstraction does NOT cover the internode
  hop - the very thing this proposal exists to secure - so coverage must be by
  construction and "no credential presented" must be a framework decision. New
  6.5.4 records the mandatory-over-pluggable counter-argument at full strength.
  Outstanding: I3 (internal call inventory).
- 2026-08-16, update 3: **I3 (internal call inventory) reported. All three
  investigations in.** It CORRECTED section 5: three classes are not sufficient
  as authorization principals, because a shared `core` identity lets auth or
  migrated inherit control's authority. New 5.1 records that classes organize
  policy while identity authorizes; 5.2 carries the measured per-identity
  allowlist; 5.3 ranks seven findings. Two that change sequencing: the worker's
  PostgreSQL superuser DSN defeats ANY HTTP allowlist, so the database-role work
  is a PREREQUISITE for S1 rather than a parallel track; and the PostgreSQL and
  Redis pools cannot enable TLS at all today, so S2 has a driver-level
  prerequisite in `compio-postgres` and `compio-redis`. Two deletions became
  available: `core/migrated` needs no credentialed HTTP endpoint, and its
  `ZEROSHIP_CONTROL_KEY` is unused. Two new defects: control-to-gateway workflow
  advance is unauthenticated, and the gateway's auth-host reverse proxy forwards
  `/internal/platform-token`.
- 2026-08-16, FINAL: section 1's decision table updated to post-correction
  answers (Q2 and Q4 revised, Q6 and Q7 added). New section 9 is the
  self-contained, authoritative implementation plan: two measured blocking
  prerequisites (worker DB authority; driver TLS in compio-postgres and
  compio-redis), the build order, S1 in five concrete steps, the measured
  config-surface accounting, and the six adjacent defects. Sections 1-8 are
  retained as the record of how the decisions were reached - including the three
  drafts the investigations falsified - and section 9 is what to build from.
- 2026-08-16, correction: added 9.3.1 after the operator asked whether mTLS is
  genuinely pluggable. It is only half pluggable, and the document previously
  implied more: the identity MAPPING sits behind the trait, but the transport
  wiring (server bind, client connect) does not, and mTLS binds identity
  per-connection while the trait is invoked per-request. Also VERIFIED that
  rustls 0.23 is already in the dependency graph via cyper and compio-tls, and
  that no client-certificate plumbing exists anywhere in the tree.
- 2026-08-16, blockers re-measured: **P1 CLEARED** by merge `b63e8e22a` - the
  worker now runs on a dedicated `zeroship_worker` login, so **S1 is unblocked
  and is the next thing to build**. **P2 refined**: I3's "the pools cannot
  enable TLS at all" overstates it. `compio-postgres` already has the
  `TlsConnect`/`MakeTlsConnect` abstraction, the `connect_tls` negotiation path
  and `compio-tls` as a dependency; what is missing is a concrete rustls
  implementation, since `NoTls` is the only one and the pool hardcodes it. The
  pool also already fails CLOSED on `sslmode=require`. `compio-redis` has no
  `compio-tls` dependency and is the larger half; whether Redis is even on a
  cross-host hop is NOT CHECKED.
- 2026-08-18: **the Postgres half of P2 is BUILT** - `tls_rustls.rs` plus the
  pool's `sslmode=require` path. The bullet above is now history; see the end of
  section 9 for what changed and what is left.

---

## 12. SCALE 2026-08-16: replica count is a blast-radius choice, not an infra requirement

The operator states the target is roughly **1000 replicas per service**. That
invalidates a load-bearing assumption in sections 6 and 9, and the correction is
structural rather than a tuning change.

### 12.1 What actually breaks, and what does not

**Correction to the first draft of this section**, which claimed self-signed
assertions "do not survive 1000 replicas". That is WRONG as stated, and the
distinction matters for planning:

- **Per-SERVICE keys are infrastructure-free at ANY replica count.** Five
  keypairs, five bundle entries, the key ships with the service image or config.
  A thousand gateway replicas all sign with the same gateway key and the bundle
  stays at five. No issuer, no CA, no SPIRE. Replica count is simply irrelevant
  here.
- **What breaks is per-REPLICA self-signed identity.** There the signer and the
  subject are the same entity, so N identities need N public keys: a 5000-entry
  bundle that churns with autoscaling, and every new replica must get its key
  INTO the bundle - the enrollment problem at a scale where hand-provisioning is
  impossible.

So the mechanism does not force infrastructure. **The granularity of identity
does**, and that is a blast-radius decision, not a scale requirement.

### 12.2 The three real options

| | bundle size | revocation granularity | verdict |
| --- | --- | --- | --- |
| per-service key | 5 | rotate for all 1000 or none | a shared secret again - narrower than today's one-key-four-services, still shared |
| per-point-of-presence key | dozens | cut off one site | workable interim; matches the blast radius that actually matters |
| per-replica, short-lived, centrally issued | the ISSUER's keys only | expiry, in minutes | **this is SPIFFE** |

### 12.3 The principle

**Separate the signer from the subject.** SPIFFE JWT-SVIDs are signed by the
SPIRE server, not by the workload, so one signing key covers unlimited
identities and the bundle stays small; identity lives in the `sub` claim.
Self-signed assertions cannot do this by construction.

### 12.4 What this changes

- **The trait and `authorize()` are UNAFFECTED.** This is exactly why the seam
  was built first: the mechanism changes underneath it, the authorization model
  does not. 9.1's claim that S1 is durable survives the correction.
- **Per-service keys are explicitly INTERIM**, not the target. Section 6.5.1 and
  section 9 should be read with that qualifier.
- **S3 (attestation) is not forced by replica count.** Per-service or
  per-point-of-presence keys carry the design at any scale with zero
  infrastructure. S3 becomes necessary only when the threat model requires
  revoking a SINGLE replica without rotating its peers - which is a judgement
  about how much a compromised process should cost, not something the fleet size
  decides.
- **The attestation source depends on the runtime**, and this is now urgent
  rather than deferrable: Kubernetes provides projected service-account tokens
  as a ready-made attestation source; bare metal means running SPIRE.

### 12.5 The open question that decides the interim

**Are the replicas long-lived or churning?** A fixed provisioned fleet can live
with per-point-of-presence keys for a long time. Autoscaled replicas with
minutes-to-hours lifetimes make central issuance mandatory almost immediately,
because there is no provisioning window at all. NOT ANSWERED.

### 12.6 Terminology correction

This document says "RFC 7523" throughout. That RFC profiles JWT use for OAuth
**client authentication to an authorization server's token endpoint**. We are
adapting its shape to direct service-to-service calls, where there is no token
endpoint and `aud` is the callee rather than the AS. The accurate name is
**self-signed JWT service assertions**, or `private_key_jwt`-style service
authentication. Calling it RFC 7523 implies conformance to a profile we are
adapting, not implementing - and per this codebase's own standard, a claim that
overstates what the code does is a defect.

### 12.7 Restated plainly: what needs infrastructure

| rung | infra needed | keys to distribute | revoke granularity |
| --- | --- | --- | --- |
| per-service key | **none** | 5 private, 5 public | the whole service |
| per-point-of-presence key | **none** | one per site | one site |
| per-replica, centrally issued | issuer + attestation | issuer keys only | one process |

The first two are config distribution, which every deployment already does for
its database URL. Only the third is a system to operate.

**So the "no infrastructure" claim in section 1 stands** - for the rungs we are
actually proposing to build. It would be false only if we committed to
per-replica identity, and we are not.

---

## 13. OPERATOR DECISION 2026-08-16: JWT assertions only, mTLS deferred

> "don't support mtls now, support only jwt profile now, we'll defer mtls"

**What this settles.** Sections 6.3 and 6.5.1 already recommended self-signed JWT
service assertions as the single shipped implementation. This makes it a ruling
rather than a recommendation, and removes the TLS-termination question (12.5,
task 6) from the critical path - it was only ever a gate on the mechanism choice.

**What NOT to build, and why it matters.** No mTLS arm. No stub adapter. No
`Mtls` variant behind the trait "ready for later". The OSS survey's sharpest
operational finding applies directly: Keycloak's optional
`cache-embedded-mtls-enabled` flag **silently did nothing for two version lines**
(CVE-2024-10973), because optional hardening is untested hardening. A speculative
half-built adapter is worse than no adapter - it reads as a supported path while
never being exercised.

**What preserves the option.** The trait itself (6.6), which is why it is a SEAM
and not a mode. `authorize()` is mechanism-independent and built once; adding
mTLS later replaces `IdentityVerifier` and touches no authorization code. The
naming convention costs nothing today and keeps the door open: use SPIFFE-shaped
identifiers (`spiffe://zeroship.ai/svc/gateway`) in `iss` now, and they carry
unchanged into X.509 SANs later.

**What must be answered before mTLS is revisited**, recorded so it is not lost:
does anything terminate TLS between a point of presence and a core region?
`deploy/ops/Caddyfile:55` already reverse-proxies `control:9090`, so control's
traffic passes through a terminating proxy today. If points of presence reach
control on that same hostname, mTLS identity dies at Caddy - and the
`X-Forwarded-Client-Cert` workaround makes control trust the proxy's assertion
about who called, which is the confused-deputy shape this whole programme removed
from the mint. mTLS in that topology needs a separate direct network path, which
is real work and a second surface to secure.

**Consequence for the build order.** Task 7 is now blocked only by task 5. S2's
transport half (TLS on cross-host hops, and task 8's `MakeTlsConnect` for
compio-postgres) is UNAFFECTED - server-side TLS is required for confidentiality
regardless of how identity is proven, and JWT assertions in cleartext would be
capturable and replayable inside their `exp`.

---

## 14. SHIPPED 2026-08-16: the mechanism, and the trust-bundle answer

The minter, the verifier and the replay store are implemented. Nothing is wired
into any service's request path yet; rollout is a separate change, and mixing it
in would have made this one unreviewable.

| Piece | Where |
| --- | --- |
| Issuer identifiers, minter, verifier, `ReplayStore` trait, in-memory store | `crates/core/src/service_assertion.rs` |
| Postgres `jti` store | `crates/authn/src/service_replay.rs` |
| The replay table and its grants, in the `service_authn` schema | `db/migrations-ts/20260816000100_service_assertion_replay.ts` |
| Security-property tests | `crates/core/tests/service_assertion_test.rs` (27), `crates/authn/tests/service_replay_pg_test.rs` (4, live PG) |

### 14.0 CORRECTED 2026-08-17: the replay table is not in the `zeroship` schema

It shipped there and collided with P1 head-on. `crates/zeroship-migrate-adapter/
tests/platform_migrate.rs` asserts a BLANKET invariant - `zeroship_worker`, the
login a process running creator code holds, has no INSERT/UPDATE/DELETE/TRUNCATE/
REFERENCES/TRIGGER on ANY relation in `zeroship`, no column-level equivalent, and
nothing on a sequence there. That is 9.0's P1 written down as a test. The
migration granted the worker writes on the replay table, and the suite failed
with `zeroship_worker can write a platform relation`.

Both sides were right. 5.2 has the gateway calling `worker POST /dispatch/{app}`,
so the worker is a CALLEE, a callee is what verifies, and a verifier must claim
the `jti` in a store every replica shares. So the grant was not removable.

The resolution is that `zeroship` was carrying two trust zones. It holds platform
STATE - apps, users, deploys, grants, billing - written by the control plane, and
authority over it is authority over the platform. The replay table holds none of
that: two columns, a key and an expiry, conferring nothing, written by every
service that authenticates. It now lives in `service_authn`, the invariant stays
blanket, and the new schema is bounded by its own assertion to exactly that one
table so the escape hatch cannot grow. Narrowing the invariant instead would have
left it reading "the worker writes nothing except the things it writes".

### 14.1 The trust bundle is STATICALLY CONFIGURED, not JWKS-over-TLS

6.6 leaned JWKS. The implementation went the other way, keyed on `iss` either
way. The four reasons, in the order they matter:

1. **Bootstrapping.** 6.3.2 already names shared state on the authentication hot
   path as the strongest argument against 7523. A JWKS fetch adds a SECOND
   availability dependency to the same path, and it is a dependency on the
   PEER - a service that has just restarted would fail inbound calls until it
   could reach the caller calling it. A static bundle makes verification a pure
   function of local configuration; only the replay store remains.
2. **The trust anchor is weaker, not stronger, in this deployment.** 2.3 records
   that internal hops are cleartext today. A JWKS document fetched over plain
   HTTP is attacker-substitutable, so key distribution would be exactly as
   strong as the network the assertions exist to stop trusting. Static key
   material needs no network trust at all. JWKS becomes defensible once S2's
   transport half lands, and not before.
3. **It does not actually save configuration.** JWKS needs a per-issuer URL in
   config regardless. The trade is a public key for a URL, which moves the trust
   decision onto DNS plus TLS plus peer liveness and buys only rotation
   convenience.
4. **`JwksCache` would not have given issuer binding for free.** It is keyed on
   one URL and `keys()` returns a flat `Vec<CachedKey>` with no issuer attached
   (`crates/core/src/oidc_verify.rs:203`). Using it would have required one
   cache instance per issuer anyway - and using it as-is would have produced
   precisely the flat pool RFC 8725 section 3.8 forbids.

**What this gives up, stated plainly:** unattended rotation. A new key is
trusted by editing configuration on each peer and restarting it. The bundle
holds several keys per issuer, so the overlap window is expressible and rotation
never needs a flag day - but it is an operator action, not a background refresh.

### 14.2 Key provisioning: what exists, and the hole

**What exists.** `ServiceSigningKey` generates a keypair, loads one from PKCS#8
DER, and emits the base64url public half a peer puts in its bundle.
`ServiceTrustBundle::trust` refuses to silently replace a key already held under
the same issuer and `kid`, so a later configuration line cannot revoke an
earlier one by shadowing it.

**What does NOT exist, and is required before rollout:**

1. **No config surface.** Nothing reads a private key or a trust bundle from a
   file, an environment variable or `zeroship dev init`. Every key in the tree
   today is constructed in a test. Section 9.3's config work is unstarted.
2. **No distribution mechanism.** How service A's public key reaches service B's
   configuration is entirely manual. For a single-VPS self-hoster that is a
   `zeroship dev init` away from fine; for a multi-host deployment it is an
   unautomated N-by-N step, and that is the real cost of choosing static bundles
   over JWKS.
3. **No boot gate.** 6.5.2's gate against weak defaults is queued separately.
   Until it lands, an empty trust bundle is a verifier that trusts nobody, which
   fails closed - but a MISCONFIGURED bundle is not detected at boot.
4. **No rotation runbook.** The overlap window is expressible; the procedure for
   using it is not written down.

Until 1 and 2 exist, this mechanism protects nothing in production, because
nothing calls it. The code is the end state; the provisioning around it is not.

### 14.4 SHIPPED 2026-08-21: the boot gate of 6.5.2 (hole 3 of 14.2)

Named sentinel `CHANGE_ME_ZEROSHIP_SERVICE_KEY`, empty and sentinel as one
branch, refuse-to-boot with a banner, a build-profile dev escape, per-subsystem
scope, and the posture in `CheckConfigReport` and `/readyz`. Wired into
gateway, worker, control and migrated.
`crates/core/src/config/credential_gate.rs` is the whole policy;
`tests/service_credential_boot_gate.sh` drives the real binaries.

**Four things it fixed that were not in the plan**, each measured rather than
anticipated:

1. **Control's `control_key` guard failed OPEN.** It was
   `if !settings.control_key.is_configured()`, and `config/env.rs` resolves
   `ZEROSHIP_CONTROL_KEY=` to `Secret::supplied(Env, Some(""))` - configured,
   empty, accepted. That is Gitaly's
   `if len(conf.GetToken()) == 0 { return ctx, nil }` in another language,
   sitting in this tree while 6.5.3 cited it as somebody else's bug. Every row
   now runs a validator over the MATERIAL.
2. **`zeroship-migrated --check-config` validated nothing.** Its report emitted
   and the process returned `Ok` at `main.rs:33`; every credential guard it had
   sat below, from line 87 on. A dry run over a placeholder seal key exited 0.
   The gate now runs before the dry-run return.
3. **The compose gate could not see the two credentials that matter most here.**
   `enforced_rules` drops every `SecretStrength::Unrestricted` row, and those
   rows are `ZEROSHIP_CONTROL_KEY` and `ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` - so
   a compose file shipping either as empty or as the placeholder printed exactly
   what a clean one prints. It judged 9 occurrences before and judges 14 now,
   measured by running it on both branches.
4. **A dry run over file-sourced credentials reported `configured` while
   reading none of them.** Found by running the real binary:
   `service_credentials = configured` beside `..._checked = 0` and
   `..._unread = 4`. The posture gained a third value, `unverified`.

**One finding NOT fixed, recorded so it is not lost.** `PLATFORM_SECRETS` gives
`ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` `SecretStrength::Unrestricted` with
`require_nonempty`, but the value's real consumer,
`ManagedPolicyConfig::default_confined`, refuses anything under 32 bytes
(`crates/migrated/src/main.rs`, test `policy_config_refuses_one_byte_key`). The
table therefore describes a check that is not the strictest one that runs, which
is the class of defect the table exists to prevent. Fixing it means changing the
row's strength AND its validator together, since
`platform_secret_rows_state_the_floor_their_validator_applies` drives both.

**What the gate does NOT cover.** Auth holds no service-to-service credential
since the mint key was deleted (2.1 is now history on that point), so it has no
rows; its stash and TOTP keys inherit the sentinel refusal through the shared
validators. No DSN is covered, in any service. `zeroship-workflow-scheduler`
has one credential, a DSN, and no rows.

### 14.3 One divergence from 6.6

`IdentityVerifier::verify` is now asynchronous, returning a boxed future rather
than a `Result` (`crates/core/src/service_identity.rs`). 6.6's sketch is
synchronous. It had to change: the `jti` claim is I/O and it must happen INSIDE
the seam, because a claim made by the caller after `verify` returned would mean
a `ServiceIdentity` exists for a replayed assertion. The future is boxed rather
than written as `async fn` so the trait stays dyn-compatible, which is what the
`?Sized` bound on `verify_identity` always promised. Nothing else in 6.6 changed:
two types, `aud` as input only, no validity in the neutral output, trust domain
and name compared together.
