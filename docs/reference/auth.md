# @zeroship/auth — Platform Auth Service

## Overview

Authentication is a **platform service**, not an app feature. The platform manages users, passwords, OAuth, and consent. Apps never see tokens, never store passwords, never implement login. They call `auth.getUser()` and get back a user object — or `null`. JWT cookies are the session — no server-side session storage.

The model: Google OAuth for third parties. Users have one platform account. Each app they use gets a consent grant. The app receives a user profile, nothing more.

```
End user → Creator's app → "Sign in" → Platform auth page → Login + Consent → Cookie set → Redirect back
                                                                                              │
App server: auth.getUser() → { id, email, name, avatar }    ← Gateway decoded cookie, injected user
App client: auth.getUser() → { id, email, name, avatar }    ← Gateway injected window.__zs_user
```

## Principles

1. **The app is auth-unaware.** It doesn't know tokens exist. It doesn't handle login. It doesn't set cookies.
2. **The gateway is the guard.** JWT validation, cookie management, and login redirects happen at the network layer.
3. **Users live on the platform.** Not in the app's database. Apps reference users by `id`.
4. **Consent is per-app.** A user authenticates once with the platform, then grants each app access separately.
5. **No scopes (for now).** Every app gets `{ id, email, name, avatar }`. No granular permissions until the platform has features worth scoping.

## Architecture

```
Browser                     Gateway                     Worker (V8)
───────                     ───────                     ──────────
                                                        
GET /dashboard              Reads __zs_session cookie   
  Cookie: __zs_session=JWT  Validates JWT signature     
                            Checks app consent          
                            ├─ Valid:                    
                            │  Strips cookie             
                            │  Injects user into ctx ──→ zeroship.auth.getUser()
                            │                            → { id, email, name, avatar }
                            │
                            └─ Invalid / missing:
                               If server returns 401 ──→ Redirect to platform login
                                                         auth.zeroship.ai/authorize
                                                           ?app_id=xxx
                                                           &return=/dashboard
```

### Components

| Component | Where | Responsibility |
|---|---|---|
| **Auth service** | Control plane | User CRUD, password hashing (bcrypt), OAuth flows, JWT issuance, consent management |
| **Auth pages** | `auth.zeroship.ai` | Platform-hosted login, signup, consent UI. Not customizable by creators. |
| **Gateway middleware** | Gateway | Decode JWT from cookie, validate, inject user into V8 request context, handle 401→redirect |
| **Server SDK** | `@zeroship/auth` | `auth.getUser()`, `auth.requireUser()` — reads from V8 context |
| **Client SDK** | `@zeroship/auth/client` | `auth.getUser()` — reads `window.__zs_user` injected by gateway |

## Auth Flow

### First visit (not logged in)

```
1. User visits app.zeroship.ai/dashboard
2. Gateway: no __zs_session cookie
3. Worker runs: auth.requireUser() → throws 401
4. Gateway intercepts 401 → redirect to:
     auth.zeroship.ai/authorize?app_id=<uuid>&return=/dashboard
5. Platform auth page shows login form:
     ┌─────────────────────────────────┐
     │   Sign in to continue           │
     │                                 │
     │   [Email]                       │
     │   [Password]                    │
     │                                 │
     │   [Sign in]                     │
     │                                 │
     │   ── or ──                      │
     │                                 │
     │   [G] Continue with Google      │
     │   [→] Continue with GitHub      │
     │                                 │
     │   Don't have an account?        │
     │   [Sign up]                     │
     └─────────────────────────────────┘
6. User logs in (or signs up + logs in)
7. Platform checks: has user consented to this app?
     No → show consent screen:
     ┌─────────────────────────────────┐
     │                                 │
     │   [App Name] wants to           │
     │   access your account           │
     │                                 │
     │   • Your name and email         │
     │   • Your profile picture        │
     │                                 │
     │   [Allow]    [Deny]             │
     │                                 │
     └─────────────────────────────────┘
8. User clicks Allow
9. Platform records consent: (user_id, app_id, granted_at)
10. Platform issues JWT: { sub: user_id, app: app_id, email, name, avatar, exp }
11. Platform sets cookie: __zs_session=<JWT>; HttpOnly; Secure; SameSite=Lax; Domain=.zeroship.ai
12. Redirect back to: app.zeroship.ai/dashboard
13. Gateway reads cookie → valid → injects user → worker runs → auth.getUser() works
```

