# Auth

`@zeroship/auth` is the platform's authentication contract for an app. It has
three parts:

- `@zeroship/auth` (the server helper) — read the current user inside a
  request handler.
- `@zeroship/auth/client` (the headless browser client) — sign a user in, hold
  the session, and sign them out.
- `@zeroship/auth/react` (the React adapter) — the same client as components
  and a hook.

Sign-in itself always runs in the platform's own sign-in UI. Your app never
handles a password or a social credential: it receives a signed-in session on
the browser side and a `User` object on the server side.

The identity your app receives is **per-app**. `id` is a pairwise subject that
differs for every app the same person signs in to, and `email` is a relay
address, so one person looks different to two of your apps and cannot be
correlated across them.

## Reading the user server-side

Import `auth` from `@zeroship/auth` and call it inside a handler:

```js
import { auth } from "@zeroship/auth";

const user = auth.getUser(); // User | null
if (!user) return new Response("sign in please", { status: 401 });
return Response.json({ hello: user.name ?? user.id });
```

Three helpers:

- `auth.getUser()` — the authenticated `User`, or `null` when the request is
  anonymous.
- `auth.requireUser()` — the same `User`, or a thrown error carrying
  `status: 401` and `code: "UNAUTHENTICATED"` when the request is anonymous.
- `auth.isLoggedIn()` — `true` when the request is authenticated, `false`
  otherwise.

`getUser()` never throws; `requireUser()` is the spelling that turns "anonymous"
into a `401` inside your handler. There is no server-side sign-out helper — sign
out from the browser with the client below.

### The User object

The server helpers return the platform's identity projection directly. On the
server the verified flag is spelled `email_verified` (snake_case); the browser
client renames that one field to `emailVerified`. The fields `auth.getUser()`
returns are:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `string` | Per-app pairwise subject (`pws_…`). Not the person's platform id; distinct for each app. |
| `email` | `string` | A per-app relay address, never the user's real inbox. Empty string when the `email` scope was not granted. |
| `email_verified` | `boolean` | Whether the identity's email is verified. |
| `name` | `string` | Display name, empty string when none is available. |
| `avatar` | `string \| null` | Avatar URL, or `null` when none is available. |
| `scopes` | `string[]` | The scopes granted to your app for this user (empty when none). |

`auth.getUser()` returns `email_verified`, not `emailVerified`. The camelCase
`emailVerified` exists only in the browser client (`@zeroship/auth/client`),
where the same projection is normalized; there `email` and `name` are also
typed nullable. This table is the server-side spelling.

These helpers only *read* identity. Whether a route requires a user at all is
declared separately, in the app's resource policy, and `auth: "user"` is the
fail-closed default: a procedure that declares no `auth` still requires a
signed-in user.

Declare the requirement per procedure in the wrapper config:

```js
import { query } from "@zeroship/rpc/server";
import { auth } from "@zeroship/auth";

export const me = query(
  async () => {
    const user = auth.getUser(); // User | null
    return { id: user?.id, name: user?.name };
  },
  { id: "account.me", auth: "user" },
);
```

`auth` accepts only `"user"` or `"anonymous"`. To make a procedure public,
declare `auth: "anonymous"` together with `publiclyAccessible: true` in the
app's resource policy (`src/server/config.ts`), under a resource key `rpc:<id>`
that names one procedure or a `.`-delimited family:

```js
import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    "rpc:wizard": { auth: "anonymous", publiclyAccessible: true },
  },
});
```

`publiclyAccessible: true` is the deliberate confirmation the build requires
with `auth: "anonymous"`, and it belongs to the resource policy, not the wrapper
config. `requireUser()` is how you fetch the user object, not the gate that
protects the route. See [RPC](rpc.md) for the full resource-tree grammar.

## Signing in a browser user

The browser client is a headless client (Auth0/Supabase-shaped) that drives the
platform's same-origin sign-in:

```ts
import { createAuthClient } from "@zeroship/auth/client";

const client = createAuthClient();

// Popup window (default): password, Google, or GitHub.
const { user } = await client.signInWithOAuth({ provider: "password" });
```

`signInWithOAuth(options?)` resolves to a `Session` — `{ user, expires_at,
scopes }` — on success and rejects with an `AuthError` on failure. The
`provider` option selects the sign-in surface:

- `"password"` — the platform's own password UI.
- `"google"`, `"github"` — a federated sign-in via that provider, when the
  platform has it configured.

By default sign-in opens a popup window. Pass `popup: false` to use a full-page
redirect instead; `redirectTo` sets where the redirect returns. The credential
never reaches your JavaScript: it is typed into the platform's own sign-in UI,
and your app only receives the completed session.

After sign-in the session lives in an `HttpOnly` session cookie set on your
app's own origin. That cookie rides automatically on every same-origin request
and is what authenticates your RPC and API calls to the browser. Your code
never holds a token to attach.

### Reading the session

| Method | What it does |
| --- | --- |
| `getSession()` | The in-memory `Session` snapshot, or `null`. Local only, no network. |
| `getUser()` | Server-validated `User \| null`; always re-probes the server. |
| `refreshSession()` | Re-mints the session cookie from the server and refreshes the snapshot. |
| `checkSession()` | Session recovery on page load — restores a signed-in session after a reload. |
| `isAuthenticated()` | `true` when a non-expired session is held; local, no network. |
| `hasScope(scope)` | `true` when the current session carries `scope`; local. |
| `onAuthStateChange(cb)` | Subscribes to state transitions; returns `{ unsubscribe() }`. |
| `signOut(options?)` | Ends the session. |
| `exchangeCodeForSession(code, state?)` | Completes a code exchange (used by `AuthProvider`; rarely called directly). |

