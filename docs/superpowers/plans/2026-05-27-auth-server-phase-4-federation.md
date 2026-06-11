# Auth server — Phase 4 implementation plan: Federation (Google + GitHub)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add Google OIDC + GitHub OAuth federation to `crates/auth`. By the end, a user can sign in with Google or GitHub at `auth.zeroship.ai/login`, the account-linking decision tree handles existing-email collisions safely, and third-party RPs see a proper consent screen (replacing P2's first-party-only skip-consent).

**Architecture:** Federation runs at our auth host, NOT at hydra. Hydra knows nothing about Google/GitHub — those are OUR upstream identity providers. The flow: user clicks "Sign in with Google" on `/login` → we redirect to Google's `/o/oauth2/v2/auth` → Google returns to our `/oauth/google/callback` → we exchange the code, verify Google's ID token (using `core::oidc_verify`'s JwksCache pointed at Google's JWKS), find-or-create the local user, then accept_login on the pending hydra `login_challenge`.

**Tech Stack:** Continuing from Phases 1-3: compio, ntex, cyper, compio-postgres, hydra v25.4, argon2, askama. New surfaces: `crates/auth/src/identity/oauth/{google,github}.rs`, `crates/auth/src/ui/{link,me,consent}.rs`, `auth.identities` CRUD.

**Reference docs:**
- `docs/archive/auth-server.md` §8.2 (Google), §8.3 (GitHub), §10.3 (third-party consent UI)
- `docs/superpowers/plans/2026-05-27-auth-server-phase-3-oidc-rps.md` — pattern reference
- Phase 1 deleted `crates/auth/src/oauth/{google,github}.rs` — those files can be raided for the Google PKCE shape (git log will recover them if needed)

**Pre-launch posture:** no back-compat. Federation lands as a single PR's worth of work.

**Starting point:** worktree tip post-Phase-3 (`df101156` after `auth-phase-3` tag). Live stack still up.

---

## Phase 4 unit list

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | `auth.identities` CRUD + Google OAuth client config | identities.rs + google.rs scaffolding | 30 min |
| U2 | Google OIDC federation flow | `/oauth/google/{start,callback}` | 60 min |
| U3 | GitHub OAuth federation flow | `/oauth/github/{start,callback}` | 50 min |
| U4 | Account-linking decision tree + `/link` UI | linker.rs + link.html | 60 min |
| U5 | Third-party consent UI (real form, replaces skip-only) | consent.rs rewrite + consent.html | 50 min |
| U6 | `/me` profile page (linked identities) | me.rs + me.html | 40 min |
| U7 | Templates: Sign-in buttons + provider icons + me + link + consent | askama .html + CSS | 30 min |
| U8 | Federation e2e tests (mocked providers) | tests/e2e_google.rs + tests/e2e_github.rs | 70 min |
| U9 | Phase 4 close-out + tag | – | 10 min |

Total: ~6h, ~12 commits.

---

# Unit U1 · `auth.identities` CRUD + provider config

The `auth.identities` table already exists (Phase 1 migration). We just need the CRUD module + a config carrier for Google/GitHub credentials.

## Task U1.1 · Identities store

- [ ] **Step U1.1.1:** Create `crates/auth/src/store/identities.rs`:

