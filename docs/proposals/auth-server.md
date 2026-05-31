# zeroship auth server (`auth.zeroship.ai`) — proposal

**Status:** proposal, in review (revision 2 — hydra-based)
**Date:** 2026-05-26
**Branch:** `proposal/auth-server`
**Worktree:** `.claude/worktrees/auth-server`
**Replaces:** the in-process creator auth currently embedded in `crates/control/src/auth_*.rs`

This document proposes the v1 design for `crates/auth` (the zeroship Identity Provider). It is grounded in five upstream research briefings produced on 2026-05-26 (OIDC/OAuth spec stack, cryptographic core, identity flows + mailer, reference IdPs + threat model, and ory/hydra integration). Citations are inline.

Per the project's proposal workflow: this file lives in a fresh worktree off main and is **not committed** until the implementing PR lands.

---

## 0 · One-line product framing

> **`auth.zeroship.ai` is the zeroship platform's single Identity Provider.** It uses **ory/hydra** as the OIDC/OAuth 2.1 protocol kernel and ships a Rust crate `crates/auth/` that owns everything around it — login UI, consent UI, user store, identity flows (password, Google, GitHub, magic-link), mailer, audit log, and the gateway integration. Every other surface — the gateway (default Relying Party for hosted creator apps), the control plane (creator dashboard), the builder, and eventually third-party "Sign in with Zeroship" RPs — authenticates users by speaking OIDC to this server.

One identity per human, one global user pool, one place that ever holds raw passwords. Hydra is the only place that ever holds signing keys or issues tokens. Per AGENTS.md System 2: this fills the standalone **Auth Service** box.

**Why hydra?** Rolling the OIDC kernel from scratch is undifferentiated risk — RFC 9700 has ~60 MUST/SHOULD items, OIDC Core has subtle hash-claim semantics, and any miss is a CVE. Hydra is Apache-2.0, OpenID-Foundation-certified, postgres-backed, actively maintained (v26.2.13 on 2026-05-22), and explicitly designed for the "headless OIDC" pattern: we own UX and user data, hydra owns the protocol. The seam between them is a small Rust HTTP client against hydra's admin API.

---

## 1 · Foundation decisions

| # | Decision | Choice |
|---|----------|--------|
| 1 | Service form | New Rust crate `crates/auth/` — compio binary, sibling of gateway/control/worker. Pairs with a sidecar `oryd/hydra` container. |
| 2 | OIDC engine | **ory/hydra v25.4.0** (current Docker Hub tag; calendar versioning track since 2025-10). Apache-2.0, OpenID-Foundation-certified. Pinned by major-minor in deployment. Bump to v26.x when ory publishes that tag. |
| 3 | Tenancy model | Single global user pool. The `sub` claim is one zeroship-wide identity (typed_id `usr_…`) for every app and every internal surface. |
| 4 | Delegation model | OIDC Identity Provider. RPs include the gateway (default RP for hosted creator apps), the control plane, the builder, and third-party apps later. |
| 5 | Login methods at launch | Email+password, Google OIDC, GitHub OAuth, email magic-link. |
| 6 | Spec posture | OIDC Core 1.0 + OAuth 2.1 + RFC 9700. Implemented by hydra; we annotate our threat-model table with the boundary in §13. |
| 7 | ID-token signing | EdDSA (Ed25519) primary on hydra's `hydra.openid.id-token` keyset, RS256 secondary for compatibility. |
| 8 | Access-token format | JWT (`access_token_strategy=jwt` per first-party client), EdDSA-signed on hydra's `hydra.jwt.access-token` keyset, 1 h TTL. |
| 9 | Refresh tokens | Hydra-managed; **opaque** by design (always — for revocability). Rotation enforced; 30 s grace window for concurrent-refresh races. |
| 10 | Password hashing | Argon2id at OWASP 2026 params (m = 19 MiB, t = 2, p = 1). |
| 11 | Email transport | Pluggable `Mailer` trait: `StdoutMailer` (dev/CI) + `SmtpMailer` (lettre, default) + `ResendMailer` (HTTP). |
| 12 | Templating | `askama` (compile-time-checked). |
| 13 | UI | Server-rendered HTML for `/login`, `/consent`, `/logout`, `/verify`, `/error`. No SPA. Single CSS file. |
| 14 | Storage | One PostgreSQL schema `auth.*` for **our** state (users, identities, magic-links, sessions, audit). Hydra owns its own `hydra_*` schema in the same database. |
| 15 | Hydra admin client | Hand-rolled thin client over the existing `cyper` compio HTTP stack. ~12 typed structs. We do **not** vendor `ory-hydra-client` (auto-generated, pulls `reqwest`/`tokio`). |
| 16 | Audit | Structured JSON event log to PG `auth.audit_events` + stdout JSON for SIEM ingestion. Hydra's own logs are independent. |
| 17 | Cookie domain | Hydra session and our login session both live on `auth.zeroship.ai` (Hydra public via `serve.cookies.domain=auth.zeroship.ai`, our cookie via `__Host-` prefix). Both behind the gateway on a single registrable subdomain so SameSite=Lax flows survive the OAuth dance. |

Everything in this table is load-bearing for the implementation plan. Detailed reasoning lives in the relevant sections below.

**Abstraction boundary.** The `crates/auth/src/hydra_client/` module is the only code that knows hydra's wire format. If a future decision swaps the OIDC kernel (back to roll-our-own, or to dex/zitadel/keycloak), only this module + the deployment topology change.

---

## 2 · System architecture

### 2.1 Where the components sit

```
End-user browser
       │
       │  1. GET https://myapp.zeroship.ai/anything
       ▼
┌──────────────────┐
│  crates/gateway  │  no session at myapp.zeroship.ai →
│  (OIDC RP for    │  302 to https://auth.zeroship.ai/oauth2/auth?...
│   hosted apps)   │
└─────┬────────────┘
      │
      │  6. once authed: ZeroShip-User (HMAC, base64 JSON)
      ▼
┌──────────────────┐
│  crates/worker   │  app code reads env.auth.getUser()
└──────────────────┘

                          ╔════════════════════════════════════════════════════╗
                          ║                auth.zeroship.ai                     ║
                          ║          (single host, gateway terminates TLS)      ║
                          ║                                                      ║
                          ║   /oauth2/*          ╲                              ║
                          ║   /.well-known/*      ──► oryd/hydra (port 4444)    ║
                          ║   /oauth2/sessions/*  ╱                              ║
                          ║                                                      ║
                          ║   /login                ╲                            ║
                          ║   /signup               ╲                            ║
                          ║   /consent              ──► crates/auth (port 9092) ║
                          ║   /logout               ╱                            ║
                          ║   /magic/verify         ╱                            ║
                          ║   /oauth/{google,github}/{start,callback}            ║
                          ║   /webhooks/{postmark,ses-sns}                       ║
                          ╚═════════╪═══════════════════╪══════════════════════╝
                                    │ 127.0.0.1:4445    │ compio-postgres
                                    │ (admin API)        │
                                    ▼                    ▼
                          ┌──────────────────┐  ┌──────────────────────────┐
                          │   crates/auth    │  │  shared zeroship PG       │
                          │  hydra_client    │  │  ┌──────────┬──────────┐ │
                          │  identity flows  │  │  │ hydra_*  │  auth.*  │ │
                          │  mailer + UI     │  │  └──────────┴──────────┘ │
                          └──────────────────┘  └──────────────────────────┘
```

Two processes share a host and a database; they speak through hydra's admin API on loopback. The browser only ever talks to `auth.zeroship.ai` — it never sees Hydra directly.

### 2.2 End-user login flow (full sequence)

For an end-user hitting a creator app:

```
1.  Browser → myapp.zeroship.ai/some-page
2.  Gateway: no __Host-zeroship_app_session cookie → 302
       https://auth.zeroship.ai/oauth2/auth?
         client_id=myapp_<id>&response_type=code&scope=openid+email+profile&
         redirect_uri=https://myapp.zeroship.ai/__zeroship/auth/callback&
         state=<rand>&nonce=<rand>&code_challenge=<S256>&code_challenge_method=S256

3.  Browser → hydra at /oauth2/auth
       Hydra checks its own session cookie at auth.zeroship.ai (ory_hydra_session_*).
       Missing → 302 to https://auth.zeroship.ai/login?login_challenge=<opaque>

4.  Browser → crates/auth /login
       GET hydra-admin /admin/oauth2/auth/requests/login?login_challenge=<...>
         → { skip: false, subject: "", client: { client_id, skip_consent }, ... }
       Render login form (or skip directly if skip=true).

5.  User submits credentials. crates/auth runs the identity flow (§8).
       Success → PUT hydra-admin /admin/oauth2/auth/requests/login/accept
                  { subject: "usr_01H…", remember: true, remember_for: 3600,
                    acr: "urn:zeroship:pwd", amr: ["pwd"], context: { … } }
                → { redirect_to: "https://auth.zeroship.ai/oauth2/auth?..." }

6.  Browser → hydra at /oauth2/auth (the redirect_to)
       Hydra sets ory_hydra_session_* cookie at auth.zeroship.ai.
       302 → https://auth.zeroship.ai/consent?consent_challenge=<opaque>

7.  Browser → crates/auth /consent
       GET hydra-admin /admin/oauth2/auth/requests/consent?consent_challenge=<...>
         → { skip: true, subject, client: { skip_consent: true }, requested_scope, ... }
       skip_consent=true (first-party RP) → immediately
       PUT hydra-admin /admin/oauth2/auth/requests/consent/accept
         { grant_scope: ["openid","email","profile"],
           grant_access_token_audience: ["https://api.zeroship.ai"],
           remember: true, remember_for: 3600,
           session: {
             id_token:     { email, name, picture, email_verified },
             access_token: {}    // intentionally empty — introspect leaks
           } }
         → { redirect_to: "https://auth.zeroship.ai/oauth2/auth?..." }

8.  Browser → hydra at /oauth2/auth (the redirect_to)
       302 → https://myapp.zeroship.ai/__zeroship/auth/callback?code=<...>&state=<...>

9.  Gateway at /__zeroship/auth/callback:
       POST https://auth.zeroship.ai/oauth2/token  (back-channel via cyper)
         grant_type=authorization_code, code, code_verifier, client_id, client_secret
         → { id_token, access_token, refresh_token, expires_in }
       Verify id_token signature against cached JWKS (auth.zeroship.ai/.well-known/jwks.json).
       Set __Host-zeroship_app_session at myapp.zeroship.ai (opaque session id, server-side state
         lives in gateway's per-app session store — see §9.2).
       302 → /some-page

10. Browser → myapp.zeroship.ai/some-page
       Gateway: __Host-zeroship_app_session ok → proxy to worker with
         ZeroShip-User: base64(JSON).hex-hmac
       Worker: env.auth.getUser() resolves to the JSON.
```

**Single sign-on falls out for free.** Step 3's "missing hydra session cookie" is the only place that triggers steps 4–5. Once the user signs in at any app, the next app's redirect to `/oauth2/auth` finds the cookie, `skip=true`, and silently runs steps 6–8. The user signs in to zeroship once, then signs in to every app invisibly (subject to consent for third-party RPs).

### 2.3 Internal app flow (console.zeroship.ai)

Same machinery, different relying party. `console.zeroship.ai` is registered with hydra as a first-party client (`skip_consent=true`, `require_consent=false`). The control plane is the OIDC RP that handles `/auth/callback` and sets its own per-origin cookie.

### 2.4 Logout flow

```
1.  Browser → console.zeroship.ai/sign-out
2.  Control plane clears its session cookie, redirects to
       https://auth.zeroship.ai/oauth2/sessions/logout?id_token_hint=<jwt>&
         post_logout_redirect_uri=https://console.zeroship.ai/&state=<rand>
3.  Hydra 302 → https://auth.zeroship.ai/logout?logout_challenge=<opaque>
4.  crates/auth: GET hydra-admin /admin/oauth2/auth/requests/logout?logout_challenge=<...>
       Render a confirmation if the client has require_logout_consent=true;
       else immediately PUT /admin/oauth2/auth/requests/logout/accept
       → { redirect_to: "https://auth.zeroship.ai/oauth2/sessions/logout?..." }
5.  Browser → hydra: clears ory_hydra_session_*, fires front-channel
       (iframe GET) + back-channel (POST logout_token) to every RP with
       *_logout_uri configured.
6.  302 → post_logout_redirect_uri.

For "sign out everywhere": after step 4, our crates/auth ALSO calls
   DELETE /admin/oauth2/auth/sessions/login?subject=usr_01H…
   so hydra additionally invalidates every other RP's record of this subject.
```