`onAuthStateChange` delivers one of `SIGNED_IN`, `SIGNED_OUT`,
`SESSION_REFRESHED`, `USER_UPDATED`, or `RECOVERING`.

### Options and defaults

`createAuthClient(options?)` accepts:

- `appOrigin` — the app's own origin; defaults to the page's origin.
- `scope` — scopes to request on sign-in; defaults to
  `["openid", "profile", "email"]`.
- `refreshSkewSeconds` — seconds before `expires_at` to re-mint the session
  cookie early; defaults to `60`.

`signInWithOAuth(options?)` accepts:

- `provider` — `"password" | "google" | "github"`.
- `scopes` — override the default scope set for this sign-in.
- `popup` — popup (`true`, the default) vs full-page redirect (`false`).
- `redirectTo` — where to return after a redirect flow.
- `prompt` — `"login"` to force a fresh credential, or `"consent"` to re-show
  the consent screen.

Note the spellings: the client option is the singular `scope` (the default
scope set), while the per-sign-in override and the `Session` field are the
plural `scopes`.

`signOut(options?)` accepts `scope: "local" | "global"` (default `"local"`):
`"local"` ends the session on this device, `"global"` this app on every device.

## React

`@zeroship/auth/react` re-exports the client and layers components on top. Wrap
your app in `AuthProvider`, then read the reactive snapshot with `useAuth()`:

```tsx
import { AuthProvider, useAuth, SignInButton, SignOutButton } from "@zeroship/auth/react";

function App() {
  return (
    <AuthProvider>
      <Gate />
    </AuthProvider>
  );
}

function Gate() {
  const { user, isAuthenticated, isLoading } = useAuth();
  if (isLoading) return <p>…</p>;
  if (!isAuthenticated) return <SignInButton />;
  return (
    <>
      <p>hi {user.name ?? user.id}</p>
      <SignOutButton />
    </>
  );
}
```

`useAuth()` returns `{ user, session, isAuthenticated, isLoading, error,
signInWithOAuth, signOut, requestScopes, hasScope }`. `SignInButton` and
`SignOutButton` render ready buttons; `SignIn` is a zero-config sign-in
launcher; `SignedIn` / `SignedOut` render children only on the matching side of
the gate; `AuthModal` hosts the platform's password UI in a styled overlay;
`hasAuthParams(search?)` reports whether the URL is a sign-in redirect return.

## The session and its lifetime

A `Session` is identity only: `{ user, expires_at, scopes }`. `expires_at` is the
Unix-second instant the session cookie expires — about 15 minutes after sign-in.
The browser client re-mints the cookie from a server-held, 30-day anchor when it
lapses or on page load, so a user stays signed in across reloads without
re-entering their credential. The browser never sees the anchor or any token:
the only client-visible artifact is the `HttpOnly` session cookie, which your
JavaScript cannot read.

`signOut()` clears the session for the requested scope. It resolves even if the
server-side revoke is unavailable — the local sign-out is authoritative, so a
network failure never strands the user signed in.

## Scopes and consent

Scopes are the permissions your app asks the user to grant. The default request
is `openid`, `profile`, and `email`. `email` gates the `User.email` field: when
it was not granted, `email` is an empty string on the server (null/empty in the
browser client).

To ask for more later, call the client method
`requestScopes(scopes: string[]): Promise<Session>` — it re-opens the consent
screen for the union of the current and requested scopes and resolves to the
refreshed `Session`. In React, `useAuth()` exposes the same call bound to the
provider's client as `requestScopes(scopes: string[]): Promise<void>`. Consented
grants are remembered, so a scope already granted is not asked for again.

## Errors

The browser client rejects with `AuthError`, which extends `Error` and carries:

- `code` — one of the codes below.
- `status` — the HTTP status, when one was observed.
- `name` — always `"AuthError"`.

Branch on `error.code`:

| `code` | When |
| --- | --- |
| `login_required` | No session; the user must sign in. |
| `consent_required` | The user must consent to the requested scopes. |
| `interaction_required` | The authorization server needs interaction to proceed. |
| `invalid_grant` | A code or token exchange was rejected. |
| `invalid_credentials` | The password the user entered was rejected. |
| `invalid_request` | A request was malformed or missing a required field. |
| `missing_code_verifier` | A code arrived with no matching open sign-in flow. |
| `popup_closed` | The user closed the sign-in popup or dismissed the modal. |
| `popup_blocked` | The browser blocked the sign-in popup. |
| `timeout` | The sign-in flow timed out (60 seconds). |
| `scope_required` | The route requires scopes the caller has not granted (`403`). |
| `invalid_state` | The sign-in response's state did not match the request. |
| `network_error` | A request failed at the network level. |
| `server_error` | The server answered unexpectedly. |
| `config_error` | The client was misconfigured. |
| `client_not_provisioned` | Your app's sign-in is not provisioned yet (`503`); retryable. |

Server-side, `auth.requireUser()` throws a plain `Error` (no subclass; it is
not the browser client's `AuthError` — that type is browser-only) whose own
properties are `status: 401` and `code: "UNAUTHENTICATED"`. A handler detects
the failure by branching on `error.code === "UNAUTHENTICATED"` (or
`error.status === 401`); there is no server-side `instanceof` guard. It is the
same `code` an RPC `401` surfaces under [RPC](rpc.md).

## Non-browser clients

A browser authenticates through the session cookie described above. A
**non-browser** client authenticates requests to your app by presenting an
OAuth access token from the platform auth service as
`Authorization: Bearer <token>`. The RPC client attaches whatever its `auth`
resolver returns as that header (see [RPC](rpc.md)); where a non-browser
client obtains the token is the platform's own OAuth flow, not this SDK.

## See Also

- [RPC](rpc.md) — the route policy that decides which procedures require a
  user, and how RPC clients authenticate.