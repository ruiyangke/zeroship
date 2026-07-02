# Pluggable platform auth providers (Hydra + Supabase)

**Status:** proposal (active, not shipped)
**Date:** 2026-06-29
**Scope:** make the *platform's own* auth/OAuth backend selectable — ORY Hydra (today) **or** Supabase (GoTrue) — behind one operator switch, without leaking the choice into the rest of the codebase.

> Pre-launch, no back-compat. Renames/breaks are fine in one PR. This proposal does **not** add Hydra→Supabase migration shims — it adds a clean seam and two implementations behind it.

> **Round-1 revision note.** This draft was rewritten after an adversarial review found the original seam did not compile (`async fn` in a `dyn` trait over a `!Send` client), claimed a semantic equivalence (`introspect` → `IntrospectResult`) that its only consumer (`control::authz_guard`) cannot satisfy, assumed a GoTrue signing mode (asymmetric/JWKS) that is *not* the self-hosted default (HS256), sold a revocation **downgrade** as an upgrade, and deferred the genuinely hard work (GoTrue-identity → platform-principal mapping) to an undefined later phase. Every one of those is addressed below; see the **Revision log** at the end for the flaw-by-flaw mapping. Line/symbol references are to the real tree at `.worktrees/auth-providers`.

---

## 1. What "platform auth" actually is here

The map of the current code shows Hydra is doing **two structurally different jobs**, and conflating them is what makes "just swap in Supabase" sound simpler than it is:

| Job | What it means | Who owns it today |
| --- | --- | --- |
| **(A) End-user IdP / login** | Authenticate a human: password, magic-link, Google/GitHub social, TOTP; hold the user store + sessions; render login/consent/device UI. | `crates/auth` (the IdP front-end) — but the *tokens* are minted by Hydra. |
| **(B) OAuth2 protocol kernel** | Issue/validate tokens for *registered clients*: authorize-code+PKCE, refresh, **device grant**, introspection, **per-client registration**, **consent delegation** (login/consent challenge accept-reject), JWKS/issuer. | Hydra (the `oryd/hydra` sidecar). |

Three consumers depend on these:

- **control** — validates deploy bearers (`control/src/authz_guard.rs`, job B) and provisions a per-app OAuth client (job B, client registration).
- **gateway** — the end-user BFF (`gateway/src/oidc_rp.rs`): authorize redirect, code exchange, refresh, revoke, introspection, JWKS verify (job B), plus DPoP-bound introspection, RFC-9068 raw-JWT verification, server-held refresh anchors/families, and pairwise (`pws_`) subject projection.
- **CLI** — `zeroship login` device grant + poll (`cli/src/auth.rs`, job B), the load-bearing path for agent/CI deploy. **The CLI is a synchronous, `curl`-shelling binary with no compio runtime** (see §6) — a constraint that shapes the whole device-grant design.

**The crux:** Supabase **GoTrue is an (A) product, not a (B) product.** GoTrue issues *its own* JWTs for *its own* front-end users. It does **not** implement: the OAuth2 device-authorization grant, dynamic per-client registration with consent delegation, RFC 7662 admin introspection, DPoP, RFC-9068 `client_id`-bound access tokens, or pairwise/sectoral subjects. And critically, **a GoTrue user identity is not a platform principal** — bridging the two is the actual hard problem, addressed in §3. So a literal "GoTrue replaces Hydra" is impossible at job B. The honest design names the seam at job B, gives GoTrue-shaped *replacements* for what it lacks (a platform-mediated device flow, a platform identity bridge), and **documents precisely what degrades** (§7).

### Capability matrix (the load-bearing table)

