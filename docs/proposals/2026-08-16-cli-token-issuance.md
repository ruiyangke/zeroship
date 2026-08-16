# Who issues the CLI's token: the control plane brokers, the OP signs

**Date:** 2026-08-16
**Status:** READY. All three investigations reported. Section 9 is the blueprint; it executes the accepted ADR `docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md` rather than proposing a new direction. Phase 0 is already in flight.
Sections 5 to 7 were written provisionally and have since been replaced by the
findings in section 8. Section 4 records a claim of mine that I3 falsified.
**Scope:** how `zeroship login` obtains a credential, and what authorises the
`/internal/platform-token` mint. Explicitly NOT: PAT lifecycle after erasure
(branch `fix/credential-lifecycle`), nor JWKS retirement of rotated signing
keys (verified separately, queued).

---

## 0. Reading conventions used in this document

Every claim about current behaviour carries a `file:line`. Claims are labelled:

- **VERIFIED** - I read the cited lines in this working tree at `6729a5985`.
- **INFERRED** - a conclusion drawn from verified facts, not itself read.
- **NOT CHECKED** - stated so the reader does not mistake silence for evidence.

An empty grep is reported as an empty grep, never as proof of absence.

---

## 1. Decision summary

There are two separable questions. Collapsing them is the main risk in this
document, so they are kept apart throughout:

| # | Question | Answer |
| --- | --- | --- |
| Q1 | May the mint accept an arbitrary principal, arbitrary scopes and an unbounded TTL, authorised by a shared secret? | **No**, regardless of Q2 |
| Q2 | Should the control plane broker the device flow at all, given the OP already implements it? | **No** - and the accepted auth ADR already said so; see 9.0 |

Q1 does not depend on Q2. Fixing the mint is correct whether the broker stays
or goes, which is why that work proceeded (branch `fix/platform-mint`) while
this document was still open.

**Section 9 is the blueprint.** Phase 0 is unconditional and in flight; phases
1 to 3 execute ADR `2026-06-30-self-contained-auth-replace-hydra.md`.

---

## 2. What exists today (VERIFIED)

### 2.1 The OP implements RFC 8628 completely

- `crates/auth/src/oidc/device_token.rs:107` - `device_authorization`, the
  device authorization endpoint.
- `crates/auth/src/oidc/device_token.rs:27` - `DEVICE_CODE_GRANT_TYPE`,
  `urn:ietf:params:oauth:grant-type:device_code`.
- `crates/auth/src/oidc/device_token.rs:153` - writes `zeroship.device_grants`
  itself.
- `crates/auth/src/ui/device.rs` - the OP has its own device approval page,
  alongside `consent.rs`, `login.rs`, `sessions.rs`.

So the OP owns the endpoints, the grant table and the approval UI.

### 2.2 The control plane implements a parallel flow

- `crates/control/src/device_handlers.rs:148` - `device_auth`
- `crates/control/src/device_handlers.rs:236` - `device_approve`
- `crates/control/src/device_handlers.rs:294` - `device_token`

### 2.3 The CLI uses the control plane's flow, not the OP's

- `crates/cli/src/auth.rs:166` - posts to control `/api/device/auth`
- `crates/cli/src/auth.rs:229` - polls control `/api/device/token`

### 2.4 The control plane cannot sign, so it asks the OP to mint

`crates/auth/src/oidc/device_token.rs:566` - `mint_platform_token`:

- `:572` - the entire authorization decision is
  `authorized(&req, cfg)`, which is `extract_bearer` + `validate_control_key`
  (`:625-634`). Possession of `control_key` is sufficient.
- `:577` - `principal_id` is parsed as a UUID and never tied to the caller.
- `:580` - `audience` and `client_id` are checked non-empty only.
- `:583-589` - scopes are shape-validated by `Scope::parse`. There is **no**
  subset check against the named principal's stored grants.
- `:590-593` - `ttl_secs` must be positive. There is no ceiling.

### 2.5 The scope ceiling lives in the caller, not the issuer

`crates/control/src/device_handlers.rs:60-65` intersects `DEPLOY_TOKEN_SCOPES`
with the principal's grants **before** calling. That is discipline in one
client, not an invariant of the mint. Any other holder of the secret bypasses
it entirely.

### 2.6 The secret is not exclusive to the control plane

- `crates/worker/src/cache.rs:98`, `:130`, `:208` - the worker holds
  `control_key`.
- `deploy/compose/docker-compose.yml` - `ZEROSHIP_CONTROL_KEY` is injected into
  five services: `auth`, `control`, `gateway`, `migrated`, `worker`.

The worker is the process that executes untrusted creator code in V8.

### 2.7 These tokens cannot be recalled

`crates/control/src/device_handlers.rs:76-81` records in its own doc comment
that what does not bound this token is server-side revocation:
`zeroship.token_revocations` is only ever written for per-app RP clients with a
pairwise subject. **NOT CHECKED** by me independently; the fix branch was asked
to verify it.

---

## 3. The defect, stated precisely

The service holding the signing keys has delegated its authorization decision
to a caller and retained only the mechanical act of signing:

```
control   decides    "this human approved this CLI login"
   |
   |   POST /internal/platform-token      (shared secret)
   v
auth      executes   mints for whatever principal control names,
                     with whatever scopes control asks for,
                     without independently verifying either
```

This is a confused deputy. The OP acts on an assertion it cannot check, and the
only thing establishing that assertion's legitimacy is possession of a secret
held by five services.

**The principle violated:** an issuer must validate, not merely sign. An OP is
supposed to be the component that knows what a principal is entitled to. Here it
is the component that takes the caller's word for it.