```rust
//! `auth.identities` CRUD — OAuth/OIDC provider linkages keyed on (provider, subject).

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct Identity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider: String,
    pub subject: String,
    pub email_at_link: Option<String>,
}

/// Find an identity by (provider, subject). None if not linked.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn find_by_provider_subject(
    conn: &Client,
    provider: &str,
    subject: &str,
) -> Result<Option<Identity>> {
    let rows = conn
        .query(
            "SELECT id, user_id, provider, subject, email_at_link::text \
             FROM auth.identities \
             WHERE provider = $1 AND subject = $2",
            &[&provider, &subject],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities find: {e}")))?;
    Ok(rows.first().map(row_to_identity))
}

/// Link an identity to an existing user.
///
/// # Errors
///
/// `AuthError::Db` on PG failure (including duplicate-link constraint).
pub async fn link(
    conn: &Client,
    user_id: Uuid,
    provider: &str,
    subject: &str,
    email_at_link: Option<&str>,
    raw_profile: Option<&serde_json::Value>,
) -> Result<Identity> {
    let rows = conn
        .query(
            "INSERT INTO auth.identities (user_id, provider, subject, email_at_link, raw_profile) \
             VALUES ($1, $2, $3, $4::citext, $5) \
             RETURNING id, user_id, provider, subject, email_at_link::text",
            &[&user_id, &provider, &subject, &email_at_link, &raw_profile],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities link: {e}")))?;
    let row = rows.first().ok_or_else(|| AuthError::Db("identities link: empty return".into()))?;
    Ok(row_to_identity(row))
}

/// List a user's linked identities (for /me page).
///
/// # Errors
///
/// `AuthError::Db`.
pub async fn list_for_user(conn: &Client, user_id: Uuid) -> Result<Vec<Identity>> {
    let rows = conn
        .query(
            "SELECT id, user_id, provider, subject, email_at_link::text \
             FROM auth.identities WHERE user_id = $1 \
             ORDER BY linked_at",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities list: {e}")))?;
    Ok(rows.iter().map(row_to_identity).collect())
}

/// Unlink. Returns true if a row was deleted.
///
/// # Errors
///
/// `AuthError::Db`.
pub async fn unlink(conn: &Client, user_id: Uuid, provider: &str) -> Result<bool> {
    let affected = conn
        .execute(
            "DELETE FROM auth.identities WHERE user_id = $1 AND provider = $2",
            &[&user_id, &provider],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities unlink: {e}")))?;
    Ok(affected > 0)
}

fn row_to_identity(row: &compio_postgres::Row) -> Identity {
    Identity {
        id: row.get("id"),
        user_id: row.get("user_id"),
        provider: row.get("provider"),
        subject: row.get("subject"),
        email_at_link: row.try_get::<_, String>("email_at_link").ok(),
    }
}
```

- [ ] **Step U1.1.2:** Register `pub mod identities;` in `crates/auth/src/store/mod.rs`.

- [ ] **Step U1.1.3:** Add a live-PG test at `crates/auth/tests/identities_test.rs`:
  - Skip if `AUTH_DB_URL` unset.
  - Seed a user. Link an identity. Find by (provider, subject) returns it. List for user returns it. Unlink. Find returns None.

- [ ] **Step U1.1.4:** Commit: `auth: store/identities — find/link/list/unlink for OAuth provider linkages`.

## Task U1.2 · OAuth provider config

- [ ] **Step U1.2.1:** Extend `crates/auth/src/config.rs::AuthConfig` with optional Google + GitHub OAuth credentials:

```rust
#[arg(long, env = "AUTH_GOOGLE_CLIENT_ID")]
pub google_client_id: Option<String>,
#[arg(long, env = "AUTH_GOOGLE_CLIENT_SECRET")]
pub google_client_secret: Option<String>,
#[arg(long, env = "AUTH_GITHUB_CLIENT_ID")]
pub github_client_id: Option<String>,
#[arg(long, env = "AUTH_GITHUB_CLIENT_SECRET")]
pub github_client_secret: Option<String>,
```

Each provider is optional — auth boots without them, just doesn't expose the federation routes.

- [ ] **Step U1.2.2:** In `main.rs`, gate federation route registration on presence of credentials. Log a warning if missing: `tracing::warn!("Google OAuth disabled — set AUTH_GOOGLE_CLIENT_ID/SECRET to enable")`.

- [ ] **Step U1.2.3:** Commit: `auth: config — optional Google + GitHub OAuth client credentials`.

---

# Unit U2 · Google OIDC federation

## Task U2.1 · Google OAuth module

- [ ] **Step U2.1.1:** Create `crates/auth/src/identity/oauth/mod.rs` (stub) and `crates/auth/src/identity/oauth/google.rs`.

The Google flow:
1. `/oauth/google/start?login_challenge=<...>` — generate state + PKCE verifier + nonce, stash in short-lived cookie, redirect to Google's authorize endpoint with `scope=openid email profile`.
2. `/oauth/google/callback?code=&state=` — exchange code at Google's `/token`, fetch Google's JWKS via `core::oidc_verify::JwksCache` (URL `https://www.googleapis.com/oauth2/v3/certs`), verify Google's ID token (iss=`https://accounts.google.com`, aud=our_client_id, nonce match), extract sub+email+name+picture.

Then call `auth::linker::resolve_or_link` (U4) to find-or-create the local user, then `hydra::accept_login(login_challenge, { subject: user.id, acr: "urn:zeroship:google", amr: ["oauth"] })`.