Note: back-channel logout fan-out is **best-effort** per ory/hydra issue [#3186] — hydra may finish the redirect before all RPs have ACKed. Our own session-revocation in `auth.sessions` is the authoritative "user signed out" signal for zeroship-internal surfaces; the back-channel notification is the polite "and please drop your cached state" message to RPs.

---

## 3 · Specification surface

### 3.1 What hydra owns (we configure, don't implement)

| Spec | Hydra status |
|---|---|
| OIDC Core 1.0 | Implemented; OpenID-Foundation certified. |
| OIDC Discovery 1.0 | `/.well-known/openid-configuration` served at hydra public port. |
| OAuth 2.0 AS Metadata (RFC 8414) | `/.well-known/oauth-authorization-server` since v25.4. |
| PKCE (RFC 7636) | `oauth2.pkce.enforced_for_public_clients: true` — we enable for **all** clients. |
| Authorization-code grant | Single-use, atomic; configurable TTL (we set 60 s). |
| Refresh-token rotation (RFC 9700) | Implemented; graceful rotation window `oauth2.grant.refresh_token.rotation_grace_period`. |
| JWT access tokens (RFC 9068) | Per-client `access_token_strategy="jwt"`. |
| ID-token signing | `hydra.openid.id-token` keyset; we issue EdDSA + RS256 keys via admin API. |
| JWKS (RFC 7517) | Served at `/.well-known/jwks.json`. |
| Introspection (RFC 7662) | `/oauth2/introspect`. |
| Token revocation (RFC 7009) | `/oauth2/revoke`. |
| RP-Initiated Logout 1.0 | `/oauth2/sessions/logout` + front/back-channel fan-out. |
| Subject identifiers | `subject_type=public`; `force_subject_identifier` optional. |
| Device Authorization Grant (RFC 8628) | Added in v25.4. We don't enable it at v1. |

### 3.2 What hydra **does not** do (we mitigate or accept)

| Gap | Workaround |
|---|---|
| **DPoP (RFC 9449)** sender-constrained tokens. | Not implemented in hydra OSS. Acceptable at v1 — our tokens are short-lived (1 h access, refresh always opaque + rotated). Re-evaluate when third-party RPs ship. |
| **RFC 9207** `iss` parameter in authorization response. | Not advertised by hydra. Our RPs (gateway, control) mitigate mix-up by validating `iss` claim **inside the ID token** instead of the URL parameter. Document for external RPs. |
| **JAR / PAR / JARM** (RFC 9101, 9126, JARM). | Out of scope for v1. Hydra has PAR support in newer releases if needed later. |
| **mTLS-bound tokens (RFC 8705)**. | Out of scope. |
| **OIDC Federation, CIBA, FAPI 1/2**. | Out of scope. |
| **Dynamic Client Registration**. | Hydra supports it; we leave it disabled at v1. |
| **Hydra has no built-in MFA/passkey logic**. | That's our domain anyway — `crates/auth` will own it when added (v1.1). The `acr`/`amr` fields propagate through `accept_login`. |

### 3.3 What we own (`crates/auth`)

- **User identity flows** — password (Argon2id), Google OIDC, GitHub OAuth, email magic-link, email verification, password reset.
- **Login / consent / logout UI** — server-rendered HTML; the bytes the user sees.
- **User store** — `auth.users`, `auth.identities`, `auth.sessions`.
- **Email pipeline** — `Mailer` trait, templates, suppression list, bounce/complaint webhooks.
- **Rate limiting and brute-force defense** — per-account, per-IP+email, per-IP.
- **Audit log** at the user-action level.
- **Hydra admin client** — `hydra_client::*` typed wrappers.
- **Bootstrap** — registering OIDC clients with hydra at first boot.
- **Federation OAuth callbacks** — Google and GitHub redirect targets at our host, not hydra's.

---

## 4 · Crate layout

```
crates/auth/
├── Cargo.toml
├── src/
│   ├── main.rs                 # binary entrypoint; flags & config
│   ├── config.rs               # AuthConfig
│   ├── server.rs               # ntex routes wiring
│   ├── error.rs                # AuthError; impls IntoResponse
│   │
│   ├── hydra_client/
│   │   ├── mod.rs              # HydraAdmin struct, ctor, base URL
│   │   ├── login.rs            # get_login / accept_login / reject_login
│   │   ├── consent.rs          # get_consent / accept_consent / reject_consent
│   │   ├── logout.rs           # get_logout / accept_logout
│   │   ├── clients.rs          # create_client / get_client / update_client / delete_client
│   │   ├── sessions.rs         # delete_login_sessions (by subject)
│   │   ├── jwks.rs             # admin JWK CRUD for first-time bootstrap + rotation
│   │   └── types.rs            # ~14 serde structs matching hydra's wire format
│   │
│   ├── identity/
│   │   ├── mod.rs
│   │   ├── password.rs         # Argon2id hash/verify, dummy-hash enumeration defense
│   │   ├── magic_link.rs       # issue + redeem, CSRF-bound cross-device flow
│   │   ├── verification.rs     # email verification (signup-time)
│   │   ├── breach.rs           # HIBP k-anonymity check
│   │   └── oauth/
│   │       ├── mod.rs          # shared PKCE+state machinery (federation, NOT our IdP)
│   │       ├── google.rs       # Google OIDC; cyper client
│   │       └── github.rs       # GitHub OAuth, /user + /user/emails
│   │
│   ├── sessions/
│   │   ├── mod.rs
│   │   └── login.rs            # __Host-zsidp_session at auth.zeroship.ai
│   │
│   ├── mailer/
│   │   ├── mod.rs              # trait Mailer
│   │   ├── stdout.rs           # dev/CI default
│   │   ├── smtp.rs             # lettre-backed
│   │   ├── resend.rs           # HTTP-API-backed via cyper
│   │   ├── bounce.rs           # webhook handlers + suppression
│   │   └── templates/          # askama (.html + .txt pairs)
│   │
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── login.rs            # GET/POST /login — reads login_challenge
│   │   ├── signup.rs           # GET/POST /signup
│   │   ├── consent.rs          # GET/POST /consent — reads consent_challenge
│   │   ├── logout.rs           # GET/POST /logout — reads logout_challenge
│   │   ├── verify.rs           # GET /verify, GET /magic/verify
│   │   ├── error_page.rs       # GET /error — hydra renders here on misconfig
│   │   ├── me.rs               # GET /me — linked-identities page
│   │   ├── static_assets.rs    # serves a single bundled CSS file
│   │   └── templates/          # askama login.html, consent.html, …
│   │
│   ├── store/
│   │   ├── mod.rs
│   │   ├── migrations.rs       # one-shot CREATE TABLE on boot
│   │   ├── users.rs            # auth.users
│   │   ├── identities.rs       # auth.identities
│   │   ├── sessions.rs         # auth.sessions (our login session)
│   │   ├── magic.rs            # auth.magic_links
│   │   ├── verifications.rs    # auth.email_verifications
│   │   ├── suppressions.rs     # auth.email_suppressions
│   │   ├── ratelimit.rs        # auth.rate_limits (PG token bucket)
│   │   └── audit.rs            # auth.audit_events
│   │
│   ├── bootstrap/
│   │   ├── mod.rs
│   │   ├── clients_config.rs   # parse static client config + POST to hydra
│   │   └── keys.rs             # first-boot JWK generation via hydra admin API
│   │
│   ├── audit.rs                # event emission helper
│   ├── headers.rs              # security-headers middleware
│   ├── ratelimit.rs            # per-account / per-IP buckets
│   └── csrf.rs                 # double-submit cookie helper
│
└── tests/
    ├── e2e_password.rs         # boot hydra+auth+pg; full code+PKCE flow with password login
    ├── e2e_google.rs           # google federation end-to-end (mocked Google with cyper)
    ├── e2e_github.rs           # github federation end-to-end
    ├── e2e_magic.rs            # magic-link issue+redeem same-device + cross-device
    ├── e2e_logout.rs           # rp-initiated logout + session delete propagation
    ├── enum_defense.rs         # equal-time, equal-message login failures
    ├── threat_model.rs         # parametric over the "ours" rows of §13
    └── hydra_client_smoke.rs   # round-trip every admin endpoint we use
```

`sdks/auth/` (the existing `@zeroship/auth` package) is reduced to a thin runtime helper that reads the platform-injected `env.auth.*` namespace; the browser fallback path in today's `src/index.ts` is deleted.

**What we removed vs revision 1:**

- `oidc/` subtree — hydra owns these endpoints.
- `crypto/` subtree — hydra owns signing keys and rotation.
- `store/codes.rs`, `store/tokens.rs`, `store/keys.rs`, `store/consents.rs`, `store/clients.rs` — hydra owns the corresponding state.
- `sessions/refresh.rs`, `sessions/codes.rs` — hydra owns.

**What we added:**

- `hydra_client/` — our typed Rust wrapper over hydra's admin API.
- `bootstrap/clients_config.rs` — declarative client registration via the admin API at first boot.

---

## 5 · Data model

One PG schema `auth.*` for **our** state. Hydra creates its own `hydra_*` tables in the same database via its migration tool — we never `SELECT` from those.

```sql
-- 5.1 Users — the single global identity. sub claim = users.id (formatted as typed_id).
CREATE TABLE auth.users (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email              CITEXT UNIQUE NOT NULL,
    email_verified_at  TIMESTAMPTZ,
    name               TEXT NOT NULL,
    avatar_url         TEXT,
    password_hash      TEXT,                    -- PHC string; NULL for oauth-only accounts
    locked_until       TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at      TIMESTAMPTZ
);

-- 5.2 OAuth identities linked to a user (keyed on (provider, subject), NEVER email)
CREATE TABLE auth.identities (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    provider        TEXT NOT NULL,              -- 'google' | 'github'
    subject         TEXT NOT NULL,
    email_at_link   CITEXT,
    raw_profile     JSONB,
    linked_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider, subject)
);

-- 5.3 IdP login session — the cookie at auth.zeroship.ai.
-- Note: this is DISTINCT from hydra's session. We track our verified-user state;
-- hydra tracks its OIDC subject state. They are coordinated through the
-- /admin/oauth2/auth/requests/login accept call (which sets BOTH).
CREATE TABLE auth.sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES auth.users(id),
    auth_method     TEXT NOT NULL,                            -- 'pwd' | 'google' | 'github' | 'magic'
    amr             TEXT[] NOT NULL,
    acr             TEXT,
    auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    idle_expires_at TIMESTAMPTZ NOT NULL,                     -- now() + 30 min sliding
    abs_expires_at  TIMESTAMPTZ NOT NULL,                     -- now() + 12 h hard
    revoked_at      TIMESTAMPTZ
);

-- 5.4 Magic-link tokens — 15 min TTL, single-use, CSRF-bound
CREATE TABLE auth.magic_links (
    token_hash      BYTEA PRIMARY KEY,
    email           CITEXT NOT NULL,
    csrf_nonce      TEXT NOT NULL,
    purpose         TEXT NOT NULL,                            -- 'login' | 'signup'
    request_ip      INET,
    request_ua      TEXT,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,
    consumed_at     TIMESTAMPTZ
);
CREATE INDEX auth_magic_email_idx ON auth.magic_links (email);

-- 5.5 Email verification tokens — 24 h TTL
CREATE TABLE auth.email_verifications (
    token_hash      BYTEA PRIMARY KEY,
    user_id         UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    email           CITEXT NOT NULL,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,
    consumed_at     TIMESTAMPTZ
);

-- 5.6 Email suppressions — bounces and complaints
CREATE TABLE auth.email_suppressions (
    email           CITEXT PRIMARY KEY,
    reason          TEXT NOT NULL,                            -- 'hard_bounce' | 'complaint' | 'manual'
    suppressed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    provider_msg    TEXT
);

-- 5.7 Rate-limit buckets (PG-backed token bucket; Redis later if QPS demands)
CREATE TABLE auth.rate_limits (
    bucket_key      TEXT PRIMARY KEY,
    tokens          REAL NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- 5.8 Audit events
CREATE TABLE auth.audit_events (
    id              BIGSERIAL PRIMARY KEY,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_type      TEXT NOT NULL,
    outcome         TEXT NOT NULL,                            -- 'success' | 'failure'
    user_id         UUID,
    client_id       TEXT,                                     -- hydra's client_id when relevant
    request_id      TEXT,
    ip              INET,
    user_agent      TEXT,
    auth_method     TEXT,
    detail          JSONB
);
CREATE INDEX auth_audit_user_idx  ON auth.audit_events (user_id, occurred_at);
CREATE INDEX auth_audit_event_idx ON auth.audit_events (event_type, occurred_at);
```

**What disappeared from revision 1:**

- `auth.codes`, `auth.refresh_tokens`, `auth.consents`, `auth.clients`, `auth.signing_keys` — all hydra-managed in the `hydra_*` schema.

**Sweeper.** A single in-process compio task sweeps every 60 s: marks `magic_links` and `email_verifications` past `expires_at` as consumed; deletes consumed/expired rows older than 7 days. No external cron. Hydra runs its own cleanup against `hydra_*` tables.

---

## 6 · Endpoint surface

### 6.1 Owned by hydra (proxied through the gateway at `auth.zeroship.ai`)

| Path | Owner |
|---|---|
| `/oauth2/auth` | hydra |
| `/oauth2/token` | hydra |
| `/oauth2/revoke` | hydra |
| `/oauth2/introspect` | hydra |
| `/oauth2/sessions/logout` | hydra |
| `/userinfo` | hydra (at `/userinfo`) |
| `/.well-known/openid-configuration` | hydra |
| `/.well-known/oauth-authorization-server` | hydra |
| `/.well-known/jwks.json` | hydra |

The gateway's manifest routes these prefixes to hydra's public port (`:4444`). Everything else at `auth.zeroship.ai` goes to `crates/auth` (`:9092`).

### 6.2 Owned by `crates/auth` — user-facing UI

| Method | Path | Purpose |
|---|---|---|
| GET, POST | `/login?login_challenge=…` | Reads challenge, renders form, calls accept_login on success. |
| GET, POST | `/signup` | Create account. Verifies email asynchronously. Carries `?login_challenge=…` if entered mid-flow. |
| GET, POST | `/consent?consent_challenge=…` | Reads challenge; renders form for third-party RPs, immediately accepts for first-party (`skip_consent=true`). |
| GET, POST | `/logout?logout_challenge=…` | Confirms (or skips) and calls accept_logout. |
| GET | `/magic/verify?t=…` | Redeems magic link; binds via `__Host-zsidp_magic_csrf`. If the magic flow was initiated mid-`login_challenge`, completes the OIDC handshake. |
| GET | `/verify?t=…` | Redeems email-verification token. |
| GET, POST | `/forgot` | Password-reset request (always 200; sends reset link if account exists). |
| GET, POST | `/reset?t=…` | Password-reset redemption. |
| GET | `/me` | Read-only profile / linked-identities page. |
| POST | `/me/link` | Link an additional OAuth provider; requires re-auth. |
| POST | `/me/unlink` | Unlink; rejected if it would leave the account credential-less. |
| GET | `/error?error=…&error_description=…` | Hydra's configured error landing; we render a friendly page. |

### 6.3 Owned by `crates/auth` — federation callbacks

| Method | Path | Purpose |
|---|---|---|
| GET | `/oauth/google/start` | Start Google OIDC flow. Carries the pending `login_challenge` in a short-lived cookie. |
| GET | `/oauth/google/callback` | Exchange + verify + link/create + accept_login. |
| GET | `/oauth/github/start` | Start GitHub OAuth flow. |
| GET | `/oauth/github/callback` | Exchange + `/user` + `/user/emails` + link/create + accept_login. |

### 6.4 Webhooks (mailer feedback)

| Method | Path | Purpose |
|---|---|---|
| POST | `/webhooks/postmark` | Bounce / complaint / unsubscribe — Postmark signature verified. |
| POST | `/webhooks/ses-sns` | SES via SNS — message signature verified. |

### 6.5 Health & internal

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz` | Liveness. |
| GET | `/readyz` | PG + hydra admin reachable. |

The gateway also routes `auth.zeroship.ai/healthz` to `crates/auth`'s `/healthz`; hydra's own `:4445/health/{alive,ready}` are on the loopback admin port and probed by our deploy.

---

## 7 · Tokens & keys (operated, not implemented)

Hydra owns the token lifecycle. Configuration:

| Token | Lifetime | Format | Where |
|---|---|---|---|
| Authorization code | 60 s (we set `ttl.auth_code: 60s`) | Opaque; single-use; PKCE-bound | `hydra_oauth2_code` table |
| ID token | 1 h (`ttl.id_token: 1h`) | JWT, EdDSA-signed | not stored; minted per request |
| Access token | 1 h (`ttl.access_token: 1h`) | JWT (per first-party client `access_token_strategy=jwt`), EdDSA-signed, RFC 9068 shape | not stored when JWT; introspectable |
| Refresh token | 30 days sliding (`ttl.refresh_token: 720h`) | Opaque (hydra always opaque — by design, for revocability) | `hydra_oauth2_refresh` |

**Refresh-token rotation** is enabled with a 30 s graceful overlap (`oauth2.grant.refresh_token.rotation_grace_period: 30s`, `rotation_grace_reuse_count: 3`) to defang concurrent-refresh races without sacrificing reuse detection.

**Keys** live in hydra's `hydra_jwk` table, encrypted by hydra's `SECRETS_SYSTEM` (which we manage as a Kubernetes/Nomad secret). Two key sets:

- `hydra.openid.id-token` — signs ID tokens. EdDSA primary + RS256 secondary.
- `hydra.jwt.access-token` — signs JWT access tokens. EdDSA only.

**Bootstrap.** On first boot, `crates/auth`'s bootstrap routine:

1. `hydra migrate sql up` runs in a side container (deployment-managed).
2. `crates/auth` waits for hydra admin port readiness.
3. `crates/auth` calls `POST /admin/keys/hydra.openid.id-token { alg: "EdDSA" }`, then `POST /admin/keys/hydra.openid.id-token { alg: "RS256" }`, then `POST /admin/keys/hydra.jwt.access-token { alg: "EdDSA" }`.
4. `crates/auth` reads the static clients config (TOML) and `POST /admin/clients` for each.

**Rotation cadence:** every 90 days, run `hydra create jwk` to **prepend** a new key (active for signing, old keys retained for verification). After ~31 days (>refresh-token max lifetime), retire the old key by deleting it. Operationalized as a daily cron in `crates/auth` itself — no external job runner.

---

## 8 · Identity flows

(This is our value-add. Unchanged from revision 1 in substance; minor adjustments to note that on success we call hydra's accept_login.)

### 8.1 Email + password

**Hashing:** Argon2id at OWASP 2026 params — `m = 19456`, `t = 2`, `p = 1`. PHC string stored in `password_hash`. Crate: `argon2` (RustCrypto) + `password-hash` traits. CPU-bound; wrapped in `compio::runtime::spawn_blocking`.

**Signup (`POST /signup`):**

1. Parse `{ email, password, name }`. Reject if `len(password) < 15` (NIST SP 800-63B-4 Rev 4).
2. Always-200 response: "If `<email>` is new, we've sent a verification link."
3. HIBP k-anonymity check (`api.pwnedpasswords.com/range/<5-char-prefix>`). If hit, refuse via email.
4. Argon2id hash + INSERT row. `email_verified_at = NULL`.
5. Issue verification token (24 h, opaque, hashed, single-use). Email it.

**Login (`POST /login?login_challenge=…`):**

1. `hydra_client::get_login(challenge)` — returns subject (if skip=true), client_id, requested_scope.
2. Look up user by email.
3. **Dummy-hash branch:** if no user, verify against fixed Argon2id hash of `b"absent-user-padding"` so wall time and code path are constant.
4. If `users.locked_until > now()` → identical "invalid credentials" response.
5. Verify password; on failure, increment rate-limit bucket, emit `login_failure`, return 401 with `{"error":"invalid_credentials"}`.
6. On success:
   - Create `auth.sessions` row.
   - Set `__Host-zsidp_session` cookie.
   - `hydra_client::accept_login(challenge, { subject: format_typed_id(user.id), remember: true, remember_for: 3600, acr: "urn:zeroship:pwd", amr: ["pwd"] })`.
   - 302 to the `redirect_to` returned by hydra.

**Rate limiting** (three buckets):

| Bucket | Limit | Action |
|---|---|---|
| `login:eip:<email>:<ip>` | 5 / 15 min | 429 with backoff |
| `login:email:<email>` | 10 / hr | 429 + send "suspicious activity" email |
| `login:ip:<ip>` | 60 / hr | 429 (captcha later) |

**Password policy** (NIST SP 800-63B-4 Rev 4):

- Min 15 chars when password is sole authenticator.
- Max ≥ 64 (we accept up to 1024).
- No composition rules, no rotation, no security questions.
- HIBP breach check on set/change.

### 8.2 Google OIDC federation

Hand-rolled `cyper` client (compio). Pattern:

1. `/oauth/google/start` reads pending `login_challenge` from query/cookie; generates `state` + PKCE verifier; sets short-lived cookies; redirects to Google's `/o/oauth2/v2/auth`.
2. `/oauth/google/callback`: validate state, exchange code at `oauth2.googleapis.com/token`, verify returned ID token against Google's JWKS (`accounts.google.com/.well-known/openid-configuration`), check `iss`/`aud`/`exp`.
3. Account-linking decision tree (find by `(provider='google', subject)`, then by email with verified-email gates), exactly as in revision 1.
4. On success: load user, write `auth.sessions`, then `hydra_client::accept_login(login_challenge, { subject: …, acr: "urn:zeroship:google", amr: ["oauth"], … })`.

### 8.3 GitHub OAuth federation

PKCE-with-secret (GitHub's client model). Fetch `/user` + `/user/emails`, pick `{primary, verified, not noreply}`. Same linking tree and `accept_login` finish.

### 8.4 Magic link

15-minute TTL, opaque 32-byte CSPRNG, SHA-256-stored, atomic single-use. CSRF-nonce binding for cross-device — same-device redeems immediately; cross-device shows a 6-digit code on the redemption page that the requesting device must enter to complete the sign-in. On redemption success, `hydra_client::accept_login(challenge, { subject: …, acr: "urn:zeroship:magic", amr: ["magic"], … })`.

### 8.5 Email verification (signup-time)

24 h TTL. Verification is purpose-stamped (`purpose='email_verification'`) and lives in `auth.email_verifications` — distinct from `auth.magic_links`. Unverified-user policy:

- **Allowed:** sign in, browse, read-only.
- **Blocked:** publishing apps, creating API keys, registering OIDC clients.
- **Hard wall:** after 7 days unverified, force verification at next login.

### 8.6 Password reset

`/forgot` issues a 1 h reset token (same primitive as magic-link, purpose='reset'). Redemption at `/reset` re-runs NIST + HIBP and updates `password_hash`.

---

## 9 · Sessions and cookies

### 9.1 The two sessions at `auth.zeroship.ai`

Two independent sessions both live on the `auth.zeroship.ai` host:

- **Our IdP login session** — `__Host-zsidp_session`. Tracks "this browser holds a verified zeroship user." Server-side state in `auth.sessions`. Owned by `crates/auth`.
- **Hydra's OIDC session** — `ory_hydra_session_*` (the suffix is hydra-managed). Tracks "hydra has recorded this subject as authenticated." Set by hydra at the auth.zeroship.ai host (`serve.cookies.domain=auth.zeroship.ai`). Owned by hydra.

They're coordinated through `accept_login`. When we accept, hydra writes its cookie; the next `/oauth2/auth` finds it and reports `skip=true`. The cookies have **different roles** — one is "user identified," the other is "subject confirmed for OIDC."

**Both cookies are HttpOnly, Secure, SameSite=Lax** so the OAuth redirect dance (which is a top-level navigation) carries them.

### 9.2 The RP session at `myapp.zeroship.ai` (and `console.zeroship.ai`)

Set by the gateway (or control plane) after a successful code exchange. Cookie: `__Host-zeroship_app_session`. Opaque 256-bit session id. Server-side state in the RP's session store (gateway maintains a small per-app session table; control plane uses its existing `dashboard.sessions` if present).

The RP session is what gates worker access. The ID token / access token from hydra are validated once (at code-exchange time), and from that point on the per-origin cookie is the load-bearing artifact.

### 9.3 CSRF cookies

`__Host-zsidp_csrf` (double-submit for our login/signup/consent/reset forms) and `__Host-zsidp_magic_csrf` (15 min, binds magic-link redemption to the requesting device) — exactly as revision 1.

### 9.4 Rotation

We rotate `__Host-zsidp_session` on every step-up (login success, MFA pass [future], explicit consent grant) to defeat session fixation. Hydra rotates its own cookie on subject change. Forcing logout in both places is done by the user's `/logout` handler (delete our session row + call `delete_login_sessions(subject)` against hydra admin).

---

## 10 · OIDC client registry and consent

### 10.1 Hydra owns the registry

Clients are stored in `hydra_client`. Manipulated via `POST /admin/clients` etc. We never read it directly — we go through `hydra_client::clients` if we need an admin view.

Each client carries:

```json
{
  "client_id": "console.zeroship.ai",
  "client_name": "zeroship Console",
  "grant_types": ["authorization_code", "refresh_token"],
  "response_types": ["code"],
  "redirect_uris": ["https://console.zeroship.ai/auth/callback"],
  "post_logout_redirect_uris": ["https://console.zeroship.ai/"],
  "scope": "openid offline_access email profile",
  "token_endpoint_auth_method": "client_secret_basic",
  "subject_type": "public",
  "access_token_strategy": "jwt",
  "id_token_signed_response_alg": "EdDSA",
  "audience": ["https://api.zeroship.ai"],
  "skip_consent": true,
  "require_consent": false,
  "require_logout_consent": false,
  "frontchannel_logout_uri": "https://console.zeroship.ai/logout/front",
  "backchannel_logout_uri": "https://api.zeroship.ai/oidc/backchannel"
}
```

`skip_consent` (and its sibling `require_consent`) signals our consent app to accept immediately without UI. Hydra exposes this flag through the consent challenge's `client.skip_consent` field — we honour it.

### 10.2 First-party clients at launch

| client_id | role |
|---|---|
| `console.zeroship.ai` | creator dashboard — first-party, skip_consent=true |
| `gateway` | per-deployed-app fan-out client; one `redirect_uri` per app at registration |
| `builder.zeroship.ai` | builder UI — first-party |

The `gateway` client's `redirect_uris` list grows by one entry every time a creator deploys an app (`https://<app>.zeroship.ai/__zeroship/auth/callback`). The control plane calls `PUT /admin/clients/gateway` to append the URI during deploy. **Exact-string match** per RFC 9700 §4.1 is preserved — there are no wildcards.

(Alternative: every deployed app gets its own client_id. Pros: fully scoped tokens, per-app secret rotation. Cons: client_id management at deploy/un-deploy. This is one of the §19 open questions.)

### 10.3 Consent UI

For first-party clients (`skip_consent=true`): `crates/auth/src/ui/consent.rs` reads the challenge, sees `client.skip_consent=true`, immediately calls `accept_consent` with all requested scopes. The browser sees a single 302 from `/consent` → `/oauth2/auth` → `redirect_uri`. No human interaction.

For third-party clients (`skip_consent=false`): render the consent UI per §10.4 of revision 1 (RP identity + readable scope translation + Allow/Deny buttons + "Remember this choice" checkbox).

Honour `prompt=none|login|consent` per OIDC Core §3.1.2.1.

---

## 11 · Migration from `crates/control`

In-place, single-PR, no back-compat.

1. **Retire the orphaned legacy `crates/auth/`.** A previous attempt at the auth service (bcrypt + the tokio-shaped `openidconnect` crate + Apple/Meta/Google/GitHub OAuth providers) lives on main but nothing depends on `zeroship-auth` from elsewhere in the workspace — it is dead code. Delete the legacy contents wholesale before laying down the new skeleton; git history preserves the old code if anything ever needs to be retrieved.
2. **New crate** `crates/auth/` per §4, replacing the legacy contents.
3. **Hydra deployment**: add `oryd/hydra:v25.4.0` (current Docker Hub tag) to compose / Nomad spec. `hydra migrate sql up` runs as an init step. Hydra config (§16) lives in `ops/hydra.yaml`.
4. **`auth.*` schema** added via `crates/auth/src/store/migrations.rs`. The existing `auth_users` / `auth_app_consents` / `auth_sessions` tables in the public schema are migrated by `INSERT INTO auth.users SELECT … FROM auth_users` once, then the public-schema tables are `DROP TABLE`d in the same transaction.
5. **Control plane** loses `auth_service.rs`, `auth_handlers.rs`, `oauth.rs`, and the `/auth/*` routes. It gains a small `crates/control/src/oidc_rp.rs` that runs an OIDC client against `auth.zeroship.ai`.
6. **Gateway** loses `crates/gateway/src/user_auth.rs`'s JWT-cookie path. It gains an OIDC RP module that:
   - On unauthenticated HTML requests, redirects to `auth.zeroship.ai/oauth2/auth`.
   - On `/__zeroship/auth/callback`, exchanges the code at `auth.zeroship.ai/oauth2/token`, validates the ID token against cached JWKS, sets `__Host-zeroship_app_session` at the app's origin.
   - The HMAC-signed `ZeroShip-User` payload to the worker is **unchanged**.
7. **Gateway** also gains a proxy rule for the `auth.zeroship.ai/oauth2/*` and `auth.zeroship.ai/.well-known/*` prefixes — forwards to hydra's public port.
8. **`sdks/auth`** is reduced: the browser fallback (`window.__zs_user`) is deleted. The package becomes a thin `env.auth.getUser() / requireUser() / signOut()` wrapper around the platform-injected identity. (`env.auth` becomes a newly-registered native namespace — currently "planned" in AGENTS.md.)
9. **Existing skew gets fixed:** the `email_verified` claim issue disappears because hydra now owns the JWT shape; the gateway parses real ID tokens, not shared-secret JWTs.

No `@deprecated` aliases. No `__zs_session` cookie shim. Pre-launch posture per AGENTS.md.

---

## 12 · Mailer abstraction

Unchanged from revision 1.

```rust
#[async_trait::async_trait]
pub trait Mailer: Send + Sync + std::fmt::Debug {
    async fn send(&self, msg: Email) -> Result<MessageId, MailerError>;
}
```

Three impls — `StdoutMailer`, `SmtpMailer` (lettre), `ResendMailer` (cyper). Templates via `askama`. Bounce/complaint webhooks at `/webhooks/postmark` and `/webhooks/ses-sns` populate `auth.email_suppressions`; the mailer short-circuits suppressed addresses.

Email templates: `verify-email`, `magic-link`, `password-reset`, `suspicious-activity`, `signin-alert` (last one v1.1 unless cheap).

---

## 13 · Threat model

Each row is annotated with the boundary: **[hydra]** = hydra implements the mitigation; **[ours]** = `crates/auth` implements it; **[gap]** = not mitigated at v1, accepted risk. Our `tests/threat_model.rs` parametric harness covers every **[ours]** row.

| Threat | RFC § | Mitigation | Boundary |
|---|---|---|---|
| Open redirect on `redirect_uri` | 9700 §4.1 | Exact byte-equal match against client.redirect_uris (no wildcards). | [hydra] |
| Authorization-code injection/interception | 9700 §4.5 | PKCE S256 mandatory; `oauth2.pkce.enforced_for_public_clients=true`; we also set it for confidential clients. | [hydra] |
| Authorization-code replay | 6819 §4.4.1.1 | Single-use atomic redemption; revoke issued tokens on second redemption. | [hydra] |
| CSRF on `state` | 9700 §4.7 | Hydra validates `state` round-trip. | [hydra] |
| Login CSRF (attacker logs victim in as attacker) | 9700 §4.13 | `__Host-zsidp_csrf` double-submit on every login/signup/consent/reset POST. SameSite=Lax IdP session. Rotate `__Host-zsidp_session` on login success. | [ours] |
| Mix-up attacks | RFC 9207 | Hydra does not advertise `iss` in the auth-response query. RPs validate `iss` **inside the ID token** instead (gateway and control RP modules enforce this). | [gap, ours-mitigated] |
| Token leakage via Referer | 6819 §4.4.2.5 | `Referrer-Policy: no-referrer` on all `crates/auth` UI; hydra sets its own. Code-only response_type — no tokens in URLs. | [ours + hydra] |
| Token leakage via browser history | 9700 §2.1.2 | Code-only response_type. | [hydra] |
| Session fixation | 6819 §4.4.1.11 | Rotate `__Host-zsidp_session` on every step-up. | [ours] |
| Account enumeration | OWASP ASVS V2 | Dummy-hash branch, identical status/body/timing on login. Identical signup response regardless of existing email. | [ours] |
| Brute force / credential stuffing | 9700 §4.16 | Three rate-limit buckets. Soft lock at threshold. HIBP on set. | [ours] |
| Phishing via OAuth consent | 9700 §4.18 | Client name + redirect host pinned at registration; consent UI shows both. | [ours] |
| Clickjacking on consent | 6819 §4.4.1.9 | `Content-Security-Policy: frame-ancestors 'none'` + `X-Frame-Options: DENY` on consent page. | [ours] |
| PKCE downgrade | 9700 §2.1.1 | Hydra rejects token requests missing `code_verifier` when challenge was present. We never advertise `plain`. | [hydra] |
| Refresh-token theft + replay | 9700 §4.14 | Rotation + grace-window reuse detection (`rotation_grace_reuse_count=3`); on detected reuse beyond the window, hydra revokes the family. | [hydra] |
| JWT alg-confusion (none, RS256↔HS256) | RFC 8725 §3.1 | Hydra never accepts `none`; signs with EdDSA/RS256; advertises `id_token_signing_alg_values_supported` correctly. Our RP verifiers pin `alg` per `kid`. | [hydra + ours-on-RP] |
| SSRF via `request_uri` / JAR | RFC 9101 §10.4.1 | Hydra disables `request_uri` and `request` parameters at v1 (we do not enable PAR). | [hydra-config] |
| `sub` claim misuse | OIDC Core §5.7 | `sub` is our opaque typed_id `usr_…`; documented as opaque. | [ours-policy] |
| `kid` injection / path traversal | RFC 8725 §3.6 | Hydra resolves `kid` via JWKS only. | [hydra] |
| DPoP unavailability for sender-constrained tokens | RFC 9449 | Hydra OSS does not implement. Accepted at v1 (short-TTL access tokens, opaque rotated refresh). Re-evaluate when third-party RPs ship. | [gap] |
| Hydra admin port accidentally exposed | hydra docs §production | Bind admin port `127.0.0.1` only; firewall rule denies external 4445. Documented in `docs/runbooks/auth-deploy.md`. | [ops] |
| `session.access_token` PII leakage via introspect | hydra flow doc | Keep `session.access_token` empty by policy; rely on `session.id_token` for claims. | [ours-policy] |
| Stale `login_challenge` resubmission | hydra issue #1280 | On `"WasHandled=true"` error from hydra, redirect user to the challenge's `request_url` so they restart the flow. | [ours] |

---

## 14 · Cookies, headers, CSP

`crates/auth` sets every response with:

```
Strict-Transport-Security:  max-age=63072000; includeSubDomains; preload
Content-Security-Policy:    default-src 'self'; script-src 'self' 'nonce-<random>';
                            style-src 'self' 'nonce-<random>'; img-src 'self' data:
                            https://*.zeroship.ai https://lh3.googleusercontent.com
                            https://avatars.githubusercontent.com;
                            connect-src 'self'; form-action 'self';
                            frame-ancestors 'none'; base-uri 'none'; object-src 'none';
                            upgrade-insecure-requests
X-Frame-Options:            DENY
X-Content-Type-Options:     nosniff
Referrer-Policy:            no-referrer
Permissions-Policy:         camera=(), microphone=(), geolocation=(), payment=(),
                            publickey-credentials-get=(self), interest-cohort=()
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Cache-Control:              no-store
```

Hydra sets its own headers on the `/oauth2/*` responses. Both share the same hostname so users see consistent transport security.

Cookies set at `auth.zeroship.ai`:

| Cookie | Owner | Path | SameSite | HttpOnly | Secure | Max-Age | Notes |
|---|---|---|---|---|---|---|---|
| `__Host-zsidp_session` | crates/auth | `/` | Lax | yes | yes | 12 h | Opaque session id; rotated on step-up. |
| `__Host-zsidp_csrf` | crates/auth | `/` | Strict | no | yes | 1 h | Double-submit; readable by inline `<script nonce>`. |
| `__Host-zsidp_magic_csrf` | crates/auth | `/` | Lax | yes | yes | 15 min | Binds magic-link redemption. |
| `ory_hydra_session_*` | hydra | `/` | Lax | yes | yes | configured | Hydra's subject session. |
| `oauth2_authentication_csrf` | hydra | `/` | Lax | yes | yes | session | Hydra CSRF cookie for OIDC flow. |
| `oauth2_consent_csrf` | hydra | `/` | Lax | yes | yes | session | Hydra consent-flow CSRF. |

Hydra's cookies are intentionally not `__Host-`-prefixed because hydra needs to share its session across subdomains in some deployments. We force them to `auth.zeroship.ai` exact-host scope by setting `serve.cookies.domain=auth.zeroship.ai`. They're SameSite=Lax + Secure + HttpOnly.

Per-app cookies (`__Host-zeroship_app_session`) are set at the app's origin by the gateway — those are documented in `crates/gateway/`'s design.

---

## 15 · Audit log

Unchanged from revision 1. Events emitted by `crates/auth` cover everything we own; hydra emits its own logs (configured to JSON via `log.format=json`) that we additionally ingest for correlation.

`crates/auth` event types:

- `signup`, `signup_blocked`
- `login_attempt`, `login_success`, `login_failure`
- `oauth_start`, `oauth_callback_success`, `oauth_callback_failure`, `oauth_link_success`
- `magic_issued`, `magic_redeemed_same_device`, `magic_redeemed_cross_device`
- `verification_issued`, `verification_redeemed`
- `password_reset_requested`, `password_changed`
- `hydra_accept_login`, `hydra_accept_consent`, `hydra_accept_logout`, `hydra_reject_*` *(low-volume; correlates each accept/reject we issued)*
- `hydra_delete_session` (sign-out-everywhere)
- `client_registered`, `client_updated` *(from bootstrap)*
- `key_rotation_announced`, `key_rotation_completed` *(driven by our cron)*
- `mailer_send`, `mailer_bounce`, `mailer_complaint`, `mailer_suppressed`

Always redacted: raw tokens (SHA-256 first 8 hex chars only), passwords, client secrets, magic-link tokens. Full email redacted to domain on `signup_blocked` (avoid log-side enumeration). PII retained 90 days; security events 365 days hot, 7 years cold; `refresh_reuse_detected` (sourced from hydra logs via correlation) forever.

---

## 16 · Operational concerns

### 16.1 Deployment shape

Two processes per "auth pod":

- `oryd/hydra:v26.2.x` — public port `:4444` (mapped to gateway's `auth.zeroship.ai/oauth2,/.well-known,/userinfo` routes), admin port `:4445` on loopback only.
- `crates/auth` binary — port `:9092` (mapped to gateway's `auth.zeroship.ai/*` for everything else).

Both share host networking; `crates/auth` reaches hydra via `http://127.0.0.1:4445`. Both share the same Postgres cluster.

### 16.2 Hydra configuration (`ops/hydra.yaml`)

```yaml
dsn: postgres://hydra@<host>:5432/zeroship?sslmode=verify-full

serve:
  public:  { port: 4444, host: 0.0.0.0 }
  admin:   { port: 4445, host: 127.0.0.1 }
  cookies: { same_site_mode: Lax, domain: auth.zeroship.ai }

urls:
  self:
    issuer: https://auth.zeroship.ai/
    public: https://auth.zeroship.ai/
  login:   https://auth.zeroship.ai/login
  consent: https://auth.zeroship.ai/consent
  logout:  https://auth.zeroship.ai/logout
  error:   https://auth.zeroship.ai/error

strategies:
  access_token: opaque   # default; per-client override sets JWT

oauth2:
  pkce:
    enforced_for_public_clients: true
    enforced: true                                   # all clients
  grant:
    refresh_token:
      rotation_grace_period: 30s
      rotation_grace_reuse_count: 3

ttl:
  access_token: 1h
  refresh_token: 720h
  id_token: 1h
  auth_code: 60s
  login_consent_request: 1h

secrets:
  system:  [${SECRETS_SYSTEM}]                       # rotate by prepending
  cookie:  [${SECRETS_COOKIE}]

oidc:
  subject_identifiers: { supported_types: [public] }
  dynamic_client_registration: { enabled: false }

webfinger:
  jwks: { broadcast_keys: [hydra.openid.id-token] }

log:     { level: info, format: json }
tracing: { provider: otel, providers: { otlp: { server_url: ${OTLP_URL} } } }
```

`SECRETS_SYSTEM` (≥16 chars) and `SECRETS_COOKIE` are loaded from Kubernetes/Nomad secrets at process start. They MUST be identical across all hydra replicas in the same deployment.

### 16.3 `crates/auth` configuration

```
--addr 0.0.0.0:9092
--db-url postgres://…
--hydra-admin http://127.0.0.1:4445
--hydra-public https://auth.zeroship.ai
--clients-config /etc/zeroship/auth-clients.toml
--mailer smtp | resend | stdout
--mailer-config /etc/zeroship/mailer.toml
--google-client-id …  --google-client-secret-file …
--github-client-id …  --github-client-secret-file …
--bootstrap                                       # allow first-boot client + JWK creation
--insecure-dev                                    # drop Secure flag on cookies; localhost only
```

The `auth-clients.toml` declaratively lists every OIDC client. At each boot, `crates/auth` reconciles the file against hydra's admin API — creating new clients, updating changed metadata, leaving extra clients alone.

### 16.4 First-boot sequence

1. `hydra migrate sql up` (deployment-managed init container / Job).
2. Hydra starts; admin port reachable on 127.0.0.1:4445.
3. `crates/auth` starts. Runs its own migrations (idempotent `CREATE TABLE` in `auth.*`).
4. With `--bootstrap`: `crates/auth` queries `GET /admin/keys/hydra.openid.id-token` — if empty, `POST /admin/keys/hydra.openid.id-token { alg: "EdDSA" }`, then `POST /admin/keys/hydra.openid.id-token { alg: "RS256" }`. Same for `hydra.jwt.access-token` (EdDSA only).
5. Reconcile clients from `auth-clients.toml`.
6. Start serving.

Without `--bootstrap`, an empty `hydra_jwk` set is a fatal startup error (prevents accidental key regeneration on a clean DB clone).

### 16.5 Key rotation

A daily compio task in `crates/auth` checks the age of the current signing key. When age > 90 days:

1. `POST /admin/keys/hydra.openid.id-token { alg: "EdDSA" }` — **prepends** a new key (hydra's list-based store activates the head element for signing).
2. Emit `key_rotation_announced` audit event.
3. After 31 days (> refresh-token max lifetime), `DELETE /admin/keys/hydra.openid.id-token/<old-kid>`.
4. Emit `key_rotation_completed`.

Same cycle for `hydra.jwt.access-token`.

### 16.6 Multi-instance

Hydra replicas: yes — N replicas against one Postgres. All replicas share `SECRETS_SYSTEM` and `SECRETS_COOKIE`. First-boot caveat: launch with `replicas=1` until JWK creation completes, then scale.

`crates/auth` replicas: yes — stateless. Sessions in PG, rate-limit buckets in PG. The bootstrap routine is idempotent (key set already populated → skip; client registry reconciled).

### 16.7 Backups

Postgres backup covers both `hydra_*` and `auth.*` schemas in the same snapshot. To restore a working auth service, you also need `SECRETS_SYSTEM` (off-box, in the secret manager). Without it, hydra's at-rest-encrypted `hydra_jwk` rows are useless and a recovery requires re-generating keys (which invalidates outstanding tokens).

### 16.8 Disaster: hydra unavailable

If hydra is down, **every login is down** — the platform is dark. Mitigation:

- Run hydra HA (≥2 replicas).
- Monitor `/health/ready` on admin port from outside the pod.
- Database is the dominant failure mode; ensure hot-standby PG.
- For graceful degradation, our gateway's per-app `__Host-zeroship_app_session` cookies remain valid (gateway verifies JWT against cached JWKS, doesn't hit hydra per request) — so already-signed-in users keep working until their access tokens expire (1 h).

`crates/auth` returns 503 with a polite "auth service is temporarily unavailable" page if hydra admin is unreachable for >5 s.

---

## 17 · Testing

### 17.1 Unit tests

- `identity/password.rs` — Argon2id roundtrip; dummy-hash equal-time.
- `identity/magic_link.rs` — atomic single-use; CSRF binding; cross-device flow.
- `hydra_client/*.rs` — typed wire-format round-trips against hydra's published OpenAPI fixtures.
- `store/*.rs` — atomic upserts; ratelimit-bucket math.

### 17.2 Integration tests (PG + hydra + crates/auth)

The `compose-test` harness (modeled on `compio-postgres/tests/`) boots Postgres, hydra (real binary), and `crates/auth` for each suite. Tests drive HTTP against `crates/auth` and hydra public.

- `tests/e2e_password.rs` — register → /oauth2/auth → /login → /consent → /token → /userinfo. Asserts ID-token signature, claims, refresh round-trip.
- `tests/e2e_google.rs` — mock Google's `/token` and `/userinfo` via cyper; same end-to-end.
- `tests/e2e_github.rs` — mock GitHub; verify primary-verified-email picker.
- `tests/e2e_magic.rs` — issue + redeem same-device + cross-device 6-digit code.
- `tests/e2e_logout.rs` — RP-Initiated Logout; assert hydra session cleared + our session row revoked.
- `tests/e2e_sso.rs` — second app's `/oauth2/auth` finds `skip=true` and silently completes.
- `tests/enum_defense.rs` — N login attempts, half on nonexistent users; assert response body, status, and timing distributions equal within tolerance.
- `tests/hydra_client_smoke.rs` — round-trip every admin endpoint we use (login, consent, logout, clients, sessions, jwks).

### 17.3 Threat-model parametric tests

`tests/threat_model.rs` covers every **[ours]** and **[ours-policy]** row in §13. Hydra-implemented rows are covered by hydra's own conformance suite (we trust the OpenID certification).

### 17.4 Faithful E2E (no shims)

Per the project's "faithful e2e" rule (memory: `feedback_faithful_e2e_tests.md`): every test boots a real hydra binary, a real `crates/auth` instance, and a real Postgres. No mocked admin API. No in-memory hydra. Mocked surfaces are limited to upstream OAuth providers (Google, GitHub) and the mailer (where `StdoutMailer` is captured into an `InMemoryMailer` for assertions).

### 17.5 Spec conformance

Not run by zeroship — hydra holds the OpenID certification. If we ever fork hydra or replace it, the OpenID Foundation conformance suite becomes a CI gate.

---

## 18 · Out of scope (deferred, explicit)

- **Dynamic Client Registration** — disabled in hydra config; revisit when third-party self-serve ships.
- **Device Authorization Grant (RFC 8628)** — hydra supports it (since v25.4); we don't enable.
- **CIBA**, **OIDC Federation**, **FAPI 1/2** — out of scope.
- **PAR / JAR / JARM** — disabled in hydra config.
- **mTLS-bound tokens (RFC 8705)** — out of scope.
- **DPoP (RFC 9449)** — hydra OSS gap; accepted at v1.
- **RFC 9207 `iss` in auth-response URL** — hydra gap; we mitigate at the RP layer (validate `iss` in ID token).
- **Passkeys / WebAuthn** — `Permissions-Policy: publickey-credentials-get=(self)` reserves the surface; ceremony is v1.1.
- **MFA (TOTP / SMS / hardware key)** — v1.1. The `acr`/`amr` propagation through `accept_login` is forward-compatible.
- **Back-channel logout consumption** — hydra emits; our gateway/control RP layers will consume in v1.1.
- **Redis-backed rate-limit / session state** — PG suffices at v1.
- **OIDC Federation, account portability** — out of scope.
- **Tenancy beyond `public` `sub`** — pairwise subject identifiers if a future RP requires.

---

## 19 · Open questions for review

1. **Hydra version pin.** v26.2.x (latest, monthly point releases) or v25.4.x (the last quarter-aligned LTS-shaped release)? v26.2 is current; v25.4 added device flow + OAuth 2.1 discovery and has more bake time. I'd pin v26.2.x but flag.
2. **Client model for hosted creator apps.** Single shared `gateway` client with per-deploy `redirect_uris` appended, OR a separate `client_id` per deployed app? The latter scales linearly with deploys (control plane manages clients via admin API) but lets us scope tokens per-app and rotate per-app secrets. Recommend per-app `client_id` once we have ≥ 100 apps; shared `gateway` for v1.
3. **Resource indicators (RFC 8707).** Hydra accepts `audience` on clients today. Should each app deploy include a unique `aud` (e.g. `https://<app>.zeroship.ai`)? Or stick with `https://api.zeroship.ai` for everything?
4. **DPoP timeline.** If we plan to ship third-party "Sign in with Zeroship" in v1.1, hydra's DPoP gap becomes pressing. Options: (a) wait for ory to ship DPoP, (b) fork+patch hydra, (c) implement DPoP at the gateway/RP layer (token-binding from outside hydra). (c) is feasible but invasive.
5. **Hydra admin port surface.** Default exposure is 127.0.0.1 in our config. Do we want admin API also reachable from the gateway (for a future admin UI)? If yes, we add a path-prefix on the gateway with allowlisted IP + service-to-service auth.
6. **Backup / restore drill.** Multi-instance hydra needs `SECRETS_SYSTEM` in the secret manager. Where does it live in dev? `--master-key-file` for local? Document the recovery runbook before the first PR lands.
7. **JWK rotation cadence.** 90 days is industry-standard. Should we tie rotation to refresh-token TTL (so we can drop old keys after 30 days, not 31)? Or be more conservative (120 days)?

---

## 20 · Source briefings

This proposal draws on five research briefings produced 2026-05-26 (full transcripts retained in agent task logs):

- **OIDC/OAuth spec essentials** — RFC 9700, OIDC Core 1.0, OAuth 2.1 draft, PKCE-everywhere posture, discovery doc, common pitfalls.
- **Cryptographic core** — Ed25519 vs RS256, JWKS rotation, RFC 9068 JWT access tokens, refresh-token rotation with reuse detection, 60-second authorization code TTL.
- **Identity flows + mailer** — Argon2id at OWASP 2026 params, dummy-hash enumeration defense, NIST SP 800-63B-4 Rev 4 password policy, GitHub vs Google federation differences, magic-link cross-device CSRF binding, `lettre`/`askama`/`resend-rs` choices.
- **Reference IdPs + threat model** — patterns from `dexidp/dex`, `supabase/auth`, `authelia`; patterns avoided from `keycloak` and `zitadel`; RFC 6819 + RFC 9700 + RFC 8725 threat matrix.
- **ory/hydra integration deep-dive** — login/consent/logout challenge contract, admin API surface, client/key management, hydra v26.2 config, deployment topology, operational pitfalls (PSL, cookie scoping, stale challenges), hand-rolled Rust admin client, gaps (DPoP, RFC 9207).

Every concrete number in this document traces to one of those briefings or to a hydra config field cited in the integration brief.