| Capability (job B) | Hydra | Supabase / GoTrue | Provider strategy |
| --- | --- | --- | --- |
| Token validation | `/admin/oauth2/introspect` (RFC 7662, remote, **revocation-immediate**) | No RFC 7662 endpoint. JWT is **locally verified** (HS256 shared-secret **by default**, or asymmetric/JWKS if enabled) | `verify_token()` → Hydra: remote introspect; Supabase: **pure local verify** (§4). Revocation is recovered by a **control-side** session deny-list checked at control's authz boundary, *not* inside `verify_token` (§5). Returns a provider-native `VerifiedToken`, **not** a ready-made authz result — §3 maps it. |
| JWKS / issuer discovery | `{public}/.well-known/jwks.json`, issuer `{public}/` | asymmetric mode only: `{url}/auth/v1/.well-known/jwks.json`, issuer `{url}/auth/v1`. **HS256 mode has no JWKS.** | `issuer()` / verification-mode selector (§4). |
| Authorize-code + PKCE (end-user login) | `/oauth2/auth` + `/oauth2/token` | GoTrue `/auth/v1/authorize` + `/auth/v1/token?grant_type=pkce` | gateway BFF authorize/exchange/refresh/revoke — Hydra-rich path stays concrete; Supabase degrades per §7. |
| **Device authorization grant** (CLI) | `/oauth2/device/auth` + poll `/oauth2/token` | **Absent** | **Platform-mediated device flow** (§6): control is the device-authorization server; GoTrue authenticates the human; the issued bearer is a **real GoTrue session token** (so control's `verify_token` is actually exercised — §6, P-S2). |
| **Per-app OAuth client registration** | `/admin/clients` CRUD | **Absent** (no third-party client model) | Hydra: `HydraAdmin`/control's client-writer (kept as-is in P2). Supabase: platform-DB client record (§7c) — explicitly a follow-up, with a designed identity/consent story, not a hand-wave. |
| **Consent delegation** | `/admin/oauth2/auth/requests/*` | **Absent** | Hydra-only delegation protocol; stays in `crates/auth`. Under Supabase the consent step is the platform's own UI in front of a GoTrue session (§6 step 2 / §7c). |

The matrix is the contract. We do **not** pretend GoTrue can do consent-delegation, DPoP, pairwise subjects, or client registration — §7 enumerates each degraded behavior with its consequence.

---

## 2. The seam: `core::auth_provider` (enum dispatch, no `dyn`, no async-trait)

A new module `crates/core/src/auth_provider/`. It is **zero-tokio / cyper-based** and **`!Send` by construction** — the same constraint that already governs `core::oidc_verify` (its `cyper::Client` lives in a `thread_local!` because cyper's connector wraps its future in `send_wrapper::SendWrapper` and *panics* cross-thread; `oidc_verify.rs:39-66`) and `core::hydra` (`#[allow(clippy::future_not_send)]` on `introspect`, `hydra.rs:130/191`).

<!-- Added in round 1: addressing CRITICAL C1 — async fn in a dyn trait does not compile, and the stack is !Send. -->
**Dispatch mechanism — enum, not trait objects.** The original draft stored providers as `Arc<dyn TokenProvider>`. That does **not compile**: native `async fn` in traits (stable since 1.75) is **not `dyn`-compatible**, and the only escapes — `#[async_trait]` (imposes `Send`, which the cyper stack cannot satisfy) or `#[async_trait(?Send)]` (a `Pin<Box<dyn Future>>` heap allocation *per call* on the auth hot path) — are both wrong for a `!Send`, allocation-sensitive verify path.

This repo already solved exactly this problem and the design follows its idiom verbatim. `plugin-db/src/backend/mod.rs:28-31` chooses `async fn` in trait **without** boxing precisely "so the orchestrator's hot paths don't allocate a `Box<dyn Future>` per call," and runtime backend selection is an **enum** matched at the call site (`context.rs`: `BackendHandle::Postgres(..)` / `BackendHandle::Sqlite(..)`, `exec.rs`: `match` on it). `plugin-storage` uses `#[async_trait(?Send)]` only in *tests*, never on a hot path.

So the seam is a concrete enum with inherent `async` methods. No trait object, no `async_trait`, no per-call allocation, `!Send` preserved:

```rust
/// The selected platform auth provider. One value per service, built once
/// at startup from ZEROSHIP_AUTH_PROVIDER + provider config, stored in
/// AppState as `Rc<AuthProvider>` (single-threaded compio; no Send/Sync).
pub enum AuthProvider {
    Hydra(HydraProvider),       // wraps today's HydraIntrospector / OidcRp / HydraAdmin
    Supabase(SupabaseProvider), // §4–§7
}

impl AuthProvider {
    /// Validate a bearer (deploy token / access token). Returns the
    /// provider-native verified claims — NOT an authz decision. The
    /// GoTrue-identity → platform-principal bridge is a SEPARATE step (§3),
    /// because a GoTrue `sub` is not a control-plane principal and a GoTrue
    /// `role` is not an OAuth scope.
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, AuthError> {
        match self {
            AuthProvider::Hydra(p) => p.verify_token(token).await,     // remote introspect
            AuthProvider::Supabase(p) => p.verify_token(token).await,  // pure local verify (deny-list is control-side, §5)
        }
    }

    pub fn issuer(&self) -> &str {
        match self { AuthProvider::Hydra(p) => p.issuer(), AuthProvider::Supabase(p) => p.issuer() }
    }
    // begin_device / poll_device live on the *server-side* device-authorization
    // server (control), not here — the CLI never holds an AuthProvider (§6/§8).
}
```

`VerifiedToken` is the *neutral substrate*, not a ready authz result:

```rust
pub struct VerifiedToken {
    /// Provider-native subject. Hydra: a platform principal UUID (usr_…).
    /// Supabase: a GoTrue user UUID — NOT a platform principal (§3 bridges it).
    pub provider_subject: String,
    pub email: Option<String>,
    /// Whether the IdP asserts the email is verified. Hydra: from the
    /// `email_verified` claim (already carried on `AccessClaims`,
    /// `oidc_rp.rs:632`). Supabase: `true` iff GoTrue stamps
    /// `email_verified == true` (equivalently a non-null `email_confirmed_at`).
    /// **Defaults to `false`** — an absent/unparseable claim is NOT verified.
    /// §3.2 gates email-based account-linking on this being `true`.
    pub email_verified: bool, // <!-- Added in round 2: MAJOR-1 — close the unverified-email account-takeover vector. -->
    /// GoTrue `session_id` when present — the revocation key the control-side
    /// deny-list is built on (§5). Carried on the substrate; the *lookup* is
    /// NOT in this verify path (it lives at control's authz boundary, §5).
    pub session_id: Option<String>,
    /// Provider-native authorization signal:
    ///   Hydra    → OAuth `scope` string (platform-controlled scopes).
    ///   Supabase → GoTrue `role` (`authenticated`/`service_role`) ONLY.
    /// authz policy is NOT derived from this alone for Supabase (§3).
    pub provider_authz: ProviderAuthz,
    pub exp: u64,
}

pub enum ProviderAuthz { OAuthScope(String), GoTrueRole(String) }
```

> Why `VerifiedToken` and not the old `IntrospectResult`: the original claimed Supabase `introspect()` "returns the same `IntrospectResult`," but its only consumer (`authz_guard::oauth_guard_from_bearer`, lines 250-295) reads three fields a GoTrue token cannot satisfy — `aud` (must contain the platform OAuth audience; GoTrue stamps `aud="authenticated"`), `sub` (parsed as a control-plane principal `Uuid`; a GoTrue UUID is not one), and `scope` (drives the whole policy; GoTrue has `role`, not `apps:deploy`). The struct can be shared; the *semantics* cannot. §3 is the bridge that turns a `VerifiedToken` into an `AuthzGuard`.

`HydraProvider` / `SupabaseProvider` are concrete structs holding their own state (thread-local cyper client, `JwksCache`, HS256 secret, deny-list handle). They expose inherent `async fn`s; the enum forwards. The gateway's *rich* OAuth machinery (DPoP, RFC-9068 binding, refresh anchors, pairwise) is **not** flattened into this enum — it stays on the concrete Hydra `OidcRp`, and §7 states exactly what the Supabase arm does instead.

### Selector + config (fail-closed boot validation)

```
ZEROSHIP_AUTH_PROVIDER = hydra | supabase     (default: hydra)
```

| Provider | Config |
| --- | --- |
| `hydra` | `HYDRA_ADMIN_URL`, `HYDRA_PUBLIC_URL`, `ALLOW_REMOTE_HYDRA_ADMIN`, `AUTH_UI_URL` (unchanged). |
| `supabase` | `SUPABASE_URL`, `SUPABASE_ANON_KEY`, `SUPABASE_SERVICE_ROLE_KEY` (admin ops), and **exactly one** verification mode: `SUPABASE_JWT_SECRET` (HS256) **xor** `SUPABASE_JWKS_URL`+`SUPABASE_JWT_ISSUER` (asymmetric). |

<!-- Added in round 1: addressing MINOR m2 — fail-closed config validation. -->
**Boot validation (S-grade, fail-closed — the repo's config-hardening bar):** with `=supabase`, the service refuses to start if (a) neither or both of `SUPABASE_JWT_SECRET` / `SUPABASE_JWKS_URL` are set (ambiguous verification mode is a key-confusion foothold — §4), (b) `SUPABASE_SERVICE_ROLE_KEY` is absent while any admin op is reachable, or (c) `SUPABASE_URL` is not HTTPS in a non-dev profile. No silent fallback to HS256, no "try JWKS then HS256" — the mode is pinned at boot.

---

## 3. GoTrue identity → platform principal (THE hard problem)

<!-- Added in round 1: addressing CRITICAL C2 + the "Missing concepts #1" identity-mapping gap. -->
This is the section the original draft was missing entirely. `control/src/authz_guard.rs::oauth_guard_from_bearer` (250-295) turns a validated bearer into an `AuthzGuard { principal_id, token_policy, … }`. Under Hydra that is trivial because Hydra is configured to mint platform-shaped tokens: `sub` *is* a platform principal UUID, and `scope` *is* the platform policy. **Under Supabase neither holds.** We must bridge explicitly.

### 3.1 The identity-link table

```sql
-- control schema
CREATE TABLE zeroship.identity_links (
    principal_id     uuid NOT NULL REFERENCES zeroship.users(id),
    provider         text NOT NULL,            -- 'supabase'
    provider_subject text NOT NULL,            -- GoTrue user UUID
    email            text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, provider_subject)
);
```

A GoTrue `VerifiedToken.provider_subject` resolves to a platform `principal_id` through this table. **No row → not a principal.**

<!-- Added in round 2: MINOR-5 — reconcile with the existing per-app identity / anchor tables so P-S3 extends rather than forks them. -->
**Boundary vs. the existing per-app identity tables (MINOR-5).** This is a *different scope* from the auth-sdk per-app identity infrastructure, and the two must not be conflated:

| Table | Migration | Scope | Maps |
| --- | --- | --- | --- |
| `zeroship.app_user_identities` | `V0009` | **end-user × creator-app** | `(app_client_id, global_user_id)` → per-app `pairwise_sub` (`pws_…`, a never-stored projection) + relay email. |
| `zeroship.app_session_anchors` | `V0006` | **end-user × creator-app** | per-app server-held rotating refresh family (reload-recovery anchor). |
| `zeroship.identity_links` *(new)* | this proposal | **external-IdP-subject × platform principal** | `(provider, provider_subject)` → `principal_id` (control/deploy identity). |

The new `identity_links` is **net-new**, not an extension of `app_user_identities`: the latter keys on the *per-app* client and stores a per-app pairwise subject for *end-user* sessions, whereas `identity_links` keys on the *platform-global* IdP subject and resolves the *creator/operator* principal that drives control-plane authorization. They share only the `zeroship.users(id)` anchor (`global_user_id` / `principal_id` both FK it). **The collision the implementer must watch is P-S3** (per-app end-user OAuth on GoTrue, §7c): there, a GoTrue *end-user* identity must flow through the **existing** `app_user_identities` + the `pws_` projection (extending V0009, e.g. adding a `provider` discriminator), **not** a second parallel table. So: `identity_links` = platform-principal layer (this proposal); `app_user_identities` = per-app end-user layer (extend in P-S3). The boundary is the `zeroship.users` row each side resolves to.

### 3.2 JIT provisioning (account linking, the Supabase-recommended pattern)

First-time login (the device-flow approval step, §6) runs inside an authenticated GoTrue session, so the platform holds the GoTrue `sub`, `email`, and `email_verified`. At that moment control:

1. looks up `identity_links(provider='supabase', provider_subject=sub)`; if present → return its `principal_id` (done).
2. if absent, decide how to provision the link **inside one transaction** (the `(provider, provider_subject)` PK makes it idempotent under concurrent first-logins):
   - **Email-merge into an existing principal is allowed ONLY when `VerifiedToken.email_verified == true`** *and* an existing `zeroship.users` row already owns that exact email. Then the new `provider_subject` is linked onto that existing `principal_id`.
   - **Otherwise** (`email_verified == false`, or no email, or no existing principal owns it) control creates a **fresh** `zeroship.users` principal and links the `provider_subject` to it — it never merges on an unverified or absent email.
3. returns the platform `principal_id`.

<!-- Added in round 2: MAJOR-1 — the email-link merge is the classic account-takeover surface. -->
**Why the `email_verified` gate is load-bearing (MAJOR-1).** Merging on email is exactly the account-linking-takeover vector Auth0/Okta/Microsoft warn about: a self-hosted GoTrue with lax email confirmation (`GOTRUE_MAILER_AUTOCONFIRM=true`, or a provider that doesn't verify) lets an attacker register a *victim's* email, and a blind email-merge would link the attacker's `sub` into the victim's principal and hand them the victim's `principal_grants` (deploy rights). The substrate's `email_verified` (populated in §4) makes the merge refuse unless the IdP asserts the email is verified; an unverified collision becomes a *distinct* principal, never a takeover. This is "just-in-time provisioning / identity linking" (the Auth0/Clerk/WorkOS pattern) **with the verified-email precondition those vendors mandate**. JIT happens **only** in the authenticated device-approval step — never blindly from a bearer on the deploy hot path (a bearer for an unlinked `sub` is rejected, not provisioned).

### 3.3 Deploy scopes under Supabase (policy is platform-side, not token-side)

GoTrue tokens carry no `apps:deploy`/`apps:read` scopes — only a coarse `role`. So the deploy policy is **not** derived from the token. It is derived from the **principal's platform grants** recorded when the device approval is granted:

```sql
CREATE TABLE zeroship.principal_grants (
    principal_id uuid NOT NULL REFERENCES zeroship.users(id),
    grant        text NOT NULL,        -- 'apps:deploy', 'apps:read'
    PRIMARY KEY (principal_id, grant)
);
```

<!-- Corrected in round 2: MINOR-6 — this is a call-site refactor, not a one-line branch. -->
`authz_guard` becomes provider-aware. This is a **call-site refactor of `oauth_guard_from_bearer` (`authz_guard.rs:250-295`), not a single added branch**: today it hardcodes `state.hydra_introspector.introspect(token)` and reads an `IntrospectResult { active, aud, sub, scope }`. The refactor (a) swaps `AppState.hydra_introspector` for `Rc<AuthProvider>` and calls `state.auth_provider.verify_token(token)`; (b) folds the old `active`-flag check into `verify_token`'s success/error result (a verify error *is* "inactive"); (c) keeps the **PAT branch above this function untouched**; and (d) splits the post-verify mapping on `VerifiedToken.provider_authz`:

- **Hydra** (`OAuthScope`): unchanged — `sub` is the principal, `scope` → policy via `authz::scopes_to_policy`. Audience is checked against `expected_oauth_audience` exactly as today.
- **Supabase** (`GoTrueRole`): reject unless `role == "authenticated"`; resolve `provider_subject` → `principal_id` via `identity_links` (§3.1; reject if unlinked); derive policy from `principal_grants` (§3.3), **not** the token. The platform OAuth-audience check is **not applicable** — GoTrue audience is validated inside `verify_token` (`aud == "authenticated"`, §4); the control-plane authorization comes from the principal's grants, so there is no weakening of the Hydra audience gate (the two paths are disjoint, selected by `provider_authz`).

The net effect: `authz_guard` still ends at `AuthzGuard { principal_id, token_policy }`, but the three fields the GoTrue token can't supply are sourced from the platform identity bridge instead of the token. This is the resolution of "introspect returns the same result" — it doesn't; the bridge makes the consumer correct.

---

## 4. SupabaseProvider — token verification (HS256 **and** asymmetric)

<!-- Added in round 1: addressing CRITICAL C3 — GoTrue's default is HS256 shared-secret, no JWKS, no kid; core::oidc_verify only does asymmetric and requires a kid. -->
The original claimed GoTrue JWTs are "locally verifiable against GoTrue's JWKS (ES256/RS256)" and that we'd "just reuse the hardened `core::oidc_verify`." That is wrong for the **default and self-hosted** case — exactly what P-S1's test container runs. Self-hosted GoTrue signs **HS256 with a shared `GOTRUE_JWT_SECRET`**: no `kid`, no JWKS. And `core::oidc_verify` builds `DecodingKey`s only from RSA/EC/OKP JWK components (`oidc_verify.rs:304-342`) and **requires a `kid`** (`oidc_verify.rs:447-450`) — it cannot verify an HS256, kid-less token at all. Asymmetric JWT signing keys are a recent, **opt-in** Supabase feature.

So `SupabaseProvider` supports **two verification modes, pinned at boot** (§2):

**Mode A — HS256 (default / self-hosted), `SUPABASE_JWT_SECRET`.** A **new, small, audited symmetric path** (NOT a reuse of `JwksCache`): `jsonwebtoken::decode` with `DecodingKey::from_secret(secret)`, `Validation::new(Algorithm::HS256)`, `set_issuer(&[{url}/auth/v1])`, `set_audience(&["authenticated"])`, `exp`/`nbf` enforced. The `Validation` **hard-pins `alg = HS256`** so a kid-less RS/ES token cannot sneak through. The "reuse the already-hardened verifier" risk-control does **not** apply to this mode — it is new code and gets its own negative-test suite (§8).

**Mode B — asymmetric / JWKS (hosted Supabase, or self-hosted GoTrue with asymmetric keys enabled), `SUPABASE_JWKS_URL`+`SUPABASE_JWT_ISSUER`.** Reuses `core::oidc_verify`'s `JwksCache` + a new thin `verify_access_jwt`-style function (no nonce/at_hash — these are access/session tokens, not OIDC ID tokens). The `Validation` **hard-pins the asymmetric alg family** and rejects HS256, closing the classic **HS256-with-RSA-public-key key-confusion** vector. (The existing kid+alg match at `oidc_verify.rs:456` already blocks it for ID tokens — a HS256 header finds no RS256-keyed JWK and fails `NoMatchingKey` — but the Supabase verifier states the pin explicitly and tests it.)

Both modes pin `iss` (from config — **not hardcoded**: hosted GoTrue uses `{url}/auth/v1`, but **self-hosted defaults to the literal `"supabase"`** via `GOTRUE_JWT_ISSUER`, so the verifier reads `SUPABASE_JWT_ISSUER` and pins exactly that), match `aud` **tolerantly** (GoTrue's `aud` is a `ClaimStrings` — usually the literal `"authenticated"` but legally an array, and `service_role` differs — so accept-if-contains, not string-equality), and enforce `exp`/`nbf`. Mapping into `VerifiedToken`: `sub → provider_subject`, `email`, `session_id` (GoTrue `session_id` claim — reliably present in modern GoTrue, the deny-list key), `role → ProviderAuthz::GoTrueRole`, `exp`.

<!-- Corrected in round 3 (GoTrue source research): GoTrue does NOT emit an email_verified JWT claim. -->
**`email_verified` — corrected mechanism (round 3).** GoTrue's access token does **not** carry a trustworthy `email_verified` claim. There is no such top-level claim in GoTrue's `AccessTokenClaims`; the only in-token signal is a **user-writable `user_metadata.email_verified` mirror that is known to go stale** (the confirm-link updates `email_confirmed_at` on the user record but not reliably the metadata flag — supabase/auth #1620). Authoritative verification state lives on the **user record (`email_confirmed_at`)**, reachable only via the **service-role admin API** `GET /auth/v1/admin/users/{id}`. Therefore, under Supabase, `VerifiedToken.email_verified` is **NOT** filled from the token — the §3.2 account-link gate performs a **service-role admin lookup of `email_confirmed_at` at link time** (the one moment it matters, off the hot path), and fails closed if the lookup is unavailable. The `VerifiedToken.email_verified` field still exists and is populated from the token for the **Hydra arm** (the existing `AccessClaims.email_verified`, `oidc_rp.rs:632`, `None → false`); for the Supabase arm it defaults to `false` on the substrate and the verified truth is resolved by §3.2's admin lookup, not by trusting the JWT.

---

## 5. Revocation under local verify (a real downgrade, mitigated — not "strictly better")

<!-- Added in round 1: addressing CRITICAL C4 + MINOR m3 — local verify cannot revoke before exp; hydra.rs:105 invalidate_by_sub is a live logout dependency. -->
The original sold "no network on the hot path — strictly *better* than Hydra" as an upgrade. It is a **security downgrade** in one dimension: local JWT verification **cannot revoke a token before its `exp`**, whereas Hydra's `/admin/oauth2/introspect` reflects revocation immediately — and the code *relies* on that (`hydra.rs:105 invalidate_by_sub` drops cached active-token entries after OIDC back-channel logout). Under naive local verify there is nothing to invalidate; a stolen deploy/session token stays valid until expiry. We state this plainly and mitigate in two layers:

1. **Short access-token TTL + refresh.** Configure GoTrue `GOTRUE_JWT_EXP` short for platform/deploy tokens (minutes, not the 3600s default); the durable credential is the refresh token. The CLI **already** refreshes (`cli/src/auth.rs:106-138`, `grant_type=refresh_token`), so the platform-mediated session (§6) inherits short-lived access tokens with no CLI change. This bounds the post-revocation validity window to the access-token TTL.

2. **A session deny-list — the provider-neutral analog of `invalidate_by_sub`, checked at control's authz boundary, NOT in the core verify hot path.**

<!-- Rewritten in round 2: MAJOR-3 — specify WHERE the deny-list lives, the data source, the cache/staleness model, and keep verify_token pure so the gateway hot path stays lookup-free. -->
The round-1 wording put the lookup inside `verify_token`, which is wrong twice: (a) `verify_token` lives in `core::auth_provider`, is `!Send`, and holds only a thread-local cyper client — it has **no** DB/Redis handle and dragging one across the core boundary breaks the seam; (b) `verify_token` is shared with the **gateway** edge path, where a per-request deny-list round-trip would re-add exactly the network-on-hot-path cost local-verify removed. Resolved by placing the deny-list deliberately:

- **`core::auth_provider::verify_token` stays pure local-verify — no deny-list lookup.** It only checks signature / `iss` / `aud` / `exp` / `nbf` and emits `session_id` onto `VerifiedToken`. Both the gateway edge and control call it identically; neither blocks on a store.
- **The deny-list is consulted only at control's authz boundary** — one extra step in `oauth_guard_from_bearer` (§3.3), *after* `verify_token` succeeds, before returning the `AuthzGuard`. Control **already holds** its compio-postgres pool / compio-redis handle, so no handle crosses into `core`; the check is `is_denied(verified.session_id, verified.provider_subject)` on control-owned state. The **gateway edge path does NOT consult the deny-list** — it relies on the short access-token TTL alone (the gateway only sees end-user app tokens, not the platform deploy tokens the deny-list governs).
- **Data source:** a small **revoked-session set** keyed by GoTrue `session_id` (with `sub` for the coarse ban-the-user case), populated by the platform logout / ban / back-channel path — the same writer that drops the Hydra cache today. It lives in **control's Postgres** (`zeroship.revoked_sessions(session_id text pk, principal_id uuid, revoked_at timestamptz, expires_at timestamptz)`), optionally fronted by shared **compio-redis** when control is multi-replica. Entries are pruned at the token's max `exp`, so the set is bounded by `(#revocations in one token-TTL window)`.
- **In-process TTL cache with a stated staleness bound.** `oauth_guard_from_bearer` does **not** hit PG/Redis per request. Control holds an in-process `Rc<RefCell<DenyCache>>` (single-threaded compio, no Send) refreshed from `revoked_sessions` on a **compio interval task** every `DENY_CACHE_TTL` (default **10 s**). A revocation is therefore visible to control within at most `DENY_CACHE_TTL` after it is written. (Membership is a small in-memory set; if the revoked set ever grows large, the same shape holds with a counting Bloom filter behind the exact set — not needed at platform-operator scale.)

**The explicit tradeoff (made concrete).** Effective post-revocation validity window:
- **control / deploy path** = `max(DENY_CACHE_TTL, 0)` for a known revocation = **≤ 10 s** (deny-cache staleness), bounded above by the access-token TTL for revocations the deny-list never receives.
- **gateway edge path** = the **access-token TTL** (no deny-list there by design).

So immediate-ish revocation is restored for the platform-controlled logout/ban path (`≤ DENY_CACHE_TTL`), at the cost of a bounded staleness window instead of Hydra's synchronous-introspect zero-window. It does **not** cover GoTrue-side revocations the platform never observes (those wait out the short TTL — the residual, documented downgrade). Everything here is zero-tokio: compio-postgres / compio-redis for the store, a compio interval task for the refresh, `Rc<RefCell<_>>` for the cache.

This is also the operational story (Missing-concept #8): under Hydra an operator watches introspection latency + `active=false` rates; under Supabase those signals vanish, replaced by **JWKS-rotation failure** (mode B) / **HS256-secret-rotation failure** (mode A) and deny-list growth as the health signals to alert on.

---

## 6. Device flow — Hydra-native vs platform-mediated (the CLI stays sync/curl)

<!-- Added in round 1: addressing MAJOR M5 + M1 — CLI is sync curl, no compio; key on device_code not user_code; specify entropy/TTL/CSRF/rate-limit. -->
GoTrue cannot do RFC 8628. Rather than drop CLI login under Supabase, control **becomes** the device-authorization server, parameterized by which IdP authenticates the human.

**The CLI never holds an `AuthProvider` and never touches async cyper.** `cli/src/auth.rs` does HTTP via `Command::new("curl")` and polls with `std::thread::sleep` — a deliberate zero-tokio dodge (there is no compio runtime in the CLI). Forcing the async `AuthProvider` enum into it would either drag a compio executor into the CLI or fork "one trait, two impls." Instead the CLI selects the **device protocol** by provider, both driven by the **same existing sync `curl` `post_form`/`poll_for_token`** — only the URLs/params differ:

- **Hydra:** today's native device grant — `POST {auth}/oauth2/device/auth`, poll `{auth}/oauth2/token` with `grant_type=…:device_code`. Unchanged.
- **Supabase:** the platform-mediated flow — `POST {control}/api/device/auth`, poll `POST {control}/api/device/token`. Same HTTP shape, different endpoint. A small sync `enum CliDeviceFlow { Hydra, Supabase }` picks the URLs; nothing async, nothing cyper. The "two impls" live **server-side** (Hydra sidecar vs control's device endpoints), not in the CLI.

**The platform-mediated flow (Supabase path):**

<!-- Rewritten in round 2: MAJOR-2 — name the concrete GoTrue mechanism. The browser (not control) obtains the GoTrue session via GoTrue's own PKCE flow; control never mints a GoTrue token. -->
The session is minted by **GoTrue's own PKCE browser flow — control mints nothing**. The device-approval page is itself an ordinary GoTrue PKCE client (the same flow `supabase-js` runs with `flowType: 'pkce'`); the only platform-specific step is binding the resulting GoTrue **refresh token** to the `device_code`. This uses **zero admin/unproven GoTrue surface** — just the standard `authorize` → `token?grant_type=pkce` exchange every Supabase web app already uses.

1. CLI `POST {control}/api/device/auth` → control mints a high-entropy `device_code` and a low-entropy `user_code`, stores the pending record (keyed `sha256(device_code)`, §6.1), and returns `verification_uri = {auth-ui}/device`, `interval`, `expires_in`.
2. User opens the URI on the platform device-approval page. **The page runs the GoTrue PKCE flow in the browser:** it generates a PKCE `code_verifier`/`code_challenge`, then authenticates the human directly against GoTrue — magic-link / social (`GET {supabase}/auth/v1/authorize?...&code_challenge=…&code_challenge_method=s256`) or password (`POST {supabase}/auth/v1/token?grant_type=password`, PKCE-wrapped). On success the browser holds a GoTrue `code`, which it exchanges at **`POST {supabase}/auth/v1/token?grant_type=pkce`** (body `{ auth_code, code_verifier }`, `apikey: SUPABASE_ANON_KEY`). The page now holds a genuine GoTrue session: `{ access_token, refresh_token, expires_in, user }`.
3. The page then **approves the device**: `POST {control}/api/device/approve` with `{ user_code, refresh_token }` under the existing `__Host-zsidp_csrf` double-submit guard (§6.1). Control (a) verifies the `access_token` once via `verify_token` to read the GoTrue `sub`/`email`/`email_verified`, (b) runs **JIT provisioning + linking** (§3.2) and writes `identity_links` + `principal_grants`, and (c) stores the **GoTrue `refresh_token`** (encrypted at rest, AES-256-GCM via `core::crypto`, exactly like the V0006 `app_session_anchors.refresh_token_enc` does for the per-app anchor) on the pending `device_code` record. **The durable bound credential is the GoTrue refresh token — control issues no token of its own.**
4. CLI polls `POST {control}/api/device/token` with the `device_code`; once approved, control redeems the stored refresh token at **`POST {supabase}/auth/v1/token?grant_type=refresh_token`** to obtain a fresh short-lived access token, deletes the one-time refresh binding from the pending record, and returns `{ access_token, refresh_token, expires_in }` to the CLI (the standard RFC-8628 device-token response shape). The CLI persists both, exactly as today.

<!-- Added in round 1 (M3), refined in round 2 (MAJOR-2): the deploy bearer is a GoTrue access token obtained via GoTrue's PKCE flow; control introspects it via verify_token. -->
**The deploy bearer is a real GoTrue access token — not a platform PAT, and control never forged it.** The original minted a control PAT here, which meant `verify_token` (the entire P-S1 deliverable) was **never exercised in the MVP**. Now the token is one GoTrue itself issued through its PKCE flow (step 2–4); `zeroship deploy` sends the GoTrue access token; `authz_guard` falls past the PAT branch (it isn't a PAT) into `oauth_guard_from_bearer` → `verify_token` (Supabase local verify, §4) → identity bridge (§3) → policy. Deploy **does** exercise the verify+bridge path.

**CLI refresh under Supabase (provider-selected endpoint).** The CLI already refreshes (`cli/src/auth.rs:106-138`, `grant_type=refresh_token`) but **hardcodes the `/oauth2/token` path** (Hydra). Under Supabase the refresh target is `{supabase}/auth/v1/token?grant_type=refresh_token` and requires the `apikey: SUPABASE_ANON_KEY` header. So the stored credential record gains the **token endpoint + anon apikey** (returned by the device-token response), and the same sync `enum CliDeviceFlow` that selects the device URLs also selects the refresh URL + header — no async, no cyper, a small additive change to the credential struct and `load_credentials`. GoTrue access tokens are short-lived (1h default; we set `GOTRUE_JWT_EXP` to minutes for platform tokens, §5), so storing and refreshing the **refresh token** is what carries the session — which is precisely what step 3 binds.

### 6.1 Device-flow security parameters (RFC 8628, made concrete)

The original keyed the pending record on the low-entropy `user_code` and specified no parameters. Corrected:

- **Poll secret = `device_code`, never `user_code`.** `device_code` = 32 random bytes (**≥256-bit**) from a CSPRNG, base64url. The pending record is keyed/looked-up by `sha256(device_code)` (stored **hashed at rest**, like PATs, so a DB read leaks no pollable secret). `user_code` is a *separate, secondary* index used only by the human-facing approval page.
- **`user_code`:** 8 chars from the RFC-8628 ambiguity-free alphabet `BCDFGHJKLMNPQRSTVWXZ`, formatted `XXXX-XXXX`. It is a one-time selector for an *already-authenticated* approval, never a poll secret.
- **TTL:** `device_code`/`user_code` expire in 600 s; expiry → `expired_token`.
- **Poll rate-limit / backoff:** `interval = 5 s`; `slow_down` adds +5 s (the CLI already honours this, `cli/src/auth.rs:214`); control rate-limits per `device_code` and caps total attempts, returning `slow_down`/`access_denied` past the cap.
- **`user_code → user` binding + CSRF:** the approval POST binds `user_code` to the authenticated principal **and** carries the existing `__Host-zsidp_csrf` double-submit token. `crates/auth/src/ui/device.rs:28-34` already implements this guard for the Hydra device-confirmation POST (BFF §5.4 names device confirmation as a CSRF target); the platform-mediated page **reuses the same `crate::csrf` module** rather than throwing it away — so a logged-in attacker cannot approve a victim's `user_code` via a forged cross-site POST.

---

## 7. Gateway OAuth machinery under Supabase — what degrades, stated explicitly

<!-- Added in round 1: addressing MAJOR M4 — OidcRp is far richer than a 4-method trait; the abstraction must not silently drop DPoP / RFC-9068 binding / refresh-anchor / pairwise. -->
`gateway/src/oidc_rp.rs` (1560 LOC) is **not** four methods. Collapsing it into a neutral trait would hide the features that don't survive the swap. The neutral `AuthProvider` enum stays **narrow** (token verification + issuer); the gateway's rich end-user OAuth path stays **concrete on the Hydra arm**, and here is exactly what the Supabase arm does for each, with the consequence:

| Gateway behavior (Hydra) | Code | Under Supabase | Consequence |
| --- | --- | --- | --- |
| **DPoP-bound introspection** | `introspect_token` (320-382), `core::dpop::verify` | **No equivalent** — GoTrue has no DPoP. Bearer-only. | **Scoped out.** Sender-constrained tokens are lost on the Supabase path; documented, not silent. |
| **RFC-9068 raw access-JWT + per-app `client_id` binding** | `verify_access_token`/`verify_access_jwt` (605-779), `AccessClaims.client_id` (624-626) | GoTrue tokens carry **no `client_id` claim**. Per-app binding must use the platform's own app/session binding, not the token. | Behavior change: per-app token binding moves to the platform layer (§7c), not the token. |
| **Server-held refresh anchor / family-revocation** | `refresh_token_public` (416-490), 720 h ceiling, `invalid_grant` family detection | GoTrue owns refresh rotation + reuse detection natively. | Behavior change: family-revocation maps onto **GoTrue session revocation** (+ the §5 deny-list); the gateway's anchor model is Hydra-specific and not reproduced. |
| **Pairwise / sectoral `pws_` subjects** | `AccessClaims.sub` doc (620-623): "Slice 4 projects this to a per-app `pws_`; **Slice 1c uses it directly (no pairwise derivation yet)**" | GoTrue emits **one global UUID to every app**. | **Privacy property preserved by the platform, not GoTrue — as designed future work.** <!-- Corrected in round 2: MINOR-4 — pws_ is a DESIGNED, not-yet-shipped gateway slice (oidc_rp.rs:621-623). --> The `pws_` projection is a **planned platform-side derivation (Slice 4, not yet shipped** — `oidc_rp.rs:621-623` documents that Slice 1c uses the global `sub` directly, no pairwise derivation today**). It is IdP-independent by construction**: because `pws_` derives from *any* global `sub`, it does not depend on Hydra. So when Slice 4 ships it implements `pws_ = HMAC(per-app-sector-salt, global-sub)` at the BFF projection layer for **both** providers (Hydra's `usr_…` and GoTrue's global UUID alike), preserving cross-app unlinkability. Under Supabase this is **not an existing function to call** — it is the same unshipped slice, which is exactly why the cross-app-unlinkability assertion is gated in P-S3, not the MVP. Without it, per-app OAuth on either provider would be a real cross-app correlation regression. |

**Net:** the MVP (platform auth = Hydra → Supabase for control/deploy + CLI) does **not** depend on per-app end-user OAuth, so DPoP / RFC-9068 / refresh-anchor only matter for **§7c (per-app OAuth on GoTrue)**, which is a designed follow-up — and the one privacy-critical property (pairwise) is preserved by the *planned* platform-side `pws_` derivation (Slice 4, not yet shipped) rather than dropped, IdP-independently.

### 7c. Per-app end-user OAuth (follow-up, designed — not a one-sentence wave)

When an end-user logs into a *creator's app*, Hydra issues tokens for a per-app registered client with consent delegation. GoTrue has no client model (matrix). The follow-up design: `ensure_client` records the app's client in **control's DB** (a platform record, **not** a Hydra DCR object — see m1 below); the per-app authorize/consent/token flow is served by the **gateway BFF + platform consent UI** in front of a GoTrue session; identity flows through the §3 bridge; the per-app subject is the §7 `pws_` derivation. This is a full platform-side OAuth AS and is **gated as its own phase** (P-S3) with its own e2e, not folded into the MVP.

---

## 8. Phasing & verification (each phase proves what it ships)

<!-- Added in round 1: addressing MAJOR M2 (P2 contradiction) + M3 (MVP gates prove the wrong thing) + m4 (named negative tests). -->

| Phase | Deliverable | Gate (must be green before next phase) |
| --- | --- | --- |
| **P1** | `core::auth_provider`: the `AuthProvider` enum, `VerifiedToken`, `ProviderAuthz`. **No call sites changed.** | `cargo build` workspace; `cargo test -p zeroship-core`. |
| **P2** | `HydraProvider` wraps today's `HydraIntrospector` / `OidcRp` / **both** existing client-writers (control's ad-hoc cyper calls **and** `auth`'s `HydraAdmin`); control/gateway/CLI route through the enum. **Pure behavior-preserving extraction — the two client-writers are kept as-is, NOT unified.** | Full per-crate suites green: core, control, gateway, auth, cli. `oidc_rp_e2e` green. This is a mechanical-extraction proof. |
| **P-unify** *(separate, later)* | Unify control's client-writer and `auth::HydraAdmin` onto one `ClientRegistry`. | A **behavioral-equivalence** test (same admin calls → same Hydra requests) — *not* "existing tests pass." Deferrable independently of the Supabase work. |
| **P3** | `ProviderKind` selector + `ZEROSHIP_AUTH_PROVIDER` + fail-closed boot validation (§2). | Boot each service `=hydra` (unchanged) and `=supabase` (config parses, mode pinned, missing/ambiguous config **fails closed**) in unit tests. |
| **P-S1** | `SupabaseProvider::verify_token`: **HS256 path (default)** + asymmetric/JWKS path; the §3 identity bridge (`identity_links`, `principal_grants`, JIT with the `email_verified` merge gate); the **`oauth_guard_from_bearer` call-site refactor** (MINOR-6: `hydra_introspector` → `Rc<AuthProvider>`, `provider_authz` split, PAT branch untouched); the **control-side deny-list** (`revoked_sessions` + in-process TTL cache, §5). | **Negative-test suite (m4):** `alg=none` rejected; **HS256-token-against-JWKS-mode** rejected (key-confusion); **asymmetric-token-against-HS256-mode** rejected; wrong-`iss`; `exp`/`nbf`; `aud != "authenticated"`; kid-spoof; unlinked-`sub` rejected (not provisioned); **`email_verified=false` collision → fresh principal, NOT merged (MAJOR-1)**; deny-listed `session_id` rejected at control's boundary within `DENY_CACHE_TTL` (MAJOR-3); gateway edge path does **not** consult the deny-list. |
| **P-S2** | Platform-mediated device flow (§6): control device-authorization endpoints (`/api/device/{auth,approve,token}`) with the §6.1 security parameters; the **browser GoTrue PKCE** approval page (`authorize` → `token?grant_type=pkce`, MAJOR-2) that binds the GoTrue **refresh token** to the `device_code`; CLI `--provider=supabase` incl. the provider-selected refresh endpoint. | **e2e:** `zeroship login --provider=supabase` against a **self-hosted (HS256) GoTrue container** → browser PKCE session (no GoTrue admin call) → `zeroship deploy`, where deploy is authorized **by `verify_token` + the §3 bridge** (proves the introspect path is exercised, not bypassed) and CLI refresh hits `{supabase}/auth/v1/token?grant_type=refresh_token`. Plus: `device_code` entropy ≥256-bit + hashed at rest; pending record keyed on `device_code`, not `user_code`; CSRF double-submit enforced on `approve`. |
| **P-S3** *(follow-up)* | Per-app end-user OAuth on GoTrue (§7c), incl. the `pws_` pairwise derivation. | Per-app login e2e on the supabase path; cross-app subject-unlinkability assertion (two apps see different `pws_` for one GoTrue user). |

Each phase: TDD, a regression test per behavior, commit-only, never push. Anything auth-touching gets the named negative-test gate above — "adversarial review" is now a concrete vector set, not a slogan.

---

## 9. Risks & non-goals

- **Risk: weakening token validation.** Local JWT verify (Supabase) must pin `iss`, pin `aud="authenticated"`, **pin the alg to the configured mode** (HS256 *xor* asymmetric — the ambiguous-mode key-confusion foothold is closed at boot, §2), enforce `exp`/`nbf`, and reject `alg=none`. HS256 mode is new code with its own negative suite (§8 P-S1); asymmetric mode reuses `core::oidc_verify`.
- **Risk: revocation latency (DOWNGRADE).** Local verify cannot revoke before `exp`. Mitigated by short access-token TTL + refresh **and** the `session_id` deny-list (§5). Residual: GoTrue-side revocations the platform never observes wait out the short TTL. Explicitly *not* "strictly better than Hydra."
- **Risk: identity-bridge correctness.** JIT provisioning runs only inside the authenticated device-approval step, never from a bare bearer; an unlinked `sub` is rejected, not auto-provisioned (§3.2). The `(provider, provider_subject)` PK makes concurrent first-logins idempotent.
- **Risk: account-linking takeover (MAJOR-1).** Merging a new IdP subject onto an existing principal by email is the classic takeover surface (Auth0/Okta/Microsoft advisories). Closed by the `email_verified` substrate field (§2/§4): §3.2 merges into an existing principal **only** on `email_verified == true`; an unverified or absent email yields a distinct fresh principal. P-S1's negative suite asserts the unverified-collision case does not merge.
- **Risk: P2 over-reach.** P2 is a pure extraction that **keeps both client-writers**; unification is the separate P-unify with a behavioral-equivalence gate (M2). The two are no longer conflated.
- **Privacy regression (mitigated, future work):** GoTrue's global `sub` would correlate a user across creator apps. The fix is the planned `pws_` projection (Slice 4, **not yet shipped** — `oidc_rp.rs:621-623`), which derives `pws_ = HMAC(sector-salt, global-sub)` platform-side for *either* provider. Because per-app end-user OAuth is itself a follow-up (P-S3), this is a gated assertion (two apps see different `pws_` for one GoTrue user), not an MVP property — and it is IdP-independent, so Supabase introduces no new requirement Hydra didn't already have.
- **Non-goal:** migrating an existing Hydra deployment's data to Supabase. Pre-launch; the selector is chosen at deploy time.
- **Non-goal:** running both providers simultaneously in one deployment. One `ZEROSHIP_AUTH_PROVIDER` per instance.
- **Non-goal (this proposal):** Auth0/Clerk/Keycloak. The enum + bridge make them addable later (a new arm + an `identity_links` provider value), not a rewrite.

<!-- Added in round 1: addressing MINOR m1 — OAuth2Client is Hydra-shaped, no GoTrue analog. -->
- **Note (m1): `OAuth2Client` is Hydra-shaped, not neutral.** A Hydra admin client (redirect_uris, grant_types, token_endpoint_auth_method) is an OAuth-DCR object with no GoTrue analog. It is **not** moved to core as a "provider-neutral" type. Under Supabase the per-app client is a **platform-DB record** with a different lifecycle (§7c); the Hydra type stays in `auth/src/hydra_client/types.rs` behind the Hydra arm.

---

## 10. Why this shape (vs. alternatives)

- **Alt A — "GoTrue straight-swaps Hydra":** rejected. GoTrue lacks device grant, consent delegation, client registration, DPoP, RFC-9068 binding, pairwise subjects, and (the real killer) a platform-principal identity. Pretending otherwise breaks CLI deploy, per-app OAuth, and authorization.
- **Alt B — adapter emulating Hydra's admin API in front of GoTrue:** rejected. A fake `/admin/oauth2/*` shim is more code and more fragile than moving consent/device/identity into the platform where they belong.
- **Alt C — `Arc<dyn AuthProvider>` async-trait seam (original draft):** rejected. Does not compile (async fn in `dyn` trait), and `#[async_trait(?Send)]` adds a per-call heap alloc on the `!Send` verify hot path. The repo already rejected this in `plugin-db` for the same reason.
- **Alt D (chosen) — enum dispatch at job B (repo idiom), platform owns identity-bridge + device-grant + consent, narrow neutral surface with explicitly-enumerated degradations:** the architecturally honest seam. A third provider is a new enum arm + an `identity_links` value, not a rewrite.

---

## Revision log (round 1)

Mapping each reviewed flaw (8 must-fix + the identity-mapping gap) to its resolution:

1. **C1 — `async fn` in `dyn` trait + `!Send` cyper.** Replaced the `Arc<dyn TokenProvider>` design with **enum dispatch** (`enum AuthProvider { Hydra, Supabase }`, inherent `async fn`s, `match self`) — the exact idiom `plugin-db/src/backend/mod.rs:28-31` chose to avoid `Box<dyn Future>` per call over the single-threaded compio stack. No `dyn`, no `async_trait`, no per-call alloc, `!Send` preserved (§2).
2. **C2 — `IntrospectResult` semantically wrong for `authz_guard`.** Verify now returns a provider-native `VerifiedToken`, **not** an authz result. §3 adds the bridge: `aud` check is provider-specific (GoTrue `aud="authenticated"` validated in verify, platform OAuth-aud only on the Hydra branch), `sub`→principal via `identity_links`, policy from `principal_grants` not the token. The false "same result" claim is retracted and replaced with a real mapping.
3. **C3 — GoTrue HS256/no-JWKS default.** §4 adds **two boot-pinned modes**: a **new HS256 symmetric verify path** (`SUPABASE_JWT_SECRET`, `DecodingKey::from_secret`) for the default/self-hosted case, and the asymmetric/JWKS path (reusing `core::oidc_verify`) for hosted/asymmetric. States exactly which verify code is reused (asymmetric) vs new (HS256), and retracts "just reuse the hardened verifier" for HS256.
4. **C4 — revocation latency.** §5 (new) + a risk entry: reframed as a **downgrade**, mitigated by short access-token TTL + refresh **and** a `session_id` **deny-list** (the provider-neutral analog of `hydra.rs:105 invalidate_by_sub`). "Strictly better" deleted.
5. **M3 — MVP gates prove the wrong thing.** §6: the Supabase deploy bearer is now a **real GoTrue access token** that control validates via `verify_token` (local verify) + the §3 bridge — the PAT-minting bypass is removed, so P-S1's introspect path is actually exercised by deploy. §8 re-phased so every phase's gate proves what it ships (P-S1 = verify+bridge negative suite; P-S2 = device→GoTrue-session→deploy-authorized-by-verify).
6. **M2 — P2 contradiction.** Split into **P2** (behavior-preserving extraction, **both** client-writers kept) and **P-unify** (separate, later, gated by a **behavioral-equivalence** test). §3 phase table + risk note.
7. **M4 — `OAuthFlowProvider` too thin.** §7 (new) enumerates DPoP, RFC-9068 `client_id` binding, refresh-anchor/family-revocation, and pairwise `pws_` — each with its Supabase consequence. DPoP scoped out (stated); RFC-9068/anchor become platform-layer behaviors; **pairwise is preserved** by platform-side `pws_=HMAC(sector-salt, global-sub)` derivation, turning a silent privacy regression into a kept property. Gateway's rich path stays concrete on the Hydra arm; the neutral enum stays narrow.
8. **M5 — CLI sync/curl.** §6: the CLI **never holds the async `AuthProvider`**; a sync `enum CliDeviceFlow` selects Hydra (native device grant) vs Supabase (platform-mediated) URLs, both over the existing sync `curl` `post_form`/`poll_for_token`. The "two impls" are server-side. §6.1 fixes the device-flow security: poll secret keyed on **`device_code`** (≥256-bit, stored hashed), `user_code` 8-char ambiguity-free as a secondary selector, 600 s TTL, interval/`slow_down`/attempt-cap, and **reuse of the existing `__Host-zsidp_csrf` double-submit** guard on the approval POST.
- **Identity-mapping gap (Missing-concept #1).** §3 (new, dedicated): `identity_links` table, JIT provisioning/account-linking, and `principal_grants` as the platform-side source of deploy scopes. This is the formerly-invisible hard problem made explicit.
- **Also addressed:** m1 (`OAuth2Client` Hydra-shaped, not moved to core — §9 note + §7c), m2 (fail-closed boot validation — §2), m3 (logout/revocation Supabase story — §5 deny-list), m4 (named negative-test vector set — §8 P-S1 gate), and Missing-concept #8 (operational signals differ: JWKS/HS256-rotation failure + deny-list growth replace introspection-latency/`active=false` — §5).

## Revision log (round 2)

Mapping each round-2 must-fix (3 MAJOR + 3 MINOR) to its resolution. Grounded against the real tree and against GoTrue's actual PKCE API.

1. **MAJOR-1 — `VerifiedToken` could not express verified-email; email-merge was an account-takeover vector.** Added `email_verified: bool` to `VerifiedToken` (§2), populated from the GoTrue `email_verified`/`email_confirmed_at` claim and defaulting **false** (§4); the Hydra arm fills it from the existing `AccessClaims.email_verified` (`oidc_rp.rs:632`). §3.2 now **refuses email-based merge unless `email_verified == true`** — an unverified/absent-email collision creates a *distinct fresh* principal instead of inheriting the victim's grants. New §9 risk entry + P-S1 negative test ("`email_verified=false` collision → fresh principal, not merged").

2. **MAJOR-2 — §6 step 3 named no concrete GoTrue session-mint mechanism.** Rewrote §6 steps 1–4 to use **GoTrue's own PKCE browser flow — control mints nothing**: the device-approval page is an ordinary GoTrue PKCE client (`GET /auth/v1/authorize` or `POST /auth/v1/token?grant_type=password` → `POST /auth/v1/token?grant_type=pkce`), the *browser* obtains `{access_token, refresh_token}`, and the page binds the GoTrue **refresh token** to the `device_code` (encrypted at rest like V0006's `app_session_anchors.refresh_token_enc`). The CLI poll redeems it via `grant_type=refresh_token` and returns a real GoTrue session. **No GoTrue admin endpoint is required** — only the standard Supabase web-app exchange. Also specified the CLI's provider-selected refresh endpoint (`{supabase}/auth/v1/token?grant_type=refresh_token` + `apikey` header), since `cli/src/auth.rs:114` currently hardcodes the Hydra `/oauth2/token` path. P-S2 gate updated to assert "browser PKCE session, no admin call."

3. **MAJOR-3 — the deny-list re-introduced a per-verify lookup with unspecified wiring/staleness.** Rewrote §5 layer 2: **`verify_token` stays pure local-verify (no deny-list lookup)** so the `!Send` core seam needs no DB handle and the gateway edge path stays round-trip-free. The deny-list moves to **control's authz boundary** (`oauth_guard_from_bearer`), using control's *existing* compio-postgres/compio-redis handle; data source is a `zeroship.revoked_sessions` set keyed on GoTrue `session_id`/`sub`, populated on logout/ban; fronted by an **in-process `Rc<RefCell<DenyCache>>` refreshed every `DENY_CACHE_TTL` (default 10 s)** via a compio interval task. Made the tradeoff explicit: control-path revocation window ≤ `DENY_CACHE_TTL`; gateway-path window = access-token TTL (no deny-list there by design). Zero-tokio throughout. §2 enum comment + capability matrix corrected to "control-side, not in `verify_token`."

4. **MINOR-4 — §7 overstated `pws_` as "already a platform-side derivation."** Corrected to **"planned platform-side derivation (Slice 4, not yet shipped)"**, citing `oidc_rp.rs:621-623` ("Slice 1c uses it directly, no pairwise derivation yet"). Stated it is IdP-independent (derives from any global `sub`, so no new Supabase requirement) and that the cross-app-unlinkability assertion is gated in P-S3, not assumed to exist. §9 privacy risk + §7 "Net" reworded to future tense.

5. **MINOR-5 — `identity_links` not reconciled with existing per-app identity tables.** Added a boundary table to §3.1 distinguishing **`identity_links`** (net-new; external-IdP-subject × *platform principal*; control/deploy) from **`app_user_identities`** (V0009; *end-user* × creator-app; per-app `pws_` projection) and **`app_session_anchors`** (V0006; per-app refresh families). Stated they share only the `zeroship.users(id)` anchor and that **P-S3 must extend V0009 (add a `provider` discriminator), not fork a parallel table**.

6. **MINOR-6 — the `authz_guard` change is a call-site refactor, not "a single new branch."** Reworded §3.3 to describe the real edit at `oauth_guard_from_bearer` (`authz_guard.rs:250-295`): swap `hydra_introspector` → `Rc<AuthProvider>`, fold the `active` flag into verify's success/error, leave the PAT branch untouched, split post-verify on `provider_authz`. P-S1 deliverable re-scoped to name the call-site refactor.

## Revision log (round 3 — GoTrue claim-shape source research)

Before implementing P-S1, the GoTrue access-token claim shape was verified against the `supabase/auth` source (`internal/api/token.go` `AccessTokenClaims`) + official docs, not assumed. Two round-2 assumptions were **unsafe** and are corrected above (§4); they do **not** affect the P1+P2 Hydra slice (the `VerifiedToken` field set is unchanged — only how the *Supabase arm* fills two of them):

1. **`email_verified` is not a GoTrue JWT claim (the load-bearing correction).** Round 2 had the Supabase arm fill `email_verified` from a token claim. There is no such trustworthy claim — only a stale user-writable `user_metadata.email_verified` mirror (supabase/auth #1620). Authoritative state is `email_confirmed_at` on the user record, via the **service-role admin API** (`GET /auth/v1/admin/users/{id}`). §4 now routes the §3.2 account-link verified-email gate through an **admin lookup at link time** (off the hot path), not the JWT. MAJOR-1's *intent* (never merge on unverified email) is preserved; only the *source of truth* changed.
2. **`iss` is config-dependent, not `{url}/auth/v1`.** Self-hosted GoTrue's issuer defaults to the literal `"supabase"` (`GOTRUE_JWT_ISSUER`). §4 now pins `iss` from `SUPABASE_JWT_ISSUER` config and never hardcodes it. `aud` is matched tolerantly (it is a `ClaimStrings` that can be an array). 

Confirmed-good assumptions (no change): `session_id` is reliably present in modern GoTrue (deny-list key holds, §5); default access-token TTL 3600 s (short-TTL revocation mitigation holds, §5); self-hosted default signing is HS256+shared-secret with no kid/JWKS (Mode A holds, §4); the browser PKCE exchange (`grant_type=pkce`/`password` → `refresh_token`) needs no admin endpoint (§6 holds). Full findings: `docs/proposals/.gotrue-claims-research.md`.