Provide:
- `pub async fn start(query: &StartQuery, cfg: &AuthConfig) -> Result<(String, String)>` returning `(authorize_url, stash_cookie_value)`.
- `pub async fn finish(query: &CallbackQuery, stash: &str, cfg: &AuthConfig, jwks: &JwksCache) -> Result<GoogleIdentity>` returning the verified profile (struct: `sub`, `email`, `email_verified`, `name`, `picture`, `hd` — for Workspace domain).

The full plan source for this module is ~250 LOC; pattern off the deleted `crates/auth/src/oauth/google.rs` (recoverable from git) for the cyper-based code-exchange shape, but verify ID token via `core::oidc_verify` (NEW) rather than hand-rolling.

- [ ] **Step U2.1.2:** Unit-test the URL builder + stash round-trip (no live Google).

- [ ] **Step U2.1.3:** Commit: `auth: identity/oauth/google — OIDC federation client (start + callback)`.

## Task U2.2 · Wire `/oauth/google/{start,callback}` routes

- [ ] **Step U2.2.1:** Create `crates/auth/src/ui/oauth_google.rs` — thin HTTP handlers that call into `identity::oauth::google`.

- [ ] **Step U2.2.2:** Wire into `server::configure` only if `cfg.google_client_id.is_some()`.

- [ ] **Step U2.2.3:** On callback success: extract `login_challenge` from the stash, call `linker::resolve_or_link`, then `hydra_client::accept_login`.

- [ ] **Step U2.2.4:** Commit: `auth: /oauth/google/{start,callback} — Google sign-in route`.

---

# Unit U3 · GitHub OAuth federation

Same shape as U2, but:
- GitHub is OAuth 2.0 (not OIDC). No ID token. We fetch `/user` + `/user/emails` after the code exchange.
- GitHub supports PKCE since July 2025 (per the research brief), but still requires the client secret. Treat as confidential client.
- The "verified primary email" picker: scan `/user/emails`, pick `{ primary: true, verified: true, NOT email.endsWith("@users.noreply.github.com") }`. If none qualifies, abort with clear error.

## Task U3.1 · GitHub OAuth module

- [ ] **Step U3.1.1:** Create `crates/auth/src/identity/oauth/github.rs`.

```rust
pub struct GitHubIdentity {
    pub subject: String,   // GitHub's numeric user.id as a string
    pub login: String,
    pub email: String,     // primary + verified, not @users.noreply
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

pub fn start_authorize_url(cfg: &AuthConfig) -> (String, StashCookie) { ... }
pub async fn complete_callback(code: &str, verifier: &str, cfg: &AuthConfig) -> Result<GitHubIdentity> {
    // 1. POST github.com/login/oauth/access_token with code+verifier+secret
    // 2. GET api.github.com/user with bearer access_token  → user.id, user.login, user.name, user.avatar_url
    // 3. GET api.github.com/user/emails → pick primary+verified
    // 4. Reject if none qualifies
    // 5. Return GitHubIdentity
}
```

Cyper handles both POST (form-encoded) and GET (with Authorization header). All endpoints over HTTPS.

- [ ] **Step U3.1.2:** Unit-test the verified-primary-email picker against a fixed JSON fixture.

- [ ] **Step U3.1.3:** Commit: `auth: identity/oauth/github — OAuth 2.0 federation client (with verified-primary picker)`.

## Task U3.2 · Wire `/oauth/github/{start,callback}` routes

- [ ] **Step U3.2.1:** Mirror U2.2's pattern with the GitHub identity.

- [ ] **Step U3.2.2:** Commit: `auth: /oauth/github/{start,callback} — GitHub sign-in route`.

---

# Unit U4 · Account-linking decision tree + `/link` UI

The decision tree on every federation callback (per proposal §8.2 step 3):

1. `identities::find_by_provider_subject(provider, sub)` — if found, that's the user. Done.
2. Else: `users::find_by_email(email)`. If found:
   - If user's email is verified locally (`email_verified_at IS NOT NULL`) AND provider says email_verified=true AND (for Google: hd claim present OR email ends @gmail.com) → redirect to `/link?provider=google&login_challenge=...` for explicit confirmation.
   - Else: do NOT auto-link (the `failedstartup.com` domain re-registration attack). Force a verification step or refuse.
