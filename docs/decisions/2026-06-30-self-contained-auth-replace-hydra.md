# 2026-06-30 — Build a self-contained OAuth2/OIDC provider in `crates/auth`; remove ORY Hydra entirely

**Status:** accepted (immutable once landed).

**Design:** `docs/proposals/2026-06-30-self-contained-auth-replace-hydra.md` (the HOW — scope, phasing, §5A standards conformance).
**Standards map:** `docs/proposals/oauth2-standards-conformance.md` (RFC-verified; distilled into design §5A).
**Supersedes the approach of:** the pluggable-provider epic (`feat/auth-providers`, 11 commits) — Hydra and Supabase stop being the platform Authorization Server; see *Consequences*.

## Decision

The platform builds its **own OAuth2/OIDC Authorization Server (a near-complete OpenID Provider) inside `crates/auth`** and **removes ORY Hydra entirely** — both Hydra's end-user-OAuth role and its deploy/control token-kernel role. Specifically:

1. **Self-contained identity.** `zeroship.users` + password / magic-link / TOTP / sessions is the primary IdP. Supabase / Google / GitHub become optional **upstream social** logins (federation), not the AS.
2. **Two app-facing faces.** Browser / creators consume the gateway-signed **session cookie + `ZeroShip-User`** (BFF, already built). CLI / programmatic clients use **OAuth2** — the Device Authorization Grant (`zeroship login`) plus a **closed-world** `/authorize` + `/token`.
3. **Closed-world AS.** Only the grants/flows zeroship itself uses; clients are platform-provisioned; **no third-party OAuth clients, no dynamic client registration** (see *Tripwire*).
4. **`crates/auth` issues + signs platform tokens** (its own key) and serves the platform **JWKS**; the gateway and control verify platform tokens via that JWKS, replacing Hydra introspection. The worker (untrusted V8) never holds keys.

## Rationale

- **Operational weight.** ORY Hydra is a general-purpose, certified, multi-tenant AS — a separate Go sidecar + its own `oauth_hydra` Postgres schema/role + admin/public split. For a **closed-world, first-party** auth surface that is overkill; the operator's call is that the ongoing operational + integration weight exceeds its value here.
- **In-process token kernel.** Owning issuance in `crates/auth` (compio/zero-tokio, `jsonwebtoken`/EdDSA) removes the outbound Hydra dependency + its circuit breaker, and unifies the token contract under one platform issuer.
- **The closed world bounds the risk.** Because zeroship controls both ends (it provisions clients and owns the SDK/gateway that consume tokens), the AS may legitimately omit the machinery that exists to protect *untrusted third-party* clients (implicit/hybrid/ROPC — removed by OAuth 2.1 anyway; dynamic client registration; FAPI; third-party introspection; DPoP/mTLS). That makes the OP **auditable**.

## Standards baseline (non-negotiable)

The build targets **OAuth 2.1 + RFC 9700 (OAuth Security Best Current Practice, BCP 240)** as the normative baseline, with a 15-item RFC-tagged conformance checklist (design §5A) as implementation acceptance criteria — incl. PKCE-S256 (RFC 7636), exact redirect-URI match (OAuth 2.1 §2.3.1), `nonce` + `at_hash` (OIDC Core), the `iss` response param for mix-up defense (RFC 9207), JWT access tokens `typ: at+jwt` (RFC 9068), refresh rotation + reuse-detection (OAuth 2.1 §6.1), the Device Grant polling discipline (RFC 8628), Native-App rules for the CLI (RFC 8252), and reject `alg:none` (RFC 7515). **The OpenID Foundation conformance suite (Basic OP + Config OP) is the cutover acceptance gate.** The closed-world cuts are documented as **spec-permissible** — the subset is a compliant profile, not a corner-cut.

## Consequences

- **This is a near-complete OpenID Provider build, not a bolt-on** (the honest scope the design review forced out). `crates/auth` today is a **Hydra login/consent _front-end_** (login completion calls `accept_login` back to Hydra; `consent.rs` is a Hydra consent-challenge handler) — so login/consent/logout must be **rewritten** to mint platform artifacts, and the gateway's full Hydra OIDC relying-party stack (~2,700 LoC across `oidc_rp.rs` / `anchors.rs` / `backchannel_logout.rs`) must **re-home** onto platform tokens. ~7 gated phases; adversarial review on every auth-touching slice.
- **We own a security-critical OP forever.** The CVE-class core (PKCE, auth-code store, id_token/nonce/at_hash, alg-pin, redirect exact-match, key rotation, refresh reuse-detection, revocation/logout, relay/pairwise) is mandatory at full rigor regardless of the closed-world cuts. Mitigated by the conformance suite + the smaller auditable surface + the tripwire below.
- **The 11-commit pluggable-provider epic is reframed.** *Survives:* the device-grant state machine, the own-ecosystem email hook, the `AuthProvider` verify seam (now verifies platform tokens). *Repurposed:* the Supabase identity-bridge / GoTrue login / `email_confirmed_at` lookup → the Supabase-upstream-social adapter. *Superseded:* "control verifies GoTrue access tokens for deploy" — the platform issues its **own** deploy tokens now. The operator accepts that part of that just-shipped work is demoted from co-equal-provider to optional-upstream.

## Tripwire — when this decision must be revisited

The closed-world subset is safe **only while** there are **no third-party OAuth clients** and **no dynamic client registration**, and redirect URIs are exact platform-known values. If the product later needs creator apps to be OAuth providers *to third parties* ("Login with [creator's app]"), or an external-developer client-registration platform, or arbitrary redirect hosts — **stop and revisit**: that is the signal to adopt a full general-purpose AS product, not to extend the subset.

## Alternatives considered (and why rejected here)

- **Keep Hydra (status quo).** Rejected: the operator judges it too heavy for the closed-world surface.
- **Keep coupled, bolt a parallel AS on only for Supabase** (the broker-analysis "Alt A"). Rejected: leaves two divergent end-user paths + a provider-dependent creator contract.
- **Rent a different AS as a sidecar** (Rauthy [Rust], Zitadel, Keycloak, Ory Kratos+Hydra). A legitimate lighter-than-build option and the standard best practice ("don't roll your own AS"); the language of a sidecar is irrelevant (you talk HTTP). **Rejected by the operator** in favor of an in-process, dependency-free, closed-world OP — *with eyes open* to owning a security-critical OP forever (this ADR records that the build-vs-rent tradeoff was examined, not assumed).
- **General-purpose (non-closed-world) AS.** Rejected: unnecessary surface for a first-party deployment; the closed world is the whole point.
