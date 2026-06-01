# zeroship authorization — proposal

**Status:** proposal, in review (revision 1)
**Date:** 2026-05-27
**Branch:** `proposal/auth-server`
**Worktree:** `.claude/worktrees/auth-server`
**Companion to:** `docs/proposals/auth-server.md` (the IdP). This document layers authorization on top of the identities that auth-server proves out.

This file lives in a fresh worktree off main and is committed in the same PR that lands the implementing code (per the project's proposal workflow). It encodes locked design decisions — engineers implement from this; reviewers verify against it.

---

## 0 · One-line product framing

> **Authorization in zeroship is one Cedar engine, three audiences, four grant paths, and a wrapper SDK that hides Cedar from anyone who doesn't want to see it.**

The platform answers exactly one question per request: *"Is this principal allowed to perform this action on this resource right now?"* — for platform staff, for creators, and (in P12) for end users inside creator apps. Cedar is the single decision engine. A closed-vocabulary wrapper SDK (`crates/authz/`) hides Cedar's syntax behind a toggle-matrix UI for the 95% case; a Cedar power-user tab is available for the long tail. OAuth 2.0 (hydra) delivers the credentials; Cedar decides what to do with them.

---

## 1 · Goals & non-goals

### 1.1 Goals

- **One engine.** Cedar evaluates every authz decision in the platform — platform RBAC, creator-app management, and end-user authz inside creator apps. One mental model, one audit format, one set of correctness tests.
- **Hide Cedar by default.** Creators get a toggle-matrix UI backed by a closed enum vocabulary. Cedar source is opt-in via a power-user tab. End-user authz in creator apps is declarative (`policies.cedar` in the bundle).
- **TOKEN ⊂ USER invariant.** A personal-access token can never exceed its owner's permissions. Revoking the owner's app membership implicitly revokes every token derived from it. Two-call evaluation (owner-without-token, owner-with-token) makes this impossible to defeat by drift.
- **Four grant paths, one rule.** Every grant — platform role, app member, PAT self-grant, third-party OAuth — is bounded by the grantor's own permissions. `assert_grantor_authorized()` is the only function that turns a request into a row.
- **Zero tokio.** Cedar evaluation runs synchronously on the compio thread. v8class hosts the engine in the worker. No async authz — decisions are µs-class.
- **Deterministic decisions.** Every authorize call logs `{token_id, action, resource, decision, matched_policy_ids}`. Replay against an old policy set is trivial.

### 1.2 Non-goals

- **Not a full IAM product.** No attribute-based access control beyond the closed condition library. No SCIM. No directory federation (LDAP/AD).
- **Not a policy DSL of our own.** Cedar is the DSL; we wrap it. No template-to-rego translation, no homegrown grammar.
- **Not a hot-reload server.** Policies load on app boot and re-load on explicit update; no per-request DB read for the rules themselves (only for membership/role lookups, which are needed anyway for the principal entity).
- **Not a runtime sandbox.** Per-end-user authz logic in creator apps runs inside the same V8 isolate as the app — Cedar is data, not code, so no eval-the-untrusted needed.
- **Not back-compat-friendly.** Pre-launch posture (AGENTS.md): rename the table, delete the old column, ship in one PR.

---

## 2 · The three audiences

Cedar makes one call shape — `is_authorized(principal, action, resource, context, policies, entities)` — and we use it for three distinct populations. Each gets a concrete request example.

### 2.1 Platform RBAC (zeroship staff)

> "Can the support engineer view this creator's billing dashboard right now?"

```http
GET /admin/creators/cr_01H.../billing
Cookie: __Host-zs_console_session=…
```

Authorization:

```
principal  = User::"usr_01H...staff..."
action     = Action::"billing:read"
resource   = Resource::Org::"org_zeroship_platform"
context    = { ip: "10.0.5.42", mfa_age_seconds: 312 }
policies   = static Cedar in policies/platform/*.cedar
entities   = { User w/ role "support" via platform.roles }
```

Hardcoded 4-role set: `admin`, `support`, `billing`, `readonly`. Stored in `platform.roles`. Policies in `policies/platform/*.cedar` shipped in the repo; an operator override mechanism (§9.2) lets a deployed instance amend them without a rebuild.

### 2.2 Creator authz (app owners, collaborators, machine clients)

> "Can Alice's CI bot, holding PAT `pat_01H...`, deploy a new bundle to `app_01H...`?"

```http
POST /apps/app_01H.../deploy
Authorization: Bearer pat_01H...
```

Authorization (two-call, see §13.2):

```
Call 1: principal = User::"usr_01H...alice..." (the PAT owner)
        action    = Action::"apps:deploy"
        resource  = Resource::App::"app_01H..."
        policies  = static creator base + control.app_members row
        → must allow

Call 2: principal = Token::"pat_01H..."
        action    = Action::"apps:deploy"
        resource  = Resource::App::"app_01H..."
        policies  = control.permission_tokens.policies (this token)
        → must allow

Both must allow → 200. Either denies → 403.
```

App membership lives in `control.app_members` (per-app, per-user role); the PAT's own policy is JSONB-packed wrapper output in `control.permission_tokens.policies`.

### 2.3 End-user authz inside creator apps (P12)

> "Can end-user Bob (logged into the creator's app `todoapp.example.com`) delete this Todo row?"

```javascript
// Inside the creator's RPC handler:
import { permissions } from "@zeroship/permissions";

await permissions.require({
  action: "todos:delete",
  resource: { type: "Todo", id: todo.id, owner: todo.owner_id }
});
```

Authorization (in the worker, via v8class):

```
principal  = User::"usr_01H...bob..."    (creator-app subject)
action     = Action::"todos:delete"
resource   = Resource::Todo::"todo_01H..." with attrs {owner: ...}
policies   = creator's policies.cedar bundled in the deploy
entities   = creator-declared groups (members, admins, …)
```

Creators write Cedar directly here — it's the right level for app-internal logic ("owner can delete, members can read"). No wrapper UI; this is power-user territory.

---

## 3 · System architecture

```
                            ┌─────────────────────────────────────────────┐
                            │           CREDENTIAL ENTRY POINTS            │
                            └─────────────────────────────────────────────┘
                                              │
       ┌──────────────────────┬───────────────┴────────────────┬──────────────────────┐
       │                      │                                │                      │
       ▼                      ▼                                ▼                      ▼
 ┌───────────┐         ┌─────────────┐                ┌────────────────┐      ┌────────────────┐
 │  Session  │         │  PAT (EdDSA │                │ Hydra access   │      │  Device-grant  │
 │  cookie   │         │   JWT, our  │                │ token (OAuth   │      │  access token  │
 │  (hydra → │         │  signing    │                │  Authcode+PKCE │      │  (RFC 8628 via │
 │  RP set)  │         │  key, NOT   │                │  via hydra)    │      │  hydra)        │
 │           │         │   hydra)    │                │                │      │                │
 └─────┬─────┘         └──────┬──────┘                └────────┬───────┘      └────────┬───────┘
       │                      │                                │                       │
       └──────────────────────┴────────────────┬───────────────┴───────────────────────┘
                                               │
                                               ▼
                                ┌──────────────────────────────┐
                                │   ntex extractor AuthzGuard   │
                                │   - parse credential          │
                                │   - resolve principal         │
                                │   - resolve resource from URL │
                                │   - assemble Cedar entities   │
                                └──────────────┬───────────────┘
                                               │
                                               ▼
                                ┌──────────────────────────────┐
                                │       crates/authz wrapper    │
                                │   - closed Action / Resource  │
                                │   - lower Statement → Cedar   │
                                │   - cache PolicySet by hash   │
                                └──────────────┬───────────────┘
                                               │
                                               ▼
                                ┌──────────────────────────────┐
                                │      cedar_policy::Engine     │
                                │  is_authorized(p,a,r,ctx,…)   │
                                └──────────────┬───────────────┘
                                               │
                  ┌────────────────────────────┼────────────────────────────┐
                  │                            │                            │
                  ▼                            ▼                            ▼
        ALLOW + audit event             DENY + audit event       Indeterminate → 500
```

Three deployment surfaces share this pipeline:

1. **Control plane** (`crates/control/`) — every handler that touches an app, env, deploy, billing, or member runs through `AuthzGuard`. Cedar engine is in-process.
2. **Gateway** (`crates/gateway/`) — relevant only for platform-internal admin routes the gateway exposes (today: backchannel-logout receiver). Most user-facing gateway behavior is access enforcement (does this session exist?), which is auth, not authz; authz only kicks in on `/__zeroship/admin/*` if we add such routes.
3. **Worker** (`crates/worker/` + `crates/plugin-authz/`, new) — Cedar engine runs inside the V8 isolate via v8class for end-user authz inside creator apps. P12 only.

The same Rust `crates/authz/` crate compiles into all three. v8class bindings live in `crates/plugin-authz/`.

---

## 4 · Action vocabulary

Closed enum. Adding an action is a Rust PR + UI translation update + Cedar template update; we accept that friction in exchange for "every action is enumerable, audit-decodable, and lintable".

```rust
// crates/authz/src/action.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Action {
    // Apps
    AppsRead,           // "apps:read"
    AppsWrite,          // "apps:write"
    AppsDeploy,         // "apps:deploy"
    AppsDelete,         // "apps:delete"

    // Environment
    EnvRead,            // "env:read"
    EnvWrite,           // "env:write"

    // Secrets (subset of env, marked sensitive)
    SecretsRead,        // "secrets:read"
    SecretsWrite,       // "secrets:write"

    // Billing
    BillingRead,        // "billing:read"
    BillingWrite,       // "billing:write"

    // Team / members
    TeamRead,           // "team:read"
    TeamWrite,          // "team:write"

    // Account-level (user's own profile, tokens, MFA settings)
    AccountRead,        // "account:read"
    AccountWrite,       // "account:write"

    // Deployments
    DeploymentsRead,    // "deployments:read"
    DeploymentsRollback,// "deployments:rollback"
}
```

Round-trip: the variant name → Cedar's quoted action string. The wrapper's `Display` and `FromStr` are the canonical conversion; we never embed string literals elsewhere in the codebase. Validation: `try_from(&str)` is the only ingress for raw strings (UI form posts, PAT-policy JSON parse), and it's `#[serde(deny_unknown_fields)]` strict.

**Why a closed enum (vs free-form strings).** Free-form strings would let creators express any action — including ones the platform doesn't enforce — which silently no-ops. A closed enum makes every action grep-able, every UI toggle backed by code, every Cedar policy compile-checked against the same vocabulary. The cost (a PR per new action) is the right friction at v1: it forces the question "is this a real action or a UI affordance?"

**End-user authz (P12) exception.** Inside creator apps, the action vocabulary is creator-defined (strings in their `policies.cedar`). That's fine — there is no platform-side UI for end-user actions, no central audit, no enforcement outside the worker. The cost (no central enumerability) is what creators are paying for the flexibility.

---

## 5 · Resource model

Resources are typed entities. Cedar likes typed entities; we lean into it.

```rust
// crates/authz/src/resource.rs
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum Resource {
    App { id: AppId },        // typed_id app_…
    Org { id: OrgId },        // typed_id org_…
    Any,                       // for cross-cutting actions (e.g., AccountRead w/o a target)
}
```

Cedar entity uids:

| Wrapper variant | Cedar uid |
|---|---|
| `Resource::App { id }` | `App::"app_01H..."` |
| `Resource::Org { id }` | `Org::"org_01H..."` |
| `Resource::Any` | `Any::"singleton"` |

### 5.1 Hierarchies (P11)

```
Org::"org_acme"
   parents: []
App::"app_acme_blog"
   parents: [Org::"org_acme"]
```

Cedar's `principal in Org::"org_acme"` walks the parent chain natively. P11 introduces the `Org` entity and populates `App.parents` when an app belongs to an org. P9 ships with `Org::"singleton"` for every user's personal scope; the migration to populated orgs is a Cedar entity change only — no policy rewrites.

### 5.2 Entity assembly

`AuthzGuard` (§13) assembles three entity classes per request:

- **Principal entity** — `User::"usr_..."` with attributes `{ platform_role, mfa_age_seconds, email_verified, account_locked }`. From `auth.users` + `platform.roles`.
- **Resource entity** — `App::"app_..."` with attributes `{ owner, plan, locked }`. From `control.apps`.
- **Membership edges** — `App::"app_..." in Org::"org_..."` (P11) and `User::"usr_..." in AppMembers::"app_..._members"` (a synthetic Cedar group per app). From `control.app_members`.

All three are constructed in one transaction at request start, cached per request, never persisted.

---

## 6 · Condition library

Conditions are the second closed enum. They compile to Cedar `when {}` blocks; the wrapper UI exposes them as discrete toggles + parameter fields.

```rust
// crates/authz/src/condition.rs
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    /// Only honored if request IP is in one of the CIDRs.
    IpRange { cidrs: Vec<IpCidr> },

    /// Only honored if local time at TZ is between start and end.
    /// start/end are minutes since midnight (0..=1440).
    TimeWindow { start_min: u16, end_min: u16, tz: Tz },

    /// Only honored if the request bears an MFA assertion at all.
    RequireMfa,

    /// Only honored if the most recent MFA assertion is within N seconds.
    MfaWithin { seconds: u32 },
}
```

Compilation to Cedar (`crates/authz/src/lower.rs`):

| Wrapper | Cedar `when` clause |
|---|---|
| `IpRange { cidrs: ["10.0.0.0/8"] }` | `context.ip in [ip("10.0.0.0/8")]` |
| `TimeWindow { 540, 1080, "America/Los_Angeles" }` | `context.local_minute >= 540 && context.local_minute < 1080` (caller computes `local_minute` from request `now` + tz) |
| `RequireMfa` | `context.mfa_age_seconds >= 0` (sentinel: `-1` ⇒ absent) |
| `MfaWithin { 900 }` | `context.mfa_age_seconds >= 0 && context.mfa_age_seconds <= 900` |

Caller fills `context` at extractor time from the credential's claims + request headers + clock. The compiled Cedar text is a deterministic function of the wrapper input; we hash it for the policy-set cache key.

**Why a closed condition set.** Cedar conditions can reference any context attribute. If we let users author arbitrary Cedar, we'd have to validate context shape forever. By fixing the condition set we fix the context shape: `{ ip, local_minute, mfa_age_seconds }` is all the extractor builds. New conditions = new wrapper variant + new context fields.

---

## 7 · Policy authoring paths

Four ways a policy enters the system. All four normalize to the same `Statement` representation, then to Cedar source, then to a `cedar_policy::PolicySet`.

### 7.1 Toggle-matrix UI (the 95% path)

The dashboard renders a table:

```
                  Read   Write   Deploy   Delete    Conditions
  apps             [x]    [x]     [x]      [ ]      (none)
  env              [x]    [ ]     —        —        (none)
  secrets          [ ]    [ ]     —        —        IP allow-list: 10.0.0.0/8
  billing          [x]    [ ]     —        —        (none)
  team             [x]    [x]     —        —        (none)
  deployments      [x]    —       —        —        Time window: 09:00-18:00 PT
```

Each checked cell + condition row becomes one `Statement { effect: Allow, actions, resources, conditions }`. The UI POSTs a JSON `Policy` blob to the control plane; the wrapper compiles it; the resulting Cedar source is stored alongside.

### 7.2 Cedar power-user tab

Toggle-matrix is a lossy view: it can express Allow-with-conditions, but not Deny-overrides, group-of-groups, or arbitrary entity references. For the long tail, the dashboard offers a Cedar tab — raw editor with Cedar-WASM lint (§13.4 P10).

A policy authored in the Cedar tab is stored as Cedar source only; the wrapper round-trips refuse to decompile it. The matrix tab is then read-only with a banner "this token uses a custom Cedar policy".

This is the same model as raw HTML in a rich-text editor: the visual view is a convenience, not a constraint. The audit log stores Cedar source either way.

### 7.3 CLI

`zeroship policy edit pat_01H...` opens `$EDITOR` with the current Cedar source. On save, control-plane round-trips through `cedar_policy::PolicySet::from_str` for validation. No matrix view in CLI.

### 7.4 Declarative bundle (creator apps, P12)

```
my-app/
├── policies.cedar     // creator-authored, Cedar source
└── src/
    └── index.ts
```

The build pipeline (`sdks/vite-plugin/`) reads `policies.cedar`, validates it via Cedar-WASM in node, and bakes it into the `.zship` manifest as `manifest.authz.policies = "..."`. At app boot, the worker compiles it once into a `cedar_policy::PolicySet` and pins it to the isolate. No DB round-trip per call.

End users get authz decisions free; creators get a declarative file that ships with their code.

---

## 8 · The wrapper SDK (`crates/authz/`)

```
crates/authz/
├── Cargo.toml
├── src/
│   ├── lib.rs          // re-exports
│   ├── action.rs       // Action enum + Display + FromStr
│   ├── resource.rs     // Resource enum + Cedar uid conversion
│   ├── condition.rs    // Condition enum
│   ├── effect.rs       // Effect = Allow | Deny
│   ├── policy.rs       // Policy { name, statements }
│   ├── statement.rs    // Statement { effect, actions, resources, conditions }
│   ├── lower.rs        // Wrapper → Cedar source string
│   ├── engine.rs       // PolicySet builder + cache by policy_hash
│   ├── eval.rs         // is_authorized() façade; two-call helper
│   ├── audit.rs        // AuthzDecision + emission helpers
│   ├── entities.rs     // EntityBuilder for principal/resource/group
│   ├── error.rs        // AuthzError; impls IntoResponse for ntex
│   └── tests/
│       ├── lower_snapshot.rs   // golden Cedar output per wrapper input
│       ├── two_call_subset.rs  // TOKEN ⊂ USER property tests
│       └── condition_eval.rs   // each Condition variant against canned contexts
```

`crates/plugin-authz/` (P12) provides the v8class binding into the Rust crate; it doesn't fork Cedar.

The wrapper's surface contract (`crates/authz/src/lib.rs`):

```rust
pub fn is_authorized(
    principal: &Principal,
    action: Action,
    resource: &Resource,
    context: &Context,
    bundle: &PolicyBundle,
    entities: &Entities,
) -> AuthzDecision;

pub fn is_authorized_with_token(
    owner: &Principal,
    token: &Token,
    action: Action,
    resource: &Resource,
    context: &Context,
    base_bundle: &PolicyBundle,    // static platform + creator-base policies
    token_bundle: &PolicyBundle,   // token's own policies
    entities: &Entities,
) -> AuthzDecision;
```

The second function is the canonical TOKEN ⊂ USER enforcement point. It calls Cedar twice; we never re-implement Cedar's evaluation logic.

---

## 9 · Storage model

### 9.1 New control-plane tables

```sql
-- 9.1.1 App membership. Adding a creator to an app's team or letting a
-- machine client touch an app both go through this table.
CREATE TABLE control.app_members (
    app_id        UUID NOT NULL REFERENCES control.apps(id) ON DELETE CASCADE,
    user_id       UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    role          TEXT NOT NULL,                  -- 'owner' | 'editor' | 'viewer'
    added_by      UUID NOT NULL REFERENCES auth.users(id),
    added_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at    TIMESTAMPTZ,
    PRIMARY KEY (app_id, user_id)
);
CREATE INDEX control_app_members_user_idx ON control.app_members (user_id) WHERE revoked_at IS NULL;

-- 9.1.2 Personal access tokens. The "token" half of TOKEN ⊂ USER. The
-- token is a gateway-issued EdDSA JWT; this table holds the metadata
-- and the policy bundle.
CREATE TABLE control.permission_tokens (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id      UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,                  -- human label, e.g., "CI deploy bot"
    policies      JSONB NOT NULL,                 -- the wrapper Policy { name, statements } shape
    policy_hash   BYTEA NOT NULL,                 -- SHA-256 of canonical-JSON(policies)
    cedar_source  TEXT NOT NULL,                  -- lowered Cedar; redundant w/ policies but cheap to keep
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at    TIMESTAMPTZ NOT NULL,           -- ≤ 1 year per §10
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);
CREATE INDEX control_permission_tokens_owner_idx ON control.permission_tokens (owner_id) WHERE revoked_at IS NULL;
CREATE INDEX control_permission_tokens_hash_idx  ON control.permission_tokens (policy_hash);

-- 9.1.3 Operator overrides to platform policies. Empty by default; a
-- deployer can add a row to extend or restrict the static policy set
-- without rebuilding the binary.
CREATE TABLE control.platform_policies (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL UNIQUE,
    cedar_source  TEXT NOT NULL,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

### 9.2 New platform-schema table

```sql
-- 9.2.1 Platform RBAC role assignments. The 4-role enum is hardcoded in
-- crates/authz/src/platform_role.rs; this table just records who has which.
CREATE TABLE platform.roles (
    user_id       UUID PRIMARY KEY REFERENCES auth.users(id) ON DELETE CASCADE,
    role          TEXT NOT NULL,                  -- 'admin' | 'support' | 'billing' | 'readonly'
    granted_by    UUID NOT NULL REFERENCES auth.users(id),
    granted_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

A user with no row is `'none'` — no platform privileges. App and account permissions still apply.

### 9.3 Audit table (extends auth)

```sql
CREATE TABLE control.authz_decisions (
    id              BIGSERIAL PRIMARY KEY,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    request_id      TEXT,
    principal_id    TEXT NOT NULL,         -- usr_… or pat_…
    token_id        UUID REFERENCES control.permission_tokens(id),
    action          TEXT NOT NULL,
    resource_type   TEXT NOT NULL,         -- 'App' | 'Org' | 'Any'
    resource_id     TEXT NOT NULL,
    decision        TEXT NOT NULL,         -- 'allow' | 'deny'
    matched_policies TEXT[] NOT NULL,      -- Cedar policy IDs
    context         JSONB                  -- ip, mfa_age_seconds, etc.
);
CREATE INDEX control_authz_decisions_principal_idx ON control.authz_decisions (principal_id, occurred_at);
CREATE INDEX control_authz_decisions_resource_idx  ON control.authz_decisions (resource_type, resource_id, occurred_at);
```

Retention: 365 days hot, 7 years cold (same as `auth.audit_events` security rows).

### 9.4 Cedar source files in repo

```
policies/
├── platform/
│   ├── admin.cedar          // full access for platform.roles = 'admin'
│   ├── support.cedar        // read-only across creators + billing:read
│   ├── billing.cedar        // billing:read + billing:write
│   └── readonly.cedar       // *:read everywhere (for auditors)
├── creator/
│   ├── app_owner.cedar      // owner role full access on owned App
│   ├── app_editor.cedar     // apps:read|write|deploy + env:read|write
│   └── app_viewer.cedar     // *:read on App
└── enduser/
    └── (creator-supplied in P12; not in repo)
```

These compile at build time via a `crates/authz/build.rs` that runs `cedar_policy::PolicySet::from_str` and `include_str!`s the validated bodies. Build fails if Cedar is invalid — this is the lint that catches typos in the canonical policies.

### 9.5 Engine cache (in-memory only)

The runtime never persists a `cedar_policy::PolicySet`. On boot:

1. Load `policies/platform/*.cedar` + `policies/creator/*.cedar` from `include_str!`.
2. Concatenate; parse once into a `PolicySet`. Store as `static_bundle`.
3. Load `control.platform_policies` rows where `enabled=true`; merge into `static_bundle`.
4. Compute `static_hash = SHA-256(canonical-cedar-source)`.

Per-request: lookup the token's `policy_hash` in a small LRU (default 1024 entries). Miss → compile its `cedar_source` and insert. The hash is stable per token; compilation cost amortizes across reuse.

---

## 10 · Token model

Three credential types reach `AuthzGuard`. Each has a distinct lifecycle.

### 10.1 PAT (Personal Access Token)

- **Mint:** user calls `POST /me/tokens` (control plane). Carries `{ name, policies, expires_at }`. Server validates TOKEN ⊂ USER at mint (every action+resource pair in the policy must currently be allowed for the user); inserts row; returns the raw token once.
- **Format:** EdDSA-signed JWT. Header `{alg:"EdDSA", typ:"pat+jwt", kid:<gateway signing key thumbprint>}`. Claims:

  ```json
  {
    "iss": "https://api.zeroship.ai",
    "sub": "usr_01H...",           // the owner
    "tid": "pat_01H...",           // the permission_tokens.id
    "iat": 1748352000,
    "exp": 1779888000,              // ≤ 365 days
    "scope": "pat",                 // marker — distinguishes from access tokens
    "policy_hash": "sha256:abc..."  // matches control.permission_tokens.policy_hash
  }
  ```

- **Signing key:** the gateway's existing Ed25519 signing key (Phase 8 wrapper key). Same KID; same JWKS endpoint at `https://api.zeroship.ai/.well-known/jwks.json`. PATs are not hydra tokens — hydra never sees them. We control the signing key directly; that's the price of "1-year TTL, mintable by any logged-in user without an OAuth dance".
- **Verify:** local Ed25519 verify; load `control.permission_tokens` by `tid`; check `revoked_at IS NULL AND expires_at > now()`; check `policy_hash` matches (defends against post-mint policy edits — a policy edit increments `policy_hash` and invalidates outstanding JWTs of the old shape).
- **Revoke:** `UPDATE control.permission_tokens SET revoked_at = NOW() WHERE id = ?`. Effective on the next request; in-memory caches are invalidated by `tid`.

### 10.2 OAuth access token (third-party apps)

- **Mint:** Authorization Code + PKCE through hydra (§12.1). The token is a hydra-issued RFC 9068 JWT (1 h access, 90 d refresh), signed by hydra's keys, audience-bound to `https://api.zeroship.ai`.
- **Format:** hydra-issued JWT. Verified by `crates/control/src/api.rs` using hydra's JWKS (already cached by the OIDC RP layer).
- **No PAT-style policy attached.** OAuth access tokens carry **scopes**, which translate to `Action` sets at the extractor. Scope vocabulary (P9):
  - `apps:read` `apps:write` `apps:deploy` `apps:delete`
  - `env:read` `env:write` `secrets:read` `secrets:write`
  - `billing:read` `billing:write`
  - `team:read` `team:write`
  - `account:read` `account:write`
  - `deployments:read` `deployments:rollback`
  - One scope per `Action` variant — 1:1 mapping.
- **Two-call still applies.** OAuth scopes constrain what the token can do; the user's underlying permissions still gate. Same `is_authorized_with_token` shape, with the OAuth scopes becoming an implicit `Policy { effect: Allow, actions: <scopes>, resources: Any, conditions: [] }`.

### 10.3 Session cookie (browser sessions)

- **Mint:** OIDC RP flow at the gateway or control plane.
- **Use:** the principal is the session's `user_id`. There is no token-side policy — a logged-in user's session has the full power of the user. Cedar evaluation is single-call: `is_authorized(user, action, resource, context, static_bundle, entities)`.
- **Lifecycle:** existing auth-server session machinery.

### 10.4 Device authorization grant (CLI)

For `zeroship login` from a CLI without a browser. RFC 8628 via hydra (which already supports it; auth-server today does not enable it — P9 turns it on).

Flow:

```
$ zeroship login
Visit https://auth.zeroship.ai/device and enter code: WDJB-MJHT
Waiting for browser...
```

CLI polls hydra's `/oauth2/token` with `device_code`; on success receives an access token + refresh token; persists them to `~/.config/zeroship/credentials.toml`. The CLI then uses the access token via `Authorization: Bearer …` headers exactly like an OAuth third-party app.

The CLI is a first-party OAuth client (`client_id=zeroship-cli`, public, no secret, PKCE-bound). Scopes default to all listed in §10.2 — i.e., the CLI can do everything its user can do.

---

## 11 · Granting paths

Every grant in the system follows one rule: **the grantor must currently be authorized to perform the action it's delegating.** `assert_grantor_authorized(grantor, action, resource)` is the function every grant path calls. No exceptions.

### 11.1 Platform role grant

```
POST /admin/users/usr_01H.../role
Body: { "role": "support" }
Authorization: session cookie of usr_01H...admin...
```

Handler in `crates/control/src/admin_handlers.rs`:

1. `AuthzGuard` resolves the caller; checks `is_authorized(caller, Action::TeamWrite, Resource::Org::"zeroship_platform", …)` against `policies/platform/admin.cedar`. Only `'admin'` rows pass.
2. INSERT/UPDATE `platform.roles`.
3. Emit `platform_role_granted` audit event.

The only path that touches `platform.roles`. No CLI for this — admin UI only — until P11 ships an `org_admin` parallel.

### 11.2 App membership grant

```
POST /apps/app_01H.../members
Body: { "user_id": "usr_01H...", "role": "editor" }   // OR { "email": "bob@…" } for invite flow
Authorization: session cookie of the app owner
```

Handler in `crates/control/src/api.rs`:

1. `AuthzGuard` checks `is_authorized(caller, Action::TeamWrite, Resource::App(app_id), …)`. Pass = caller is owner or has been delegated `team:write` on this app.
2. If body has `email`, issue an invite token (1 h TTL, single-use, stored in `control.app_invites` — table omitted here for brevity, follows `auth.magic_links` shape). Email it. The recipient clicks, signs in if necessary, redirects to a "Accept invite to App X?" page.
3. On accept (or if `user_id` was direct), INSERT `control.app_members`.
4. Emit `app_member_added` audit event.

Removing a member: `DELETE /apps/.../members/<user_id>` → `UPDATE control.app_members SET revoked_at = NOW()`. Implicitly revokes every PAT that user holds against this app (their first-call evaluation now fails).

### 11.3 PAT self-grant (`POST /me/tokens`)

```
POST /me/tokens
Body: {
  "name": "CI deploy bot",
  "expires_at": "2027-05-27T00:00:00Z",     // ≤ 365 days from now
  "policies": [
    {
      "name": "deploy-blog-only",
      "statements": [
        {
          "effect": "Allow",
          "actions": ["apps:read", "apps:deploy", "deployments:rollback"],
          "resources": [{"type": "App", "id": "app_01H...blog..."}],
          "conditions": [
            {"kind": "ip_range", "cidrs": ["10.0.0.0/8"]},
            {"kind": "time_window", "start_min": 540, "end_min": 1080, "tz": "America/Los_Angeles"}
          ]
        }
      ]
    }
  ]
}
Authorization: session cookie of usr_01H...
```

Handler in `crates/control/src/token_handlers.rs` (new):

1. `AuthzGuard` checks `is_authorized(caller, Action::AccountWrite, Resource::Any, …)` — every authenticated user has this.
2. **TOKEN ⊂ USER at mint** — for every `Statement` in the request, for the cross product of its `actions × resources`, call `is_authorized(caller, action, resource, context_with_no_conditions, static_bundle, entities)`. If any call denies, reject the mint with 403 listing the offending pairs. (Cross-product can be large — capped at 10K combinations per request, which is way beyond any reasonable use case.)
3. Lower wrapper → Cedar; compute `policy_hash`; insert row.
4. Mint EdDSA JWT (§10.1); return raw token in response body once (`{ "token": "pat_…", "id": "..." }`).
5. Emit `pat_issued` audit event.

**Mint-time TOKEN ⊂ USER is necessary but not sufficient** — a user's permissions can shrink between mint and use (admin removed them from an app). The two-call evaluation at *use* time is what makes the invariant load-bearing; mint-time validation is just a UX guardrail ("don't let them mint a token that's dead-on-arrival").

### 11.4 Third-party OAuth grant (consent UI)

```
1. Third-party app redirects user-agent to:
   https://auth.zeroship.ai/oauth2/auth?
     client_id=acme-ci&response_type=code&
     scope=apps:read+apps:deploy+deployments:rollback&
     redirect_uri=https://ci.acme.com/oidc/callback&
     state=…&code_challenge=…&code_challenge_method=S256

2. Hydra redirects user-agent to https://auth.zeroship.ai/consent?consent_challenge=…
3. crates/auth /consent renders:
     "ACME CI is requesting:
        Read apps
        Deploy apps
        Roll back deployments
      Allow / Deny"
4. On Allow: PUT hydra-admin /admin/oauth2/auth/requests/consent/accept with grant_scope.
5. Hydra redirects user-agent back to ci.acme.com/oidc/callback?code=…
6. ACME CI exchanges code → access_token + refresh_token at /oauth2/token.
```

The consent UI lives in `crates/auth/src/ui/consent.rs` (already shipped for first-party clients via skip-consent). P9 extends it to render the third-party form with translated scope names.

**`assert_grantor_authorized` analog.** The user is granting actions to a third-party app. Each scope corresponds to an `Action`. The check is: for each requested scope, is the user authorized to perform that action *anywhere*? If the answer is no for all resources (e.g., they request `apps:deploy` and the user has zero apps), the consent UI declines outright with "you don't have permission to grant this". If yes for some resources, consent is for the user's full set — narrowing happens at use time per call.

---

## 12 · OAuth flows

### 12.1 Authorization Code + PKCE for third-party apps

Already in hydra. P9 enables it for non-first-party clients with `skip_consent=false`. The consent UI is the new code.

Client registration (P9 admin-only):

```
POST /admin/oauth-clients
{
  "client_id": "acme-ci",
  "client_name": "ACME CI",
  "client_uri": "https://acme.com",
  "logo_uri": "https://acme.com/logo.png",
  "redirect_uris": ["https://ci.acme.com/oidc/callback"],
  "grant_types": ["authorization_code", "refresh_token"],
  "response_types": ["code"],
  "scope": "apps:read apps:write apps:deploy env:read env:write deployments:rollback",
  "token_endpoint_auth_method": "client_secret_basic",
  "skip_consent": false
}
```

Third-party self-serve OAuth-client registration is a P11+ open question (see §18).

### 12.2 Device Authorization Grant for CLI

Hydra supports RFC 8628 since v25.4. P9 turns it on in `ops/hydra.yaml`:

```yaml
oauth2:
  device_authorization:
    enabled: true
    request_url: https://auth.zeroship.ai/device
    token_polling_interval: 5s
```

The `/device` page in `crates/auth/src/ui/device.rs` (new in P9):

```
1. CLI: POST /oauth2/device/authorize → returns
   { "device_code": "...", "user_code": "WDJB-MJHT", "verification_uri": ".../device", "interval": 5, "expires_in": 600 }
2. CLI prints user_code + verification_uri to terminal.
3. User in browser: GET /device → "Enter the code from your CLI:"
   Form POST → calls hydra admin /admin/oauth2/auth/requests/device/accept.
4. CLI polls /oauth2/token; eventually gets access_token + refresh_token.
```

The user_code → device_code mapping is hydra's responsibility; `crates/auth` only renders the form and forwards to hydra-admin on submit.

### 12.3 Refresh-token rotation

Existing hydra mechanism (already documented in `docs/proposals/auth-server.md` §7). No change in P9.

---

## 13 · Enforcement model

### 13.1 `AuthzGuard` extractor

ntex extractor in `crates/control/src/authz_guard.rs` (new in P9):

```rust
pub struct AuthzGuard {
    pub principal: Principal,
    pub token: Option<Token>,
    pub context: Context,
}

impl FromRequest for AuthzGuard {
    type Error = AuthzError;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        // 1. Parse credential from Authorization header or session cookie.
        // 2. Verify (Ed25519 for PAT, hydra JWKS for OAuth access, DB lookup for session).
        // 3. Load principal entity (auth.users + platform.roles).
        // 4. Build context { ip, mfa_age_seconds, local_minute }.
        // 5. Return guard.
    }
}
```

Handlers extract `AuthzGuard` *and* a separately-extractable `Resource`:

```rust
#[ntex::route("/apps/{id}/deploy", method = "POST")]
async fn deploy(
    guard: AuthzGuard,
    Path(id): Path<AppId>,
    state: State<Arc<AppState>>,
) -> Result<HttpResponse, AuthzError> {
    let resource = Resource::App { id };
    state.authz.enforce(&guard, Action::AppsDeploy, &resource).await?;
    // ... business logic
}
```

`state.authz.enforce` does the two-call dance and logs the decision.

### 13.2 Two-call TOKEN ⊂ USER

```rust
pub async fn enforce(
    &self,
    guard: &AuthzGuard,
    action: Action,
    resource: &Resource,
) -> Result<(), AuthzError> {
    let entities = self.entities.assemble(&guard.principal, resource).await?;

    // Call 1: would the principal be authorized WITHOUT the token?
    let owner_decision = is_authorized(
        &guard.principal,
        action,
        resource,
        &guard.context,
        &self.static_bundle,
        &entities,
    );
    if !owner_decision.allow() {
        self.audit.deny(guard, action, resource, &owner_decision);
        return Err(AuthzError::Forbidden);
    }

    // Call 2: if a token is present, does the token also permit?
    if let Some(token) = &guard.token {
        let token_bundle = self.engine.bundle_for(token).await?;
        let token_decision = is_authorized(
            &Principal::from_token(token),
            action,
            resource,
            &guard.context,
            &token_bundle,
            &entities,
        );
        if !token_decision.allow() {
            self.audit.deny(guard, action, resource, &token_decision);
            return Err(AuthzError::Forbidden);
        }
        self.audit.allow(guard, action, resource, &owner_decision, Some(&token_decision));
    } else {
        self.audit.allow(guard, action, resource, &owner_decision, None);
    }
    Ok(())
}
```

Why two separate calls instead of one merged bundle: the owner's policies are static + per-app-member, and they cache cleanly by `(user_id, app_id) → policy_hash`. The token's policies are per-token. Merging would defeat the static cache. Two calls cost ~3 µs total (Cedar evaluation is ~1.5 µs/call); merging would cost more.

### 13.3 PolicySet caching by `policy_hash`

```rust
struct EngineCache {
    static_bundle: Arc<PolicyBundle>,        // platform + creator base
    token_lru: RwLock<LruCache<TokenId, Arc<PolicyBundle>>>,
}

impl EngineCache {
    fn bundle_for(&self, token: &Token) -> Result<Arc<PolicyBundle>> {
        if let Some(b) = self.token_lru.read().peek(&token.id) {
            if b.policy_hash == token.policy_hash {
                return Ok(b.clone());
            }
        }
        // Cache miss or policy_hash drift (policy edited mid-flight) → recompile.
        let row = self.store.get_token(token.id)?;
        let bundle = Arc::new(PolicyBundle::compile(&row.cedar_source)?);
        debug_assert_eq!(bundle.policy_hash, row.policy_hash);
        self.token_lru.write().put(token.id, bundle.clone());
        Ok(bundle)
    }
}
```

The LRU is sized to 1024 by default — enough for the working set on a single control-plane replica. A miss costs ~10 µs (Cedar parse + index); a hit is a hashmap lookup.

`static_bundle` is loaded once at boot and atomically swapped on `control.platform_policies` change (the admin UI POSTs to a `/admin/platform-policies` route that updates the DB row and triggers `engine.reload_static()`).

### 13.4 Browser-side Cedar lint (P10)

Cedar-WASM in the policy editor (Cedar tab, §7.2):

- Bundle `cedar-policy-wasm` from npm. Worker bundle, ~600 KB gzipped.
- On every edit, debounced 250 ms, call `wasm.validate(source)` → return diagnostics array.
- Diagnostics highlighted inline; save button disabled while errors exist.

This is purely UX — the authoritative validation happens at server-side mint via `cedar_policy::PolicySet::from_str`. Cedar-WASM is never on the request hot path.

### 13.5 Native Rust eval (not Cedar-WASM) at the worker

P12 needs Cedar inside V8 isolates. Options:

1. **Cedar-WASM in V8.** Ships ~600 KB of WASM per isolate; per-isolate memory cost; cold-start hit.
2. **Cedar-Rust via v8class.** Native Rust crate compiled into the worker binary; exposed to V8 via a `#[v8_class]` binding in `crates/plugin-authz/`. Zero per-isolate memory cost; warm Cedar in shared Rust memory.

We pick (2). The `#[v8_class]` macro already powers `env.db.*`, `env.kv.*`, and `env.storage.*` — adding `env.authz` is the same pattern. The user-facing API:

```javascript
// In a creator app:
import { permissions } from "@zeroship/permissions";

await permissions.allow({                    // throws on deny; returns void on allow
  action: "todos:delete",
  resource: { type: "Todo", id: todo.id, owner: todo.owner_id }
});
```

`@zeroship/permissions` calls `env.authz.isAuthorized(...)` which is v8class-bound to `crates/authz::is_authorized`. Compilation of `policies.cedar` happens once at isolate boot (the manifest carries the source; the runtime parses it and stashes the `PolicySet` in isolate-local state).

**Cedar evaluation is single-threaded, allocation-free in the hot path** (Cedar's evaluator can reuse a `cedar_policy::Authorizer`). Per-call cost is ~1 µs; v8class entry/exit is ~200 ns; total per `permissions.allow()` is single-µs. End users can fire dozens of these per request without measurable impact.

---

## 14 · End-user authz inside creator apps (P12)

### 14.1 Bundle shape

```
my-app/
├── package.json
├── policies.cedar
└── src/
    └── index.ts
```

`policies.cedar` (creator-authored):

```cedar
// Read your own todos and todos shared with you.
permit(
  principal,
  action == Action::"todos:read",
  resource
) when {
  resource.owner == principal ||
  principal in resource.shared_with
};

// Only the owner can delete.
permit(
  principal,
  action == Action::"todos:delete",
  resource
) when {
  resource.owner == principal
};

// Admins (a creator-defined group) can do anything.
permit(
  principal in Group::"admins",
  action,
  resource
);
```

At build time (`sdks/vite-plugin/`):

1. Read `policies.cedar` if present.
2. Validate via Cedar-WASM (`cedar-policy-wasm` in node). Compilation errors fail the build.
3. Embed in the manifest: `manifest.authz = { cedar: "<source>" }`.

At app boot in the worker:

1. Read `manifest.authz.cedar`.
2. Parse into a `PolicySet` once.
3. Store as isolate-local state (the same v8class registration path used by `env.db`).

### 14.2 SDK surface — `@zeroship/permissions`

```typescript
// sdks/permissions/src/index.ts
export interface PermissionsCheck {
  action: string;
  resource: {
    type: string;
    id: string;
    [attr: string]: unknown;        // creator-defined attrs (owner, shared_with, …)
  };
  context?: Record<string, unknown>;
}

export const permissions = {
  // Throws PermissionDenied if not allowed.
  async require(check: PermissionsCheck): Promise<void> { … },

  // Returns boolean; never throws.
  async allow(check: PermissionsCheck): Promise<boolean> { … },

  // Returns the set of allowed actions on a resource — for UI gating.
  async allowedActions(resource: PermissionsCheck["resource"]): Promise<string[]> { … }
};
```

The principal is read from `env.auth.user` (already set by the gateway HMAC header). Creators don't pass it.

`allowedActions` is the convenience for "show/hide buttons" — instead of calling `allow()` once per button, the UI gets the full set in one call. Internally it's a Cedar entity query (`is_authorized` over the action vocabulary).

### 14.3 Creator-defined groups

Cedar's entity model lets the creator define groups:

```cedar
// Group membership is inferred from app data at request time.
// principal in Group::"admins" requires the app to expose Group::"admins"
// as a parent entity for the principal.
```

The platform doesn't track these groups — the creator's app code does. To make `principal in Group::"admins"` resolve, the creator's RPC handler builds the entity hierarchy:

```typescript
import { permissions, entity } from "@zeroship/permissions";

// In the creator's RPC handler, before calling permissions.require:
const user = env.auth.requireUser();
const adminRow = await env.db.admins.findOne({ user_id: user.id });
if (adminRow) {
  entity.attach(user, Group("admins"));
}
```

`entity.attach` adds an in-isolate parent edge for this request only. Cedar's evaluator sees `principal in Group::"admins"`. No persistence; per-request graph.

### 14.4 Why declarative file, not API calls

A creator could express the same logic as a chain of `if` statements in their handler. We push toward a declarative `policies.cedar` because:

- **Auditable.** A creator's policy is a file in git history, not buried in a 500-line handler.
- **Static analyzable.** Cedar has a formal analyzer (P11) — we can warn at build time about contradictions, unreachable policies, etc.
- **Composable.** The platform can offer policy templates ("multi-tenant app", "social with friends", "team workspace") that creators drop in.
- **Cheap.** A Cedar evaluation is a microsecond; an `if` chain across DB queries is a millisecond.

Imperative is always available — `if (user.id !== todo.owner_id) throw new Error(...)` works fine. We just steer toward the declarative path.

---

## 15 · Audit model

Every `is_authorized` call logs one row. The schema (§9.3) records:

```json
{
  "occurred_at": "2026-05-27T18:42:11.123Z",
  "request_id": "req_01H...",
  "principal_id": "usr_01H...alice...",
  "token_id": "pat_01H...",                    // null if session-cookie auth
  "action": "apps:deploy",
  "resource_type": "App",
  "resource_id": "app_01H...blog...",
  "decision": "allow",
  "matched_policies": ["pol_app_owner", "pol_pat_deploy_blog_only"],
  "context": {
    "ip": "10.0.5.42",
    "mfa_age_seconds": 312,
    "local_minute": 678
  }
}
```

The `matched_policies` list is Cedar's `Decision::diagnostics().reason()` output — the policy IDs that contributed to the allow (or the policies that explicitly denied, for deny outcomes). This is the load-bearing audit artifact: "why did this request allow?" answers in one query.

**Sampling.** Allow decisions are emitted at 1% sample rate by default (configurable per `--authz-audit-allow-sample`); deny decisions always at 100%. Allow at 100% would 5×-10× the audit-events table; the sample is for debugging policy regressions, and a 1% sample over a day still gives thousands of rows per policy.

**Replay.** Given a row + a policy bundle, we can replay the decision deterministically. CI runs a replay-loop on every policy change: take 10K production rows, swap the new policy bundle in, assert decision matches expected. Catches policy regressions before they ship.

**Per-token audit page.** The dashboard surfaces "last 100 decisions by this PAT" — useful for users to spot a misconfigured CI bot.

---

## 16 · Failure modes & mitigations

| Failure mode | Cause | Mitigation |
|---|---|---|
| **Cache poisoning** | An attacker mints PAT A with hash H, then somehow swaps the row for PAT B's policy. Cache serves stale. | `policy_hash` is part of both the JWT and the row; mismatch invalidates the cache entry. Compile uses the row, not the cached body. |
| **Policy bypass via cross-tenant entity** | Cedar `entity.attach` (P12 creator side) lets the creator declare any parent edge. If a creator's handler attaches `Group::"admins"` to every principal, all checks pass. | Documented as creator footgun. The platform is not responsible for a creator's broken authorization any more than for their broken SQL. Compile-time lint (P11 Cedar analyzer) catches "every principal in `Group::"admins"`" patterns. |
| **TOKEN ⊂ USER drift at policy edit** | User edits a policy; outstanding PATs minted under the old policy reference stale `policy_hash`. | Verification fails; clients see 401 with `Cache-Control: no-store`; they re-mint or the user re-issues a token. We do NOT cascade-update existing PATs — the old policy is gone, the new shape requires explicit re-grant. |
| **TOKEN ⊂ USER drift at app-member revoke** | Admin removes a user from `control.app_members`; the user's PAT for that app still has the old policy. | Two-call evaluation handles this: Call 1 (owner-without-token) checks current membership. Membership row is gone → deny. The PAT's own policy is irrelevant. |
| **Hydra outage** | OAuth access tokens (third-party) need hydra JWKS for verification. Hydra unreachable → can't verify new tokens. | JWKS is cached at the gateway/control RP layer (already shipped). Cache TTL 24 h, so a multi-hour hydra outage doesn't break verification. New tokens can't mint, but in-flight ones keep working. |
| **PAT signing-key compromise** | The gateway Ed25519 signing key is exfiltrated → attacker mints arbitrary PATs. | Same blast radius as wrapper tokens (Phase 8). Mitigation: rotation. P11 adds quarterly signing-key rotation with a JWKS endpoint; old keys retired after refresh-token max lifetime. |
| **Malformed policy on disk** | Operator overrides via `control.platform_policies` insert an invalid Cedar source. | `engine.reload_static()` is a transaction: parse first, swap second. Parse failure leaves the running bundle untouched and emits `platform_policy_invalid` audit. |
| **Policy evaluation panic** | A Cedar bug panics on a specific entity shape. | `is_authorized` is wrapped in `std::panic::catch_unwind`. On panic → audit emit + deny. Failing closed is the default. |
| **Audit-table fill** | High traffic + 100% allow sampling. | Sample rate; retention cron (90 d for allow, 365 d hot / 7 y cold for deny). |
| **Mint flood (DoS)** | Attacker hammers `POST /me/tokens`. | Rate limit per-user (10/hr) + per-IP (60/hr). |
| **Cedar analyzer skew** | Build-time analyzer says "this is fine", runtime says deny because of an entity that didn't exist at build. | Acceptable — analyzer is best-effort. Runtime is the source of truth. |
| **End-user authz disabled in dev** | Creator forgets to ship `policies.cedar`; everything denies. | Default policy when manifest has no `authz` block: `permit(principal, action, resource);` (allow-all). Documented as "you have no authz; you can opt in by adding `policies.cedar`". |

---

## 17 · Phasing (P9 — P12)

Phase numbers continue the auth-server phasing (P1 — P8 already shipped). Each phase exits when the listed criteria hold.

### 17.1 P9 — engine + platform RBAC + PAT + Device Grant

**Scope.** The minimum viable authz layer. No wrapper UI yet (admin-only authoring); no end-user authz; no orgs.

**Files touched.**

```
NEW: crates/authz/Cargo.toml
NEW: crates/authz/src/{lib,action,resource,condition,effect,policy,statement,lower,engine,eval,audit,entities,error,platform_role}.rs
NEW: crates/authz/build.rs                  // compiles policies/platform/*.cedar + policies/creator/*.cedar
NEW: crates/authz/tests/{lower_snapshot,two_call_subset,condition_eval}.rs
NEW: policies/platform/{admin,support,billing,readonly}.cedar
NEW: policies/creator/{app_owner,app_editor,app_viewer}.cedar
NEW: crates/control/src/authz_guard.rs
NEW: crates/control/src/token_handlers.rs    // POST /me/tokens, DELETE /me/tokens/<id>, GET /me/tokens
NEW: crates/control/src/admin_handlers.rs    // POST /admin/users/<id>/role, POST /admin/platform-policies
NEW: crates/auth/src/ui/device.rs             // RFC 8628 device-code page
MOD: crates/control/src/api.rs                // wire AuthzGuard into every existing handler
MOD: crates/auth/src/store/migrations.rs      // add control.{app_members,permission_tokens,platform_policies,authz_decisions} + platform.roles
MOD: ops/hydra.yaml                           // enable oauth2.device_authorization
MOD: crates/cli/                              // zeroship login (device grant), zeroship policy edit
```

**Exit criteria.**

- Every existing control-plane route enforces via `AuthzGuard`. No bypass paths.
- `POST /me/tokens` mints PATs with TOKEN ⊂ USER validation at mint.
- Two-call evaluation in `is_authorized_with_token` is the only path that handles PAT-bearing requests.
- `zeroship login` works (device grant via hydra).
- All four `platform.roles` values pass acceptance tests (`admin` can do everything, `readonly` denies writes, etc.).
- `policies/platform/*.cedar` and `policies/creator/*.cedar` are parse-clean on build.
- Audit rows land in `control.authz_decisions` for every `enforce()` call.

**~Test count delta.** ~80 new tests (40 in `crates/authz`, 30 in `crates/control`, 10 in `crates/cli`).

### 17.2 P10 — wrapper SDK + policy editor UI + per-token audit

**Scope.** Make Cedar invisible to creators. Policy editor in the dashboard. Per-PAT audit view.

**Files touched.**

```
NEW: dashboard/src/routes/tokens/[id]/edit/+page.svelte    // toggle matrix
NEW: dashboard/src/routes/tokens/[id]/edit/cedar/+page.svelte  // Cedar tab w/ wasm lint
NEW: dashboard/src/lib/policy/toggle-matrix.ts             // matrix ↔ Policy {} bidirectional
NEW: dashboard/src/lib/policy/cedar-lint.ts                // wraps cedar-policy-wasm
NEW: dashboard/src/lib/policy/conditions.ts                // condition library UI components
NEW: dashboard/src/routes/tokens/[id]/audit/+page.svelte
MOD: crates/control/src/token_handlers.rs                  // GET /me/tokens/<id>/audit (paginated)
MOD: sdks/control/src/tokens.ts                            // typed client for the above
```

**Exit criteria.**

- Toggle matrix produces a `Policy` JSON identical (after lowering) to a hand-written Cedar equivalent. Round-trip test: pick 20 representative Cedar policies, generate the matching matrix state, render to JSON, lower to Cedar, parse, diff entities. Zero drift.
- Cedar tab lints in <100 ms typical; large policies (1 KB) under 500 ms.
- A user who edits a PAT in the Cedar tab sees the matrix tab in read-only banner mode.
- Per-token audit page renders last 100 decisions.

**~Test count delta.** ~40 new tests (mostly Playwright on the dashboard; ~10 in `crates/control`).

### 17.3 P11 — orgs + Cedar analyzer + incident lock

**Scope.** Hierarchies. Static analysis. Big-red-button.

**Files touched.**

```
NEW: crates/control/src/orgs.rs                  // CRUD on control.orgs + control.org_members
NEW: policies/platform/incident_lock.cedar       // `forbid(principal, action, resource) when {context.incident_lock};`
NEW: crates/authz/src/analyzer.rs                // wraps cedar-policy's Validator
NEW: scripts/ci-policy-lint.sh                   // CI gate on policies/**
MOD: crates/authz/src/resource.rs                // Org variant w/ parent walks
MOD: crates/authz/src/entities.rs                // build Org parents for Apps
MOD: crates/control/src/admin_handlers.rs        // POST /admin/incident-lock (sets a runtime flag)
MOD: crates/control/src/api.rs                   // every handler reads incident_lock flag into context
```

**Exit criteria.**

- A user added to `control.org_members` with role `'org_admin'` can manage every app in that org without explicit per-app membership.
- `cedar-policy::Validator` runs in CI on every PR touching `policies/**`. Catches unreachable policies, undefined entities, type errors.
- `POST /admin/incident-lock` flips a flag; every subsequent authz call within 10 s denies with reason "incident_lock". Releasing requires two admins (`POST /admin/incident-lock/release` requires `admin` role + a recent `audit_event` from a *different* admin within 1 h).

**~Test count delta.** ~50 new tests.

### 17.4 P12 — end-user authz via v8class + `@zeroship/permissions`

**Scope.** Creator-app-internal authz. The big rollout to creators.

**Files touched.**

```
NEW: crates/plugin-authz/Cargo.toml
NEW: crates/plugin-authz/src/lib.rs              // #[v8_class] bindings for is_authorized
NEW: sdks/permissions/package.json
NEW: sdks/permissions/src/{index,types,entity}.ts
NEW: sdks/vite-plugin/src/authz.ts               // build-time policies.cedar discovery + manifest emit
MOD: crates/bundle/src/manifest.rs               // Manifest.authz: Option<CedarSource>
MOD: crates/runtime/src/core/init.rs             // wire plugin-authz into the v8 env
MOD: docs/reference/auth.md                     // (already covers authn; we keep this scoped)
NEW: docs/reference/permissions.md               // the @zeroship/permissions surface
```

**Exit criteria.**

- A creator can add `policies.cedar` to their app, deploy, and `permissions.require()` enforces it.
- `permissions.allow()` returns boolean within <5 µs (microbench).
- An app without `policies.cedar` permits everything (no breakage on existing apps when P12 ships).
- The v8class binding is allocation-free in the hot path (verified by isolate heap snapshot diff before/after 100K calls).

**~Test count delta.** ~60 new tests (most in `sdks/permissions` and `crates/runtime` integration tests).

---

## 18 · Open questions

1. **OAuth-client self-serve registration.** P11 admin-only is fine for v1, but third-party developers will want to register their own client_id. Options: (a) ship `oauth2_dynamic_client_registration` from hydra (RFC 7591); (b) build a dashboard form that POSTs to hydra-admin; (c) defer. (b) feels right but adds an admin surface; what's the abuse posture? Recommend a developer-portal-style flow gated on email-verified accounts.

2. **Conditions as Cedar functions vs context attrs.** Today we compile every condition into a `context.X` check. That means the extractor has to compute every possible context field per request. Alternative: define Cedar custom functions (`ipInRange(...)`, `timeOfDayBetween(...)`) and call them in `when`. Cedar supports extensions but the registered-function set is global; we'd be adding to Cedar's vocabulary, not just our wrapper's. Recommend stick with context attrs at v1, revisit at P11 if the context shape balloons.

3. **PAT vs OAuth grant for first-party CLI.** Today we say "CLI uses Device Grant + OAuth access token". Alternative: CLI mints a PAT via the device-paired browser session. Pro: 1-year TTL, no refresh dance. Con: PAT minting requires a logged-in session, so the CLI still has to bounce through the browser. Both paths work; the device-grant path is more standard.

4. **PAT signing-key rotation cadence.** Phase 8 wrappers run on a single Ed25519 key. P9 PATs reuse it. When do we rotate? 90 days matches OIDC industry norm; 365 days matches our PAT max TTL (a PAT minted on day 0 of a key is still valid the day before the key retires). Recommend 365 d with 30 d overlap.

5. **Cedar policy templates in the dashboard.** "Make my app multi-tenant" is a common ask. Should P10 ship a templates gallery (`Multi-tenant SaaS`, `Marketplace with admin`, `Personal-data-only`)? Or wait until P12 to see what creators ask for?

6. **Audit retention split.** Allow rows at 90 d, deny at 365 d hot + 7 y cold. Is the 7 y cold storage the same Postgres or a separate archive? Operator decision; document the runbook before P9 ships.

7. **Cross-region audit replication.** If we go multi-region, the audit table is the most synchronization-sensitive (compliance). Single-writer for now; revisit at P11 when org hierarchies might cross regions.

8. **`Resource::Any` semantics.** We use `Any` for actions without a target (`AccountRead` for "view own profile"). Should we instead require `Resource::User { id }` and let Cedar match `principal == resource.id`? Cleaner, but the extractor has to figure out the target. Recommend keep `Any` at v1.

---

## 19 · Decisions log

Every "X over Y because Z" choice in this proposal, in order.

1. **Cedar over OpenFGA, Casbin, Oso, OPA, homegrown.** Cedar has a formal semantics, a static analyzer, an embeddable Rust crate, AND a WASM build for browser lint. OpenFGA needs a server. Casbin's perf is fine but it has no static analyzer. Oso's open-source split is incomplete; Rust support is unclear. OPA is JSON-config Rego; the language is harder to teach than Cedar. Homegrown is a forever liability.

2. **Wrapper SDK over Cedar-exposed-everywhere.** Cedar's syntax is fine for engineers, hostile for creators on a phone. A closed-vocabulary wrapper lets us ship a toggle matrix UI that hides Cedar from 95% of users while keeping the Cedar tab for power users. The wrapper also bounds our condition surface — every `Condition` variant maps to one context attribute we have to compute.

3. **Closed `Action` enum over free-form action strings.** Free-form lets creators express actions the platform doesn't enforce. A closed enum makes every action grep-able and lint-able. We pay a PR per new action; that's the right friction.

4. **Closed `Resource` enum over arbitrary entity types.** Same reason as Action. P12 inverts this — creators define resource types inside their apps. That's the price of flexibility; the platform-side enum stays closed.

5. **Closed `Condition` enum over arbitrary Cedar `when`.** A creator authoring "deny unless `principal.department == 'Engineering' && resource.tag in {'public','internal'}`" needs full Cedar — but that's not the 95% path. The 95% is "deny outside business hours" and "deny from non-VPN IPs". Closed condition library covers both with two variants.

6. **`Effect::Allow` + `Effect::Deny` over Allow-only.** Cedar supports deny-overrides. A user authoring "permit everything except secrets:write" wants Deny. We expose it.

7. **JSONB column for token policies over a relational policy schema.** Policies are write-rarely, read-often, and have a complex shape. JSONB + `policy_hash` index gives us O(1) cache lookups and trivial migrations. A relational `policy_statements` table would be 3 joins per read for no benefit.

8. **Cedar source files in repo for static policies, JSONB for dynamic.** Static platform policies are code; they belong in `policies/*.cedar` with the rest of the codebase, lintable in CI, versioned in git. Per-PAT policies are user data; they belong in JSONB.

9. **PolicySet derived at runtime, never persisted.** Cedar's `PolicySet` is a parse tree; persisting it would couple us to Cedar's internal representation. Source code → PolicySet at boot/edit is cheap.

10. **TOKEN ⊂ USER via two Cedar calls over a merged PolicySet.** Merging requires recompiling the static bundle for every token. Two calls share the same static `Arc<PolicyBundle>`; cache wins. ~3 µs total either way; the cache wins.

11. **Gateway-issued EdDSA JWT for PATs over hydra-issued tokens.** Hydra tokens are 1 h sliding. We want 1 year for PATs. We could ask hydra to extend, but that breaks hydra's threat model (long-lived tokens deserve long-form metadata). Our own signing path is simpler. We already issue wrapper tokens (Phase 8) — PATs reuse the key.

12. **`policy_hash` claim in the PAT JWT over DB-only state.** Without the hash, a policy edit silently mutates outstanding tokens — bad. With the hash, the JWT is invalidated by edit, forcing a re-mint. The user has visibility.

13. **Cedar engine via v8class over Cedar-WASM in V8.** Native Rust eval is faster (no WASM boundary), zero per-isolate memory cost, and we already use v8class for every other native primitive. Cedar-WASM lives in the browser editor only.

14. **OAuth Authorization Code + PKCE for third-party apps over implicit/device.** PKCE is the modern code-grant. Implicit is deprecated by OAuth 2.1. Device is for headless; third-party apps generally aren't.

15. **OAuth Device Authorization Grant for CLI over username/password against hydra.** Device flow is the OAuth-blessed CLI path. No browser-out-of-band, no MFA bypass, no credential capture.

16. **OAuth scopes ≅ Action enum (1:1) over scope sets.** Granular scopes simplify the consent UI ("ACME CI wants: Read apps, Deploy apps") and the wrapper-side mapping (scope string → `Action` variant). Sets of scopes (`"app_admin"` = `apps:* + env:* + secrets:*`) hide power.

17. **Mint-time TOKEN ⊂ USER validation as UX guardrail, not security.** Mint-time catches "you can't mint that" early. Use-time two-call evaluation is the load-bearing check. Belt and suspenders.

18. **`policies/` directory in repo for static platform policies, `control.platform_policies` table for operator overrides.** Same code-vs-data split as before. Operators can patch the policy set without rebuilding; the base set is git-tracked.

19. **`AuthzGuard` ntex extractor over per-handler boilerplate.** Every authz check needs principal + token + context. Extractor centralizes parsing + caching of all three. Adding `AuthzGuard` to a handler is a one-line change.

20. **Resource extraction separate from `AuthzGuard`.** The extractor doesn't know which path param is the resource. Forcing it to be `Path<AppId>` couples authz to URL design. Handlers call `state.authz.enforce(&guard, action, &resource)` explicitly. Verbose; explicit.

21. **`assert_grantor_authorized` as the single grant-path entry.** Without it, grant paths can diverge — admin-handler might check role, app-member-handler might check ownership, PAT might check session-only. With it, every grant is bounded by "the grantor must be authorized to perform what they're granting".

22. **No `@deprecated` aliases on rename.** Per AGENTS.md pre-launch posture.

23. **Allow at 1% audit sampling, Deny at 100%.** Allow under load floods the table; deny is rare and load-bearing. Configurable, with sensible defaults.

24. **Cedar's reasoning policies as the matched-policies audit field.** Cedar surfaces this natively via `Decision::diagnostics().reason()`. We don't reinvent.

25. **End-user authz default = allow-all.** A bundle without `policies.cedar` should not break; it should be permissive. Creators opt in.

26. **Static analyzer in CI from P11, not P9.** Cedar's analyzer is built but the API surface around it (`cedar-policy-validator`) is best-effort at the time of writing. We use it as a CI lint, not a build gate.

27. **`Org` and `incident_lock` deferred to P11, not P9.** Both are nice-to-haves; the core authz layer (P9) doesn't need them.

28. **Cedar-WASM only browser-side.** Server-side Rust is faster and avoids a second Cedar implementation. Cedar's Rust + WASM share the same parser, so divergence is structural-only (memory layout), not semantic.

---

## 20 · Out of scope

- **Attribute-based access control beyond the closed condition library.** Creators who need `principal.department == "eng" && resource.tag in {...}` use the Cedar power-user tab in their app bundle (P12). Platform-level ABAC is not a goal.
- **SCIM, LDAP, AD federation.** Not in v1.
- **Step-up authn within a session.** A request that needs MFA today triggers a redirect to `/login?prompt=login&acr=urn:zeroship:mfa`. Authz consumes the `mfa_age_seconds` context attr; the step-up dance is auth-server's job.
- **Policy version control.** We store the current `cedar_source` per token + platform policy. We don't keep a history. Audit log carries decisions; re-creating the policy state at time T is "look at git for the static bundle, look at `last_modified` for the table row". Past `cedar_source` versions are not retained.
- **Cross-tenant policy sharing.** Creators can't subscribe to another creator's policy template. P10 templates are platform-curated.
- **OPA / Rego fallback.** Cedar is the engine.
- **JIT (just-in-time) access requests / approval workflows.** "Bob requests `apps:deploy` for 1 h" is a future feature. v1 grants are atomic — mint or don't.
- **Policy diff UI.** Edit history not retained; diff is just before-and-after on the current version.
- **Cedar's `is_authorized_partial` for partial evaluation.** Useful when the resource isn't known yet (e.g., "what apps can this user deploy to?"). P10 or P11 — useful for the dashboard's app list rendering, not load-bearing for security.
- **Hardware attestation for PATs.** No `cnf.x5t#S256` on PATs. The Phase 8 wrapper-token mechanism for end users is the precedent for hardware binding; PATs are CI/script credentials by design.
- **Tenant-isolated audit tables.** One control plane, one audit table. Multi-tenant audit isolation is a P11+ open question (#7).