3. Else: brand-new user. `users::create(email, name, password_hash=None)` + `identities::link(user_id, provider, sub, email, raw_profile)`. Mark `email_verified_at = NOW()` since the provider verified.

## Task U4.1 · `linker::resolve_or_link`

- [ ] **Step U4.1.1:** Create `crates/auth/src/identity/linker.rs`:

```rust
pub enum LinkOutcome {
    Existing { user_id: Uuid },              // identity already linked
    Created { user_id: Uuid },               // brand new account
    NeedsConfirmation { pending_token: String }, // email-collision; user must confirm via /link
}

pub async fn resolve_or_link(
    db: &Client,
    provider: &str,
    profile: &ResolvedProfile,
) -> Result<LinkOutcome> { ... }

pub struct ResolvedProfile {
    pub subject: String,
    pub email: String,
    pub email_verified: bool,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub provider_trusted_for_email: bool, // true for @gmail.com / Workspace hd, GitHub primary+verified, etc.
    pub raw_profile: Option<serde_json::Value>,
}
```

The `pending_token` for "needs confirmation" is a short-lived (10 min) signed token stashed in a cookie. The `/link` GET reads it; on POST-with-correct-password, the link gets created.

- [ ] **Step U4.1.2:** Unit-test the three outcomes against an in-memory DB or live PG.

- [ ] **Step U4.1.3:** Commit: `auth: identity/linker — account-linking decision tree (find/create/confirm)`.

## Task U4.2 · `/link` UI

- [ ] **Step U4.2.1:** Create `crates/auth/src/ui/link.rs`:
  - GET `/link?token=<pending>` — render a form: "An account with email X exists. Sign in with your password to link your <provider> account."
  - POST `/link` — verify password (Argon2id, dummy-hash defense), then `identities::link`, then accept_login.

- [ ] **Step U4.2.2:** Create `crates/auth/src/ui/templates/link.html`.

- [ ] **Step U4.2.3:** Wire route, commit: `auth: /link — confirm account-link via password (collision path)`.

---

# Unit U5 · Third-party consent UI (real form)

Currently `/consent` only handles `skip_consent=true` (first-party). U5 replaces with the real form for third-party RPs.

## Task U5.1 · Rewrite `/consent` handler

- [ ] **Step U5.1.1:** In `crates/auth/src/ui/consent.rs`:
  - If `info.client.skip_consent`: silent accept (P2 path, unchanged).
  - Else: render the consent form:
    - RP name + logo
    - Visible redirect URI
    - Translated scope list (`openid` → "Verify your identity", `email` → "See your email address", `profile` → "See your name and profile picture", `offline_access` → "Stay signed in")
    - "Remember this choice" checkbox (default off for third-party)
    - Two buttons: Allow / Deny

- [ ] **Step U5.1.2:** On POST `/consent`:
  - If Allow: `hydra::accept_consent(challenge, { grant_scope, grant_access_token_audience, remember=checkbox, session })`
  - If Deny: `hydra::reject_consent(challenge, { error: "access_denied" })`

- [ ] **Step U5.1.3:** Honour `prompt=consent` (force the form even if already granted), `prompt=none` (no UI; return `interaction_required` to RP).

- [ ] **Step U5.1.4:** Create `crates/auth/src/ui/templates/consent.html`.

- [ ] **Step U5.1.5:** Commit: `auth: /consent — third-party consent UI (replaces skip-only)`.

---

# Unit U6 · `/me` profile page

A read-only page showing the logged-in user's email, name, and linked identities. Plus link/unlink controls.

## Task U6.1 · `/me` handler + template

- [ ] **Step U6.1.1:** Create `crates/auth/src/ui/me.rs`:
  - GET `/me` (requires `__Host-zsidp_session`): render profile page
  - POST `/me/link/<provider>`: start federation flow with a return-to-/me marker in the stash
  - POST `/me/unlink/<provider>`: unlink identity, refuse if it would leave the account credential-less (no password AND no other linked identities)

- [ ] **Step U6.1.2:** Create `crates/auth/src/ui/templates/me.html`.

- [ ] **Step U6.1.3:** Commit: `auth: /me — profile + linked-identities management (link/unlink)`.

---

# Unit U7 · Template polish

- [ ] **Step U7.1.1:** Update `login.html` to add "Sign in with Google" and "Sign in with GitHub" buttons (linking to `/oauth/google/start?login_challenge=...` and `/oauth/github/start?login_challenge=...`).