**Blast radius (INFERRED from 2.4 + 2.6):** any path yielding the worker's
`control_key` - a V8 escape, an SSRF reaching an environment dump, an operator
leak - converts into an access token for any principal, any scope, any
lifetime. A contained compromise becomes a total one. This is an escalation
*amplifier*: it is not remotely triggerable on its own.

---

## 4. Why the indirection exists - CORRECTED BY I3

**This section previously claimed the provider-pluggability justification was
dead. That claim was WRONG, and the correction matters to the decision.**

What I read at `crates/control/src/device_handlers.rs:519` and `:533-545` is
about `ensure_platform_device_provider`, which governs how the flow **starts**.
It is true that starting requires the platform OP. I generalised that to the
whole flow. I3 checked the **approval** path and found the GoTrue branch very
much alive:

- Supabase mode can construct a trusted set holding both Supabase and the
  platform OP (`crates/control/src/main.rs:129-158`).
- On approval, B accepts the authenticated GoTrue role, checks email
  confirmation fail-closed, links or provisions a canonical platform principal,
  merges only on verified email, and seeds default grants for new principals
  (`crates/control/src/device_handlers.rs:834-893`,
  `crates/control/src/identity_bridge.rs:128-223,298-342`).
- A dual-provider test reaches this path and completes minting
  (`crates/control/tests/device_handlers_test.rs:1048-1066,1175-1222`).

So the identity bridge is live behaviour, not vestigial scaffolding. Deleting B
requires one of: retiring direct Supabase deploy login, completing upstream
federation, or retaining an equivalent identity-bridge component.

**Lesson recorded deliberately:** a doc comment about one entry point was read
as a statement about the whole subsystem. The comment was accurate; the
generalisation was mine.

---

## 5. Options for Q1 (the mint) - REFINED BY I2

I2 has reported (`scratchpad/research-svc-mint.md`, 29.7 KB). Its central
finding reframes this section: **the safety of every mechanism surveyed comes
from four validations performed at the issuer, not from the grant type.**

### 5.0 What every surveyed platform does (I2, VERIFIED against primary docs)

Across AWS STS AssumeRole, GCP service-account impersonation, Azure AD
on-behalf-of, Kubernetes TokenRequest and SPIFFE/SPIRE, the same four
properties hold:

1. **The caller is individually identified** - a trust-policy principal, an IAM
   grant, a client credential, an attested SVID. Never "whoever holds the blob".
2. **Authority to act for a subject requires proof ABOUT the subject** - a user
   token with the right audience, an IAM permission on the target, an object
   binding. Never a caller-asserted principal id.
3. **The ceiling is computed at the issuer by intersection** with what the
   subject and the caller are each entitled to. The request can only shrink it.
   AWS states this explicitly: session policies "limit... but do not grant".
4. **TTL is issuer-capped at about 1h**, with narrow audited paths to 12h.

Our mint fails 1, 2, 3 and 4 (section 2.4). Property 2 is the one with no
partial credit: `principal_id` is caller-asserted with no accompanying proof.

### 5.1 The options, restated against that standard

**5a. Compute the ceiling at the issuer (property 3).** The mint looks up the
named principal's grants itself and issues
`requested AND subject-entitlements AND caller-allowance`. Fixes the class.

**5b. Per-service identity (property 1).** Split the shared secret; allowlist
which service may call which endpoint. I2 is unambiguous that renaming or
rotating a shared secret while leaving its authority intact is the same defect.

**5c. Issuer-capped TTL (property 4).** Load-bearing precisely because 2.7 says
these tokens cannot be recalled.

**5d. RFC 8693 token exchange.** I2's verdict: the correct *grammar*, and
`subject_token` is precisely our missing ingredient - it is the proof that the
user's authority actually flowed through the caller. But the RFC delegates
trust policy to the deployment (sec 1), so **the deliverable is the four
validations, not the grant-type URN.** I2's cautionary evidence: Keycloak
shipped token exchange as technology preview for roughly six years before a
fully supported V2 (26.2, May 2025), and Auth0 ships it with all validation
left to customer Action code. A token-exchange endpoint with a passthrough
policy hook is *the current mint wearing a standards costume* - it would look
audited while being identical.

### 5.2 The finding that most directly indicts current code

I2, on where the ceiling belongs: **caller-side scope capping is not a
control.** Every surveyed system computes the intersection inside the signing
boundary. Our `DEPLOY_TOKEN_SCOPES` capping (section 2.5) sits in the caller,
which is why any other holder of the secret bypasses it. This is not a
hardening opportunity; it means the ceiling does not currently exist as a
system property.

### 5.3 The worker, named

I2 identifies our arrangement as Norm Hardy's 1988 confused deputy verbatim,
formalised as CWE-441: the worker holds mint authority (its own) while
executing attacker-controlled input (untrusted V8). Its sharpest observation:
**the V8 sandbox does not have to be escaped** - the worker process merely has
to be confusable into making an outbound request from its trusted side.
(INFERRED by I2 from structure, not demonstrated.)

Prior art for the fix shape: Google BeyondProd forwards a short-lived End User
Context ticket proving the user's authority; Netflix mints a verifiable
Passport once at the edge. Both are the industrial form of `subject_token` -
one guarded issuance point, a verifiable user-context object downstream, and no
service permitted to assert identity on its own say-so.

## 6. Options for Q2 (the broker) - PENDING REFINEMENT

**6a. Delete the parallel flow.** Point the CLI at the OP's RFC 8628 endpoints.
The mint then has no caller and can be deleted rather than hardened. Section 2.1
establishes the OP already has every piece required.

**6b. Keep the broker, fix the mint.** Q1's fixes make the mint safe; the
duplication remains as accepted cost.