### Returning visit (logged in, already consented)

```
1. User visits app.zeroship.ai/dashboard
2. Gateway: reads __zs_session cookie → JWT valid, consent exists
3. Injects user into V8 context
4. Worker: auth.getUser() → { id, email, name, avatar }
5. No redirect, no popup, no interruption
```

### First visit to a different app (logged in, no consent for this app)

```
1. User visits other-app.zeroship.ai/settings
2. Gateway: reads __zs_session cookie → JWT valid, but no consent for this app
3. Redirect to: auth.zeroship.ai/authorize?app_id=<other-app>&return=/settings
4. Platform shows consent screen (skip login — already authenticated)
5. User clicks Allow
6. Platform issues new JWT scoped to this app
7. Redirect back to other-app.zeroship.ai/settings
```

## Platform Database Schema

Owned by the control plane. Apps cannot access these tables.

```sql
-- Platform users (one account per person, across all apps)
CREATE TABLE auth.users (
    id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    email       TEXT UNIQUE NOT NULL,
    name        TEXT NOT NULL,
    avatar_url  TEXT,
    password_hash TEXT,                   -- bcrypt, null if OAuth-only
    email_verified BOOLEAN DEFAULT FALSE,
    created_at  TIMESTAMPTZ DEFAULT NOW(),
    updated_at  TIMESTAMPTZ DEFAULT NOW(),
    last_login  TIMESTAMPTZ
);

-- OAuth provider links (one user can have multiple providers)
CREATE TABLE auth.oauth_links (
    id              SERIAL PRIMARY KEY,
    user_id         UUID NOT NULL REFERENCES auth.users(id),
    provider        TEXT NOT NULL,        -- "google", "github", etc.
    provider_user_id TEXT NOT NULL,
    access_token    TEXT,                 -- encrypted, for API calls on behalf
    refresh_token   TEXT,                 -- encrypted
    UNIQUE (provider, provider_user_id)
);

-- Per-app consent grants
CREATE TABLE auth.app_consents (
    id          SERIAL PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES auth.users(id),
    app_id      UUID NOT NULL,
    granted_at  TIMESTAMPTZ DEFAULT NOW(),
    revoked_at  TIMESTAMPTZ,             -- null = active
    UNIQUE (user_id, app_id)
);

-- No sessions table — JWT is the session. Validated by gateway locally (HMAC, ~2μs).
-- Token lifetime: 24h. No server-side session storage needed.
```

## JWT Structure

```json
{
  "sub": "d0f3a7c2-8b1e-4f5a-9c6d-2e3f4a5b6c7d",  // user UUID (global across apps)
  "app": "a1b2c3d4-...",                             // app UUID
  "email": "alice@example.com",
  "name": "Alice Smith",
  "avatar": "https://...",
  "iat": 1714000000,
  "exp": 1714086400                                   // 24h default
}
```

- Signed with HS256 (platform-internal, shared secret between auth service and gateway)
- App never sees the JWT — only the gateway decodes it
- Short-lived (24h), no server-side session — JWT is the session

## Native Primitives

Registered by the Rust runtime on every V8 isolate:

```ts
// zeroship.auth.* — injected into the V8 context by the gateway
interface ZeroshipAuth {
    /** Returns the authenticated user, or null if not authenticated. */
    getUser(): ZeroshipAuthUser | null;
    
    /** Returns the authenticated user, or throws a 401 error. */
    requireUser(): ZeroshipAuthUser;
}

interface ZeroshipAuthUser {
    id: number;
    email: string;
    name: string;
    avatar: string | null;
}
```

These are **synchronous** — the gateway already validated the JWT and injected the user before the worker code runs. No async, no native calls, no network.

## SDK API

### Server: `@zeroship/auth`

```ts
"use server";

import { auth } from "@zeroship/auth";

// Get current user — returns null if not authenticated
const user = auth.getUser();
// user: { id: number, email: string, name: string, avatar: string | null } | null

// Require authentication — throws 401 if not authenticated
const user = auth.requireUser();
// user: { id: number, email: string, name: string, avatar: string | null }
// Throws: Error with status 401 (gateway intercepts and redirects to login)
```

### Client: `@zeroship/auth/client`

```ts
import { auth } from "@zeroship/auth/client";

// Read platform-injected user — synchronous
const user = auth.getUser();
// user: { id: number, email: string, name: string, avatar: string | null } | null

// Check if authenticated
if (auth.isLoggedIn()) { ... }

// Sign out — redirects to platform sign-out page, clears cookie
auth.signOut();
// → redirect to auth.zeroship.ai/logout?app_id=xxx&return=/
```