- [ ] **Step U7.1.2:** Update CSS to style the OAuth buttons (with provider color/icon — keep simple, SVG inline, no external dependencies).

- [ ] **Step U7.1.3:** Commit: `auth: ui — login page Sign-in-with-Google + Sign-in-with-GitHub buttons`.

---

# Unit U8 · Federation e2e tests

Tests run against a **mocked** Google + GitHub provider (a tiny in-process HTTP server) so they're CI-friendly. The mock returns canned `/token` and `/user{,/emails}` responses.

## Task U8.1 · Mock provider fixture

- [ ] **Step U8.1.1:** Add to `crates/auth/tests/common/mock_provider.rs`:

```rust
/// In-process mock OAuth/OIDC provider. Boots ntex server on a random port,
/// responds to /authorize, /token, /jwks, /user, /user/emails with canned data.
pub struct MockProvider {
    pub base: String,        // http://127.0.0.1:<port>
    pub mode: ProviderMode,  // Google or GitHub
    pub user: MockUser,      // the identity it returns
}
```

For Google: include a JWKS endpoint + sign a mock ID token with a generated key.
For GitHub: just /user + /user/emails endpoints.

- [ ] **Step U8.1.2:** Commit: `auth: tests — mock OAuth provider fixture (Google + GitHub modes)`.

## Task U8.2 · `e2e_google.rs`

- [ ] **Step U8.2.1:** Test: boot MockProvider in Google mode + crates/auth (with `AUTH_GOOGLE_*` env pointing at MockProvider). Drive `/oauth/google/start?login_challenge=...` → MockProvider's `/authorize` → MockProvider 302s back → `/oauth/google/callback` → verifies the mock ID token → `linker::resolve_or_link` creates a new user → accept_login → 302 to hydra continuation.

- [ ] **Step U8.2.2:** Assert: a user row exists in auth.users with the mock email; an identities row exists with (provider='google', subject=mock.sub).

- [ ] **Step U8.2.3:** Commit: `auth: e2e_google — federation flow (mock provider)`.

## Task U8.3 · `e2e_github.rs`

- [ ] **Step U8.3.1:** Same shape as Google but for GitHub. MockProvider returns `/user` with id+login+name, and `/user/emails` with one primary+verified email.

- [ ] **Step U8.3.2:** Bonus test: `/user/emails` with only unverified or noreply emails → callback aborts with the correct error.

- [ ] **Step U8.3.3:** Commit: `auth: e2e_github — federation flow (mock provider, verified-primary picker)`.

---

# Unit U9 · Phase 4 close-out

- [ ] **Step U9.1:** Run full suite. Expected counts:
  - zeroship-auth: 26 + ~5 (identities, link, linker, e2e_google, e2e_github) = ~31
  - Others unchanged

- [ ] **Step U9.2:** Clippy sweep — note count vs Phase 3 baseline.

- [ ] **Step U9.3:** Milestone empty-commit:
  ```
  auth: Phase 4 complete — Google + GitHub federation + third-party consent UI

  Federation + linking + /me. Phase 5 (magic-link, email verification,
  password reset, mailer) is next.
  ```

- [ ] **Step U9.4:** Tag `auth-phase-4`.

---

# Future phases

- **Phase 5** — Magic link, email verification, password reset; `Mailer` trait + lettre/resend/stdout drivers + bounce webhooks.
- **Phase 6** — JWK rotation cron; DPoP at gateway; audit-log retention sweeper; load test; security review; `docs/runbooks/auth-deploy.md`.

---

# Self-review

**Spec coverage** — proposal §8.2 (Google), §8.3 (GitHub), §8.5 (linking decisions), §10.3 (consent UI), §10.4 (skip-consent honour), §13 (threat model rows for federation: phishing-via-consent + redirect URI + state) all mapped to units.

**Placeholder scan** — no TBDs. Some code outlines (e.g., the `MockProvider` struct) are sketchy; the implementer fills in by patterning off existing `tests/common/`.

**Type consistency** — `ResolvedProfile`, `LinkOutcome`, `Identity`, `GoogleIdentity`, `GitHubIdentity` defined once each.

**Scope** — Phase 4 only ships federation + third-party consent + /me. No MFA, no magic-link, no email verification (those are Phase 5+).

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-4-federation.md`.

Execute via subagent-driven-development: one subagent per unit, U2/U3/U8 have ~2-3 sub-commits each.