**6c. Change the grant entirely.** A loopback redirect
(`http://127.0.0.1:PORT` with PKCE) is the common CLI pattern on machines with
a browser; device flow targets headless environments.

**Upgraded from NOT CHECKED to a live option by the first I1 result (AWS).**
AWS CLI v2.22.0 (2024-11-18) made **OAuth 2.0 authorization code + PKCE
loopback the DEFAULT** for `aws sso login`, displacing device code, which
remains available behind `--use-device-code` for browserless environments.
AWS's stated reasoning: "the authorization code flow with PKCE is the
recommended best practice for access to AWS resources from desktops and mobile
devices with web browsers."

(The researcher flagged that the common *phishing-risk* framing for
deprioritising device code comes from a secondary source, not an AWS primary
quote. Recorded as INFERRED, not attributed to AWS.)

Implication: choosing between our two device flows may be choosing between two
grants that are both the wrong default for the common case - a creator on a
laptop with a browser. Device flow would remain the correct fallback for CI and
headless machines.

**6d. The control plane owns only pixels.** Proposed by I1 to absorb the
product-console UX argument.

**I3 has made this MOOT for us.** Our approval already happens on the auth
service: B's `verification_uri` points at the OP's `/device` page, not the
console and not control (`crates/control/src/device_handlers.rs:908-921`). Auth
already ships compiled pages for login, signup, consent, device authorization,
linking, reset, magic links, federation and TOTP.

So we do not have the problem 6d solves. The strongest counterargument in
section 6 - that 11 of 14 platforms approve on a product surface - simply does
not apply to us, because we are already in the minority that approves at the
IdP. That REMOVES a reason to keep the broker rather than adding one.

### The case FOR keeping the broker, at full strength

Stated deliberately, so this document is not one-sided. I1 found the UX
argument to be much stronger than I assumed:

- **Approval on a product surface is the field norm, not an indulgence.**
  **Eleven of fourteen** platforms surveyed approve on a product surface rather
  than a standalone IdP consent screen (Stripe, Supabase, Netlify, Fly,
  Railway, Render, Heroku, DigitalOcean, Vercel, Cloudflare, Databricks). The
  three counterexamples - AWS, Google, GitHub - are all cases where the
  identity surface is itself a first-class product with its own UI, "which is
  not a luxury a small platform has". The reason is substantive: a good
  approval page shows which app, which team, which scopes in product
  vocabulary, which IP and location, and often lets the user *choose* a team as
  part of approving. That data lives in the control plane.
- **I1's own honesty about its recommendation:** if the OP's consent page must
  call the control plane to render, "then you have an internal channel
  regardless - you have merely made it read-only. That is a genuine
  improvement, but it is a smaller improvement than 'delete the internal
  channel', and it is dishonest to present the recommendation as eliminating
  cross-service coupling when it eliminates only cross-service *issuance*."
  Option 6d exists precisely to absorb this.
- **Entitlements genuinely belong to the control plane.** If it owns plans,
  tiers, seats and per-app entitlements, it is the only component that knows
  what scopes a user may hold on an app today. Pushing that model into the OP
  creates a second source of truth that will drift. The counter is that this
  argues for the control plane as *policy decision point*, not *issuance
  point* - but the cost must be stated fairly: having the OP consult the
  control plane at issuance adds a hop and couples login availability to
  control-plane availability.
- **One public hostname.** (Substantially weakened by I1: see Vercel, below.)
- **Changing a working login path pre-launch costs risk.**

---

## 7. Recommendation - all investigations reported; superseded by section 9

### 7.1 Unconditional, and not blocked on Q2

I2's steelman for keeping the shared secret is genuinely strong (section 7.3),
but **its own stated preconditions include two that fail today**. These are
therefore not trade-offs; they are defects under either architecture:

1. **The worker must hold no mint credential.** I2: "No version of the argument
   covers handing mint authority to the process that executes untrusted
   customer code. This is not simplicity vs rigor; it is a defect in either
   model." Today the worker holds `control_key` (section 2.6).
2. **Principal, scope and TTL ceilings must be enforced at the issuer.**
   Wherever the broker lives, the checks belong inside the signing boundary.

Both are hours of work, and neither depends on how Q2 resolves.

### 7.1b What deletion actually costs (I3, VERIFIED)

I3 measured it, and it is NOT a CLI URL change. Three real costs:

1. **The OP's current token cannot authorize against control.** B mints with
   the global principal UUID as `sub`, control's audience, and a 12h TTL
   (`device_handlers.rs:643-649`, `auth/src/oidc/issuer.rs:387-400`). A's
   generic OAuth client mint issues `aud = "zeroship"` and a *pairwise*
   `pws_...` subject, while control requires `aud = control.zeroship.ai` and a
   `sub` that parses as a UUID (`control/src/config.rs:266-268`,
   `crates/authn/src/lib.rs`). **A first-party CLI token policy is required;
   registering a `zeroship-cli` client is not sufficient.**
2. **Per-principal live grant intersection.** B intersects its four-scope
   ceiling with the principal's current `principal_grants` at redemption
   (`device_handlers.rs:563-614`). A only checks the client's static
   registration and then mints what was requested. I3's classification: the
   fixed ceiling is cheap to move to A; **the live per-principal intersection
   cannot work in A under current privileges**; one-time creator provisioning
   needs an ownership change or a retained control callback.
3. **The Supabase identity bridge** (section 4), which is live.

Smaller deltas, both directions: B rate-limits grant creation and approval
where A does not; B returns 12h without refresh where A returns 15 minutes and
can refresh; A has stronger client binding and honours `slow_down` backoff.