The gateway injects into every HTML response:

```html
<script>window.__zs_user = {"id":42,"email":"alice@example.com","name":"Alice","avatar":null};</script>
```

If not authenticated, `window.__zs_user` is `null`.

### TypeScript Types

```ts
// @zeroship/auth
export interface User {
    id: string;  // UUID
    email: string;
    name: string;
    avatar: string | null;
}

export const auth: {
    getUser(): User | null;
    requireUser(): User;
};

// @zeroship/auth/client
export const auth: {
    getUser(): User | null;
    isLoggedIn(): boolean;
    signOut(): void;
};
```

## Gateway Behavior

### On every request:

```
1. Read __zs_session cookie
2. If missing → set request.user = null, continue
3. If present → validate JWT signature + expiry
4. If invalid/expired → clear cookie, set request.user = null, continue
5. If valid → check app_consents table (cached in memory, TTL 5min)
6. If no consent → set request.user = null, continue
7. If consent valid → inject user into V8 request context
```

### On 401 response from worker:

```
1. Worker returned HTTP 401
2. Gateway intercepts (does not forward to browser)
3. Redirect to: auth.zeroship.ai/authorize?app_id=<id>&return=<original_path>
```

### On HTML response (for client-side user injection):

```
1. Worker returned HTML (Content-Type: text/html)
2. Gateway injects before </head>:
     <script>window.__zs_user = ${JSON.stringify(user)};</script>
3. Forward to browser
```

## OAuth Flow (Google example)

```
1. User clicks "Continue with Google" on platform auth page
2. Platform redirects to:
     accounts.google.com/o/oauth2/v2/auth
       ?client_id=<platform_google_client_id>
       &redirect_uri=auth.zeroship.ai/callback/google
       &scope=email+profile
       &state=<encrypted: app_id, return_url>
3. User authenticates with Google, grants permission
4. Google redirects to: auth.zeroship.ai/callback/google?code=xxx&state=yyy
5. Platform exchanges code for tokens
6. Platform reads Google profile (email, name, picture)
7. Platform finds or creates user in auth.users
8. Platform creates oauth_link if new
9. Platform checks consent for the app → show consent or skip
10. Platform issues JWT, sets cookie, redirects back to app
```

## Creator Dashboard

Creators configure auth for their app in the control plane dashboard:

- **App name** — shown on consent screen
- **App icon** — shown on consent screen
- **Allowed origins** — CORS + cookie domain
- **Auth providers** — toggle email/password, Google, GitHub
- **OAuth credentials** — not needed; platform uses its own Google/GitHub app

Creators don't configure JWT secrets, session duration, or cookie settings. The platform controls all security parameters.

## What the app CANNOT do

- See or decode JWTs
- Set or read auth cookies
- Store passwords
- Manage user accounts
- Implement login/signup flows
- Access other apps' users
- Bypass consent

## What the app CAN do

- Read the current user: `auth.getUser()`
- Require authentication: `auth.requireUser()` (triggers login redirect via 401)
- Reference users in its own data: `db.profiles.insert({ userId: user.id, ... })`
- Sign the user out: `auth.signOut()` (client-side redirect)

## Implementation Order

### Phase 1: Core (MVP)

1. **Auth service** in control plane — users table, bcrypt password hashing, JWT issuance
2. **Login/signup page** at `auth.zeroship.ai` — email/password only
3. **Consent screen** — simple "Allow / Deny"
4. **Gateway middleware** — JWT validation, cookie handling, 401 redirect, `__zs_user` injection
5. **Server SDK** — `auth.getUser()`, `auth.requireUser()` (reads V8 context)
6. **Client SDK** — `auth.getUser()` (reads `window.__zs_user`), `auth.signOut()`

### Phase 2: OAuth

7. **Google OAuth** — accounts.google.com integration
8. **GitHub OAuth** — github.com integration
9. Provider selection on login page

### Phase 3: Enhancements

10. **Magic link** — passwordless email login
11. **Email verification** — verify email after signup
12. **User profile editing** — change name, avatar
13. **Consent management** — user can view and revoke app consents
14. **Token invalidation** — optional `token_invalidated_at` timestamp for emergency revocation
15. **Scopes** — granular permissions when the platform has features worth scoping