**What I3 found B does NOT add**, contrary to the section 6 steelman: no
membership, billing, or plan check - B's request carries no app ID
(`device_handlers.rs:86-121`), and membership authorization happens later in
authz against `app_members`. Audit is shared: auth's `/device` page emits the
same `device_grant` event for either provider. So two of the four arguments for
keeping the broker are now measured to be false.

After B is deleted the mint has **no remaining in-tree caller** (INFERRED by
I3): production's sole caller is B; the rest are five control tests, a mock,
`tests/e2e_device_login.sh`, and `tests/supabase_deploy_e2e.sh` transitively.

### 7.2 Conditional on Q2 - now leaning hard, all investigations reported

Both research investigations agree, having approached from different
directions. I2: implementing RFC 8628 on the OP **deletes the mint's reason to
exist rather than securing it**. I1: nobody in the field brokers this way, and
the one platform in our exact topology (Vercel) went direct.

**The recommended shape is 6a (delete B), and 6d is unnecessary** because we
already approve at the OP (section 6). I1's product-console argument, the
strongest case for the broker, does not describe us.

But I3 has established that deletion is a **first-party CLI client project**,
not a URL change. The prerequisites, in order:

1. Define a first-party CLI token policy on the OP: non-pairwise `sub` equal to
   the platform principal UUID, `aud` control accepts, and a deploy-appropriate
   TTL. Without this, an A-issued token is rejected by control outright.
2. Move the four-scope ceiling onto the CLI client registration (cheap), and
   decide how per-principal live grant intersection is preserved. This is the
   genuinely hard one: I3 judges it cannot work in A under current privileges.
   Either the OP gains read access to `principal_grants`, or control keeps a
   narrow read-only callback, or the token stays coarse and control enforces
   entitlements at request time as a resource server. The third is most
   consistent with I1's recommendation and keeps entitlements in the service
   that owns them.
3. Resolve the Supabase identity bridge: retire direct Supabase deploy login,
   complete upstream federation, or keep an equivalent bridge component.
4. Port B's rate limiting onto A's endpoints.

Alongside it: loopback + PKCE as the CLI default, device flow retained behind a
flag for headless and remote-agent use (our AI-coding-agent story, so not
optional), and the control plane as a **resource server** validating OP-issued
tokens.

**Sequencing consequence:** because (1) to (3) are substantial, section 7.1's
unconditional fixes should NOT wait on them. Harden the mint now; delete it
later when its callers are gone.

If any residual server-side act-as-user need survives, both investigations
prescribe the same thing: RFC 8693 at the OP, per-service client authentication
(mTLS or RFC 7523 signed JWT assertion, not a bare shared secret), a required
`subject_token`, issuer-side intersection, an `act` claim naming the caller,
and an issuer-capped TTL - implemented as the validations, not merely the URN.

### 7.3 The case against, at full strength (I2's steelman, preserved)

Recorded so this document is not one-sided. A pre-launch platform with zero
tenants, one operator and five services on one compose network is not Google:

- **The perimeter is real.** An attacker able to read the mint secret on a
  service host is probably also able to read the DSN or Postgres directly, and
  can then edit the app registry or write the auth schema. The marginal power
  the mint adds may be near zero. "Hardening the mint while the same host holds
  the database credentials is polishing one door of a house with no walls."
- **Moving parts have failure modes.** Device flow adds polling, user-code UX
  and expiry handling; token exchange adds policy tables that can be
  misconfigured; asymmetric client auth adds key distribution, clock skew and
  rotation runbooks.
- **A half-built ceremony is worse than an honest hazard.** 37% of AWS
  integrations reportedly get ExternalId wrong; Keycloak took years to make
  exchange non-preview. The shared secret at least announces its danger.
- **Pre-launch economics.** Every hour on PKI is an hour not spent on the
  deploy contract or billing correctness.

This document accepts the perimeter argument as materially true and still
proceeds with 7.1, because 7.1's two items are conceded by the steelman itself.

### 7.4 OPERATOR RULING: the pre-launch economics argument is rejected

2026-08-16. The steelman's fourth bullet, and the deferrals this document drew
from it, are overruled by the operator:

> "pre-release means we are trying our best to close the gaps to production"

The AGENTS.md pre-launch stance is about **back-compat obligations** - rename,
break wire formats, delete rather than deprecate. It is not licence to defer
security architecture. Its own justification points the other way: "pre-launch
is the moment to get shapes right." For credentials this is decisive, because
after launch a change to the credential model means migrating live tenants and
coordinating rotation across every point of presence. The cost only rises.

**Consequences for this document:**

- Target the END-STATE credential model, not the minimum defensible one. Do not
  build two intermediate schemes and discard both.
- The service-to-service ladder (7.5) is a build order, NOT a menu with the top
  rungs marked optional.
- "Later, because pre-launch" is no longer a valid reason to defer an item.
  Dependency ordering remains valid: key retirement genuinely cannot precede
  the TTL ceiling; the signed-approval artifact should not land on a branch
  that is mid-revision.
- The perimeter argument (bullet 1) is ALSO void at the target topology: dozens
  of points of presence means dozens of perimeters, which means none. It was
  only ever true of a single compose network.

---

### 7.5 Service-to-service authentication: the build order

Four properties define adequate service-to-service auth. A shared bearer secret
has none of them:

1. **Identity, not possession** - the credential says WHICH service, never
   "a service".
2. **Short-lived and auto-renewed** - rotation is continuous, not an event. At
   dozens of points of presence a rotation *event* is an outage risk and a
   response-time floor when a leak is suspected.
3. **Audience-bound** - a credential for service A cannot be replayed at B.
4. **Mutual** - the caller verifies the callee too.

Build order. Per 7.4 these are sequential steps toward the end state, not
options:

| # | Step | Closes |
| --- | --- | --- |
| S1 | Per-service credentials with a per-endpoint allowlist | identity, least privilege. Ends "any service may call any internal endpoint" |
| S2 | Asymmetric client auth (mTLS, or RFC 7523 signed JWT assertion) for every cross-host hop | a leaked string stops being sufficient; adds confidentiality. Closes the plain-HTTP `control_key` finding |
| S3 | Attested workload identity (SPIFFE/SPIRE, or the platform's managed equivalent) | **bootstrapping** - the regress that S1 and S2 leave open |
| S4 | Verifiable user assertion on any call made on a user's behalf | the confused deputy; see 7.6 |

**S3 is the one usually skipped and the reason to design for it now.** S1 and S2
both leave a bootstrap problem: how does a workload obtain its FIRST credential?
If the answer is "from its environment", the original defect has been recreated
one level down. Attestation breaks that regress - identity derives from
properties of the running workload, so there is no long-lived secret to steal.

**Identity classes, from the edge topology.** The deployment is edge points of
presence (gateway, worker) against a small number of core regions (auth,
control, Postgres). Signing happens only in core; the edge only ever validates.
So the classes are:

    core     may reach internal control and auth endpoints
    edge     may sync routes and fetch bundles - nothing else
    worker   below edge: outbound authority is the app-scoped HMAC only,
             because it executes untrusted creator code

A compromised point of presence must yield edge authority and nothing more.

### 7.6 The user assertion, in the shape this codebase already uses

S4 has a local precedent that is better guidance than "adopt RFC 8693": the
gateway does NOT tell the worker who the end user is. It HMAC-signs a
request-bound `ZeroShip-User` header which the worker verifies
(`crates/runtime/src/core/dev_auth.rs:6-8`,
`crates/gateway/src/router/dispatch.rs:2425`), and dispatch strips inbound
`zeroship-user` headers as platform-reserved so a forged one cannot ride in.

That is exactly the separation the mint lacks. Generalise it:

    Authorization:  <service credential>        "I am control"
    subject proof:  <signed approval artifact>  "this user approved this grant"

Neither alone suffices. Note WHY this matters concretely: the user's approval
already exists as evidence, but as a `zeroship.device_grants` ROW - and the
adversarial review showed the worker's provisioning DSN could forge one, which
defeated the mint hardening entirely. A signed artifact cannot be forged by
database write access. This is `subject_token` in the idiom already present in
the tree.

---

## 8. Open questions the investigations must answer

Three read-only investigations are in flight. This section records what each
was asked, so a reader can judge whether the answers actually covered it.

**I1 - how the industry does CLI auth. REPORTED**
(`scratchpad/research-cli-auth.md`, 78 KB, 14 platforms).

### The headline: nobody does this. Zero of fourteen.

No surveyed platform has a non-IdP service broker a device flow and then ask an
identity service to mint a token for an arbitrary principal over an internal
channel. Everything resolves to one of three shapes:

| Shape | Count | Platforms |
| --- | --- | --- |
| A - CLI talks to a real authorization server | 8 | GitHub, Cloudflare, Vercel, Railway, Render, AWS, Google, Databricks |
| B - no AS in the CLI path at all; the product mints its own credential, which the same service validates | 5 | Stripe, Supabase, Netlify, Fly, Heroku |
| C - paste-a-token | 1 | DigitalOcean |

Ours is a fourth shape.

**I1's own stated limit on this finding (selection bias, section 6.5):** most
of the survey is shape B, where there is no second signing authority, so those
platforms *could not* broker even if they wanted to - the finding is weaker
than the raw 0/14 suggests. Only about four platforms are in our position at
all, and three of those demonstrably go direct. Heroku is the one partial
split, and it splits toward a *dedicated auth service* - the direction of our
OP, not our control plane; its internal channel is closed-source, so it neither
supports nor refutes.

### The two findings that most affect our decision

**Vercel is our exact topology, resolved the other way.** Issuer
`https://vercel.com`, `device_authorization_endpoint` on `api.vercel.com`, CLI
performs discovery, validates the issuer, uses only discovered endpoints, pure
RFC 8628, no broker. This substantially kills the single-public-hostname
argument: OP endpoints can be *served* under the API host while the issuer
identity remains the product domain. Path routing is a deployment concern.

**Railway deleted exactly what we are considering deleting** - a bespoke
GraphQL pairing-code flow replaced by standard OAuth against a real
self-hosted AS, PR #822 merged 2026-03-25.

### On the grant choice (bears on 6c)

Loopback + PKCE is the default in 5 of the 8 direct platforms, device flow in
3. The tally matters less than the direction, which is one-way and recent:
Google killed OOB in 2022 citing remote phishing risk; AWS flipped
`aws sso login` to PKCE by default in CLI 2.22.0; **Salesforce blocked device
flow outright for its default CLI connected app in Aug 2025**. The mechanism:
loopback binds approval to the requesting process, and device flow
deliberately breaks that binding - which is simultaneously its feature and its
phishing vector (Storm-2372, TA2723).

We still need device flow: an AI coding agent on a remote machine is its home
turf. But note what I1 observes about hardening - capped poll window,
same-origin `verification_uri` check, IP and location on the approval page -
**it is all authorization-server work**, and a control-plane-owned parallel
flow gets none of it. It *structurally cannot* satisfy the same-origin check,
because its `verification_uri` is by construction not the OP's origin.

### On RFC 8693, converging with I2

The critical difference from our mint: `subject_token` is a token **the AS
itself issued** - the caller presents evidence rather than asserting a
principal id. Plus `may_act` (pre-authorization carried inside the subject's
token) and the `act` claim (provenance). RFC 8693 section 2.1 speaks directly
to our shared secret: client authentication "allows for additional
authorization checks by the STS as to which entities are permitted to
impersonate or receive delegations from other entities."

I1 checked seven production implementations (Keycloak, Okta, Zitadel, Auth0,
GCP `serviceAccountTokenCreator`, AWS STS, K8s `TokenRequest`, Vault) and found
one invariant: **the minted credential can never exceed what the requester
could already obtain, and the policy names both parties.** A shared secret plus
an arbitrary `(principal, scopes)` pair satisfies neither half.

**Published precedent:** CVE-2026-65595 (n8n) is this defect in the wild, and
its fix was literally "derive the scope list from the resolved user's role
before signing" - i.e. option 5a.

### If the broker stays anyway, the hardening that would make it defensible

I1's list, recorded so a decision to keep 6b is at least fully informed: the
mint must present OP-issued *evidence* of authorization rather than asserting a
principal; scopes intersected at mint time against an OP-side per-service
allowlist; an `act` claim naming the control plane so minted tokens are
distinguishable and separately revocable; mTLS or an RFC 7523 signed JWT client
assertion instead of a static shared secret; short TTL plus audience
restriction; and the endpoint unreachable both from the internet and via SSRF
from any request-handling path - the IMDSv2 lesson that network position must
not be sufficient.

### The original brief (retained for comparison)

Across `gh`, Wrangler,
Vercel, Netlify, flyctl, Stripe, Supabase, Heroku, doctl, Railway, Render,
gcloud, AWS SSO: does the CLI talk to the IdP directly or to a broker? Which
component terminates the flow and which issues the token? Where does the human
approve? Device flow vs loopback+PKCE - which dominates and why? **The question
that matters most:** does any well-regarded platform have a non-IdP service
broker a device flow and then ask the IdP to mint for an arbitrary user over an
internal channel? "Nobody does this" is a valuable finding.

**First result in - AWS SSO / IAM Identity Center** (VERIFIED against
`docs.aws.amazon.com` API references):

- **No broker.** The CLI talks to the OIDC service directly:
  `RegisterClient` -> `StartDeviceAuthorization` -> `CreateToken`, mapping 1:1
  onto RFC 8628 3.1/3.2/3.4 with an exact grant-type URN match. No
  non-IdP service sits in the issuance path.
- **Approval happens at the IdP's own UI** - the IAM Identity Center access
  portal (`*.awsapps.com/start`), not a product console.
- **Two-tier credentials.** Tier 1 is an SSO access token, 8h default
  (configurable 15min-90days), cached at `~/.aws/sso/cache/`. Tier 2 exchanges
  it via `GetRoleCredentials` for short-lived STS-shaped role credentials,
  auto-renewed while tier 1 is valid. Note the short-lived credential is the
  one used for work; the longer-lived one only buys more of them.
  COULD NOT DETERMINE whether `aws sso logout` revokes server-side or merely
  deletes the local cache.
- **No arbitrary-principal path exists.** `AssumeRole` is gated by the role
  trust policy plus the caller's own permission; `GetFederationToken` requires
  the caller to already hold that user's long-term credentials and makes the
  session policy mandatory; `AssumeRoleWithWebIdentity` requires an
  IdP-signed token whose issuer the trust policy names. Permission boundaries
  make the effective set an INTERSECTION across identity policy, boundary,
  session policy and SCP, with any explicit Deny winning. The result is always
  a subset, never a superset, of the calling identity's permissions.

That last point is I2's property 3 independently confirmed on a second
platform, and it is the exact inverse of our mint, where the caller names the
principal and the scopes are not intersected with anything.

**I2 - the standard for service-to-service minting.** RFC 8693 in detail;
AWS STS AssumeRole and the confused-deputy problem; GCP service account
impersonation; Azure on-behalf-of; Kubernetes TokenRequest; SPIFFE/SPIRE. For
each: what prevents the calling service from asking for more than it should
get? Where does the standard put the scope ceiling - issuer, caller, or both?
Recommended CLI credential lifetimes, and the honest cost of revocation
(RFC 7662 introspection, reference tokens, revocation lists).

**I3 - what deleting the broker would actually cost.** What flow B does that
flow A does not, exhaustively. Every caller of `/internal/platform-token`.
Whether both flows share `device_grants` and can misread each other's rows.
What session authenticates approval today. What the CLI stores and whether
`aud`/`iss`/scope shape would break downstream authorization in `crates/authn`
and `crates/authz`. Which tests change. Whether any ADR records why flow B
exists.

Each investigator was required - as output, not as permission - to argue
against its own recommendation.

---

## 9. The blueprint

### 9.0 This executes an accepted ADR; it does not propose a new direction

`docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md` is accepted and
already settles what section 7 was treating as open:

- `:13` - `zeroship.users` is the primary IdP; "Supabase / Google / GitHub
  become optional **upstream social** logins (federation), not the AS."
- `:14` - "CLI / programmatic clients use **OAuth2** - the Device Authorization
  Grant (`zeroship login`) plus a **closed-world** `/authorize` + `/token`."
- `:26` - the normative baseline is OAuth 2.1 + RFC 9700, explicitly including
  **RFC 8252 Native-App rules for the CLI** (the loopback pattern) and the
  **RFC 8628 polling discipline**.
- `:32` - **Superseded:** "control verifies GoTrue access tokens for deploy" -
  the platform issues its own deploy tokens now.

So control's parallel flow and `/internal/platform-token` are not a design
choice to re-litigate. They are the **un-migrated remnant** of a superseded
architecture. I3's "live GoTrue approval path" is live *code*, not a live
*decision* - the ADR already retired it. That removes the last open question in
section 7.2.

### 9.1 Phase 0 - unconditional, blocked on nothing

These are correct under every architecture, including keeping the broker
forever. In flight now.

| # | Change | Where | Status |
| --- | --- | --- | --- |
| 0.1 | Worker must hold no mint credential | `crates/worker`, compose | TODO - I2: "a defect in either model" |
| 0.2 | Issuer computes the ceiling: `requested AND subject-grants AND caller-allowance` | `mint_platform_token` | branch `fix/platform-mint` |
| 0.3 | Issuer-capped TTL | same | branch `fix/platform-mint` |
| 0.4 | Split the shared secret; per-service endpoint allowlist | auth + all callers | TODO |
| 0.5 | Erased accounts revoke every credential class | `crates/authn`, `crates/auth` | branch `fix/credential-lifecycle` |
| 0.6 | Prune retired signing keys from JWKS | `crates/auth/src/oidc` | **BLOCKED on 0.3** - see 9.1.1 |

#### 9.1.1 Why 0.6 cannot ship before 0.3 (correction)

This table originally listed 0.6 as parallel to 0.2 and 0.3. That was wrong,
and an agent briefed to implement 0.6 refused and explained why - correctly.

A retirement horizon must be at least the maximum lifetime of anything signed
with the key. On main today there is no such maximum: the mint accepts any
positive `i64` TTL and the issuer uses it directly as `exp = now + ttl`
(`crates/auth/src/oidc/issuer.rs:426`). **No finite horizon is safe until the
issuer-side TTL ceiling (0.3) exists.** Any number chosen now would be a guess
that can silently break verification for a longer-lived token.

Two useful facts established while proving this:

- **PATs do not constrain the horizon.** Their 365-day maximum looked like the
  binding constraint. It is not: they use a separate `PatIssuer` and the control
  signing key, and require an active DB lookup. So the horizon is governed by
  OIDC token lifetimes - 15 minutes for access and ID tokens, 2 minutes for
  logout tokens, and the 12-hour CLI deploy token as the largest fixed caller
  (`crates/control/src/device_handlers.rs`).
- **A second prerequisite nobody had named:** an old process can keep signing
  with its in-memory copy of a key after another process marks that key
  `retiring`. So `retiring_at` is not a reliable "last possible signature"
  timestamp. Either drain and fence old signers, or persist each key's maximum
  issued expiry.

Revised order for 0.6: land 0.3, then fence or record max-expiry, then prune to
a terminal `retired` status with horizon = enforced maximum + JWKS cache and
clock-skew allowance.

0.2 is the class fix: it is the published remedy for CVE-2026-65595 ("derive
the scope list from the resolved user's role before signing").

### 9.2 Phase 1 - make an OP-issued token usable by control

I3 established these are prerequisites, not consequences. Without 1.1 an
A-issued token is rejected outright.

1.1 **First-party CLI client policy on the OP.** Non-pairwise `sub` equal to
the platform principal UUID; `aud` that control accepts; deploy-appropriate
TTL. Today A issues `aud = "zeroship"` and a pairwise `pws_...` subject while
control requires its own audience and a UUID `sub`
(`crates/control/src/config.rs:266-268`).

1.2 **Move the four-scope ceiling onto the client registration.** Cheap; I3
classified it so.

1.3 **Decide where entitlements are enforced.** B intersects its ceiling with
live `principal_grants` at redemption; I3 judges that cannot move to A under
current privileges. **Recommendation: do not move it.** Issue a coarse token
and have control enforce entitlements at request time as a resource server.
This is I1's recommendation, keeps entitlements in the service that owns them,
avoids a second source of truth that drifts, and avoids coupling login
availability to control-plane availability.

1.4 **Port B's rate limiting** onto A's device endpoints.

### 9.3 Phase 2 - move the CLI onto the OP

2.1 **Loopback + PKCE (RFC 8252) becomes the default** for `zeroship login`.
This is the ADR's own normative baseline and the industry's one-way direction
of travel (Google 2022, AWS CLI 2.22.0, Salesforce Aug 2025).

2.2 **Device flow retained behind a flag** for headless and remote-agent use.
Not optional: an AI coding agent on a remote machine is its home turf.

2.3 **CLI uses OP discovery** and validates the issuer, as Vercel's CLI does.
Serve OP endpoints under whatever host you like; issuer identity stays the
product domain. Honour the same-origin constraint between the device
authorization endpoint, the token endpoint and `verification_uri` - which the
current parallel flow structurally cannot satisfy.

2.4 **Control becomes a resource server.** Validates OP-issued tokens; applies
entitlement-aware authorization at request time.

### 9.4 Phase 3 - delete the remnant

3.1 Delete `device_auth`, `device_approve`, `device_token` from control.
3.2 Delete `/internal/platform-token`. After 3.1 it has no in-tree caller.
3.3 Retire the direct-Supabase deploy path per ADR `:32`; keep Supabase as
upstream social federation per ADR `:13`.
3.4 Update the tests I3 enumerated: five control tests plus the mock at
`crates/control/tests/device_handlers_test.rs:331-369`,
`tests/e2e_device_login.sh`, `tests/supabase_deploy_e2e.sh`.

### 9.5 What this deletes, stated plainly

A shared bearer secret held by five services - including the one that executes
untrusted customer code - that authorises minting an access token for any
principal, with any scopes, for any lifetime, with no server-side revocation.
Replaced by: the OP issuing its own tokens under its own policy, which is what
the ADR said in June and what 0 of 14 surveyed platforms do differently.

### 9.6 Sequencing note

Phase 0 must NOT wait for phases 1-3. The mint is live in production today;
its hardening is hours of work, while the migration is a project. Harden now,
delete when the callers are gone.

---

## 10. Changelog

- 2026-08-16: created, PENDING. Sections 1-4 verified at `6729a5985`;
  5-7 provisional; 8 records the questions in flight.
- 2026-08-16, update 1: **I2 (service-to-service minting) reported.** Section 5
  rewritten around its four-property finding; 5.2 records that caller-side
  scope capping is not a control; 5.3 names the worker arrangement as CWE-441.
  Section 7 split into unconditional (7.1) and Q2-conditional (7.2), with I2's
  steelman preserved verbatim in 7.3 - including that its own preconditions
  concede 7.1's two items. **First I1 result (AWS SSO) folded in**: no broker,
  approval at the IdP's UI, intersection-only credentials, and PKCE loopback
  now the CLI default - which upgrades option 6c from NOT CHECKED to live.
  Still outstanding: the rest of I1, and all of I3.
- 2026-08-16, update 2: **I1 (CLI auth across 14 platforms) reported.**
  Headline: **nobody brokers - 0 of 14**, with I1's own selection-bias caveat
  recorded (most of the survey could not broker even in principle). Two
  decisive data points added: **Vercel** is our exact topology and went direct,
  which guts the single-hostname argument; **Railway deleted** an equivalent
  bespoke flow in March 2026. New option **6d - the control plane owns only
  pixels** - is now the leading candidate, because I1 showed the
  product-console approval argument is far stronger than section 6 originally
  credited (11 of 14 approve on a product surface). Grant direction recorded as
  one-way toward loopback+PKCE (Google 2022, AWS 2.22.0, Salesforce Aug 2025),
  with device flow retained for headless. CVE-2026-65595 (n8n) added as
  published precedent whose fix is exactly option 5a. Section 7.2 rewritten:
  recommend 6d, not plain deletion. **Only I3 remains.**
- 2026-08-16, update 3: **I3 (internal cost of deletion) reported. All three
  investigations in.** It CORRECTED two of my claims:
  (a) section 4's "the provider-pluggability justification is dead" was
  **wrong** - I read a comment about how the flow STARTS as a statement about
  the whole subsystem; the GoTrue identity bridge is live on the APPROVAL path
  and a dual-provider test exercises it end to end;
  (b) option 6d is **moot** - our `verification_uri` already points at the OP's
  `/device` page, so we are already in the 3-of-14 minority that approves at
  the IdP, and I1's strongest counterargument does not apply to us.
  New section 7.1b records the measured cost of deletion: a first-party CLI
  token policy is required (A's `aud`/`sub` shape is rejected by control), live
  per-principal grant intersection cannot move to A under current privileges,
  and the Supabase bridge needs a decision. I3 also **falsified two steelman
  arguments**: B adds no membership/billing/plan check and no unique audit.
  Recommendation moved from 6d to **6a, sequenced after prerequisites**, with
  section 7.1's hardening explicitly not blocked on them.
  Status stays PENDING pending your decision on the Supabase question, which is
  a product call rather than a technical one.

---

## 11. OPERATOR DECISION 2026-08-16: personal access tokens are not supported

> "do not support PAT token, this is a security decision"

Not a preference to be traded off in design. PATs are REMOVED, not hardened.

**Why this is coherent with the rest of the document.** A PAT is a SECOND
issuance authority: control holds its own signing key (`PatIssuer`,
`crates/control/src/main.rs:707`) and mints credentials with it, valid up to
365 days (`crates/control/src/token_handlers.rs:20`). Section 9 argues the OP
must be the sole issuer and control a resource server. PATs are the largest
counterexample to that, and were not previously named as one.

**Surface to remove** (VERIFIED at `b63e8e22a`):

```
routes      POST /me/tokens, GET /me/tokens, DELETE /me/tokens/{id}
            (crates/control/src/token_handlers.rs:257-264)
consumers   crates/authn/src/lib.rs:226   per-request validation query
            crates/authn/src/lib.rs:263   last_used_at update
            crates/authz/src/eval.rs:149  policy lookup
            crates/control/src/lib.rs:496 AuthzGuard bearer path
storage     zeroship.permission_tokens
key         the control plane's own signing key, if PATs are its only
            consumer - CHECK before deleting it
```

**Nothing in the CLI or the proven deploy path uses one.** The live chain
verified on zeroship.co ran on the 12-hour device token, not a 365-day PAT. So
removal does not break deployment.

**What replaces the automation use case:** a refresh-token family on the
device-flow credential. `crates/auth/src/oidc/refresh.rs` already implements
families, rotation, reuse detection and `family_absolute_expires_at`; its own
header says "CLI/programmatic rotation + reuse detection". The CLI simply never
requested one - `crates/control/src/device_handlers.rs:68` states the flow
"issues no refresh token", which is also WHY that token was given a 12-hour life.

With refresh in place the access token should get SHORTER, not longer: a brief
self-contained JWT plus a long-lived DB-backed family gives long usable life AND
real revocation - which is what PATs were providing, from the correct issuer.

| PAT gave you | a refresh family gives you |
| --- | --- |
| a 365-day credential | a long-lived family with an absolute expiry |
| revoke one row | revoke one family |
| named, separately scoped | one family per login, scoped at issuance |
| created via API for CI | one browser approval per CI setup, then self-renewing |

The one genuine loss is non-interactive creation: a PAT can be minted by API, a
device flow needs a human once. For CI that is a one-time setup cost.

**Consequence for the open findings:** the auth-flows review's HIGH finding that
PAT issuance discards the caller's OAuth scope ceiling
(`docs/architecture/auth-flows.md`, finding 3) is resolved by DELETION. Do not
spend a fix round on it.

**Ordering.** Removal is safe once the CLI has refresh, because that is what
covers long-lived automation. It does NOT depend on deleting
`/internal/platform-token`, and it must NOT be bundled with that change - two
deletions in one commit make both unreviewable.
