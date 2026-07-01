# P5a — the gateway OP client model for end-user login (design note, v2)

**Status:** design-only (draft in worktree, uncommitted per proposal-workflow). v2 is a
ground-up rewrite after the v1 critique (`.p5a-client-model-critique.md`, 33/100, CRITICAL
exploitable pairwise/PII break). v1's central mechanism — a single shared `"gateway"` OP
broker client with an apex-suffix redirect and a global-subject id_token — is **withdrawn**:
it composed into a global-identity + PII harvest by any deployed creator app. This note
re-derives the client model from the two gateway login flows and the shipped BFF session.

**Context:** P5a re-homes the gateway's OIDC-RP verify + code-exchange arms off Hydra onto
the platform OP (`crates/auth/src/oidc/`). The question this note answers is: *which OP
client(s) does end-user login use, and how does that preserve pairwise isolation (OIDC
§8.1), redirect exact-match (RFC 9700), per-app consent, and the shipped 30-day durable
session?*

---

## What changed from v1, and why (critique traceability)

| v1 finding | v1 said | v2 resolution |
|---|---|---|
| **CRITICAL-1** (shared broker client + apex redirect + global-sub id_token + ignored secret = PII/identity harvest) | One `"gateway"` client for all apps; apex-suffix redirect; global sub in id_token; secret unchecked on code grant. | **Withdrawn.** End-user login uses the **per-app `oac_` clients** (§D1). They get per-app **exact-match** redirects (§D5), per-app **consent** (§D6), and their code grant is gated by a **confidential broker secret the OP now enforces** (§D3). App JS can start an authorize but can never *exchange the code* — only the gateway can — so the global-sub id_token never reaches app code. The attack chain is broken at the code-exchange step, not merely obscured. |
| **CRITICAL-2** (D1 scoped to the wrong client; breaks the per-app SPA/popup login, which needs the global sub for re-projection + control-parity) | Global sub only for `first_party`/`broker` clients; per-app `oac_` left `'app'` (OP pairwise) → gateway can't recover `global_user_id`. | **The load-bearing fact is now stated first (§D1):** end-user login *is* the per-app `oac_` flow. Those clients become **globally-subjected** (§D4) so both gateway flows recover `global_user_id` for the anchor / `app_user_identities` / relay / `pws_` re-projection — byte-identical to control (verified: `Issuer::pairwise_subject` and the gateway both call `zeroship_core::auth::derive_pairwise`). |
| **CRITICAL-3** (broker model destroys per-app consent, contradicts §5.2) | Never mentioned consent. | Consent stays keyed on the **per-app** `oac_` `client_id` (§D6). `skip_consent`/`trusted_oauth_clients` (the existing console axis) is untouched and kept **orthogonal** to the new `brokered` flag — no duplication (fixes MAJOR-2). |
| **MAJOR-1 / M1** (D5 drops `offline_access`; contradicts the shipped `refresh_token_enc` anchor) | Gateway PKCE-only, `refresh_allowed=FALSE`, drop `offline_access`. | **Withdrawn (§D7).** The 30-day session *is* the OP refresh token (`anchors.rs:166`, `lib.rs:185`, `auth_token.rs:481`). `oac_` clients keep `offline_access` and get `refresh_allowed=TRUE`. |
| **MAJOR-4 / M4** (confidential client not authenticated on the code grant — RFC 6749 §4.1.3) | Treated the missing check as benign. | **Fixed (§D3):** the authz_code grant now authenticates `brokered` clients using the same `authenticate_for_refresh`/`verify_client_secret` machinery the refresh grant already runs (`refresh.rs:827-866`). This is the control that makes §D4 exploit-free. |
| **MAJOR-5 / M5** (D2 relaxes redirect exact-match — RFC 9700) | Apex-suffix or registry-origin redirect for the broker. | **Withdrawn (§D5).** Per-app `oac_` clients keep the OP's existing **exact string match** (`authorization_code.rs:226`), which mirrors the gateway's own `is_registered_redirect_uri` hard rule (`browser_auth.rs:169`). No wildcard, suffix, or origin match anywhere. |
| **MAJOR-2 / M2** (`client_profile` enum duplicates `skip_consent`) | New `client_profile {app\|first_party\|broker}`. | **Withdrawn (§D9).** Replaced by **one** boolean `brokered` on `oauth_clients`, orthogonal to `skip_consent`. Global-sub + confidential-broker-auth are the *same* property (they always travel together for gateway-brokered login), so one flag captures both. |
| **MAJOR-3 / M3** ("control/builder → first_party is a no-op" is false) | Flip control/builder to `first_party`. | **Dropped.** Control/builder/device are **left untouched** (`brokered=FALSE`, pairwise-on-client-id, unchanged). The principal/global path stays wired to the device flow only (`device_token.rs:78`), as today. No unverified subject change. |
| **MINOR-1 / m1** (SHA-256 secret needs an entropy mandate) | Asserted "high-entropy". | **Mandated (§D8):** the broker secret is a generated ≥256-bit value, rejected at boot if short or the dev sentinel — mirroring `validate_pairwise_salt`. |
| **MINOR-2/3 / m2,m3** (bootstrap ordering + rotation flap; secret duplicated) | Advisory-lock upsert of a shared client. | **Reframed (§D8):** no shared client to bootstrap. The broker secret is a **platform config secret with a current+previous window** verified for `brokered` clients, so rotation is config-only (no per-client re-hash, no flap). Start-before-provision is already handled by the shipped `client_not_provisioned` 503 (`auth_token.rs:112`). |
| **MINOR-4 / m4** (`hydra_client_id='gateway'` placeholder) | Written by a native path. | **N/A** — no shared gateway client is created. `oac_` rows already carry `hydra_client_id = oac_…` (inert once the OP is the AS; P5f drops the column). |
| **Missing #3** (RFC 9207 `iss` on the RP callback) | Absent. | **Added (§D10).** |
| **Missing #5** (broker token `aud`) | Absent. | **Resolved (§D10):** `oac_` clients have an `app_id`, so their tokens carry `aud = app:{app_id}` (`authorization_code.rs:88`), never the control-replayable `"zeroship"`. |
| **Missing #6** (`nonce` survival) | Absent. | **Confirmed (§D10):** flow-1 nonce validation survives; flow-2 relies on code+PKCE+`at_hash`/`c_hash` binding, as it does today. |
| **Missing #7** (blast radius of a global-sub token) | Equated to control's boundary. | **Addressed (§D4):** the global sub is confined to the gateway's server-side hold and is never obtainable by an app (confidential exchange), so the internet-facing surface never carries it outward. |

---

## Verified facts (kept from the investigation)

These were checked against the code and are the foundation of v2.

1. **There are TWO gateway login flows, both per-app, both re-homed by P5a.**
   - **Flow 1 — interactive cookie/stash.** `OidcRp::build_authorize_redirect` →
     `finish_callback` (`oidc_rp.rs:140`, `:203`). Server-side PKCE verifier/state/nonce in
     the signed `__Host-zs_oidc_stash` cookie; redirect
     `{app-host}/__zeroship/auth/callback`. **Today it uses the shared `self.client_id =
     "gateway"` + `self.client_secret`** (`oidc_rp.rs:167`, `:225`; constructed at
     `main.rs:677`).
   - **Flow 2 — SPA/popup (Supabase-style, the primary creator-app login).**
     `build_browser_authorize_url` → `exchange_code_public` (`oidc_rp.rs:459`, `:399`),
     driven by `POST /__zeroship/auth/session` (`auth_token.rs:307` → `mint_session_from_code`
     `:399`). The browser holds the PKCE verifier; the gateway injects the **per-app**
     `route.client_id` (`oac_…`, `auth_token.rs:112,417`) and today sends **no secret**
     (public client).
2. **Both flows need the GLOBAL user id.** Flow 2 parses `claims.sub` as a `Uuid`
   (`auth_token.rs:467`) and uses that `global_user_id` for: the pairwise projection
   (`pairwise_sub` → `derive_pairwise`, `auth_token.rs:132`), the encrypted anchor
   (`NewAnchor.global_user_id`, `anchors.rs`), the relay-alias lookup
   (`relay_alias_for`, `auth_token.rs:155`), the `gateway_sessions.user_id` audit row, and
   the `app_user_identities` mapping (`identities::upsert`, `auth_token.rs:619`). Flow 1
   likewise re-projects. Under Hydra both flows received the **global** sub (Hydra
   `subject_type=public`) and the gateway did its own `pws_` projection.
3. **The gateway's `pws_` projection is byte-identical to the OP's pairwise subject.**
   Gateway: `derive_pairwise(pairwise_salt, global_user_id, sector)` (`main.rs:674`,
   `auth_token.rs:134`). OP: `Issuer::pairwise_subject` → *the same*
   `zeroship_core::auth::derive_pairwise(&self.pairwise_salt, user, sector)`
   (`issuer.rs:414`). With the shared `PAIRWISE_SALT` and the app's `sector_identifier`,
   gateway, control, and OP all produce the same `pws_…` (the `main.rs:665-675` invariant).
4. **The 30-day durable session IS an OP refresh token.** It is stored AES-256-GCM-encrypted
   in `app_session_anchors.refresh_token_enc` (`anchors.rs:166`, `lib.rs:185`); login
   requests `scope=…offline_access…` (`oidc_rp.rs:169`) and the mint expects a
   `refresh_token` (`auth_token.rs:481`).
5. **The OP's client-authentication gaps (as shipped).**
   - `exchange_authorization_code` checks PKCE + code binding + consent but **never
     authenticates the client** — no `verify_client_secret` call (`authorization_code.rs:404-457`).
   - The **refresh** grant *does* authenticate per `token_endpoint_auth_method`
     (`refresh.rs:827-866`) — the machinery exists and is reusable.
   - `issue_id_token` always mints a **pairwise** sub on `client.sector_identifier`
     (`issuer.rs:344`); the **global-principal** path (`issue_principal_access_token`,
     `issuer.rs:272`) is wired to the **device** flow only (`device_token.rs:78`), not
     authorization_code.
   - Redirect validation is **exact string match** (`authorization_code.rs:226`); `iss` is
     added to the auth response per RFC 9207 (`authorization_code.rs:771`).
   - `oauth_clients` today has `skip_consent` (V0004), plus `client_secret_hash`,
     `refresh_allowed`, `token_endpoint_auth_method ∈ {none, client_secret_basic,
     client_secret_post}` (V0064). Per-app `oac_` clients are provisioned as **public
     PKCE** (`token_endpoint_auth_method='none'`, `refresh_allowed=FALSE`, no secret) in
     `app_oauth_client.rs::upsert_db_rows` (`:709`).

---

## The corrected architecture, in one paragraph

**End-user login goes through the per-app `oac_` clients — there is no shared "gateway"
client.** Each `oac_` client is a **gateway-brokered confidential client** (new flag
`brokered=TRUE`): its authorize is per-app (exact-match redirect, per-app consent, per-app
`aud=app:{id}`), but its *code exchange and refresh are authenticated by a platform broker
secret that only the gateway holds and the OP now verifies*. Because the exchange is
confidential, the OP can safely put the **global principal subject** in the `oac_` client's
id_token: only the gateway can obtain that token, and the gateway never forwards it — it
re-projects to the per-app `pws_…` exactly as it does today and returns only that to app
code. Pairwise isolation, redirect exact-match, per-app consent, and the 30-day refresh
anchor are all preserved because we kept the per-app client boundary and merely (a) made the
gateway authenticate as itself on the exchange, and (b) taught the OP to enforce that.

Two coupled OP changes make this safe; **neither is safe alone**:
- **Global subject** for `brokered` clients (so the gateway recovers `global_user_id`).
- **Enforced confidential authentication** on the code + refresh grants for `brokered`
  clients (so *only* the gateway can perform the exchange that yields that subject).

---

## Decisions

### D1 — End-user login uses the per-app `oac_` clients; the shared `"gateway"` client is eliminated
<!-- v2: addresses CRITICAL-1, CRITICAL-2, Missing#1 -->
Both gateway flows authorize and exchange against **`route.client_id` (`oac_<app>`)**, never a
shared `"gateway"` client. Flow 2 already does this; **Flow 1 is changed to do the same** —
`build_authorize_redirect`/`finish_callback` take the per-app `client_id` (resolved from the
route, exactly as `resolve_route` already yields it for `/session`) instead of the hard-coded
`"gateway"`. The `OidcRp` struct drops its single `client_id`/`client_secret` and instead
holds the **broker secret** (see D8), presenting it on every exchange with the per-request
`oac_` `client_id`.

Consequence: `main.rs:677`'s `OidcRp::new(&auth_ui_url, "gateway", oidc_client_secret, …)`
becomes `OidcRp::new(&auth_ui_url, broker_secret, …)`; there is no `"gateway"` OP client to
register at all. This is the single load-bearing fact v1 never stated and every downstream
decision depends on it.

### D2 — Each `oac_` client is a *gateway-brokered confidential client* (`brokered=TRUE`)
<!-- v2: addresses CRITICAL-1, MAJOR-4 -->
`brokered=TRUE` on an `oauth_clients` row means two things, together:
1. **Confidential exchange.** The code grant and refresh grant require the platform broker
   secret (D3, D8); a bare public exchange is rejected.
2. **Global subject.** The id_token (and its paired access token) carries the **global
   principal subject**, not a per-sector pairwise sub (D4).

These two always travel together for gateway-brokered login, so they are one flag rather than
two. `brokered` is **orthogonal to `skip_consent`** (D6) and to `refresh_allowed` (D7). All
`oac_` clients — every creator app *and* the first-party console-as-app — are `brokered=TRUE`.
Direct first-party API clients (control, builder) and the device flow are **`brokered=FALSE`**
and unchanged (D9).

Why a confidential client for a *browser* flow is not a contradiction: the browser is **not**
the OAuth client — the **gateway** is. The browser only holds the PKCE verifier and posts
`{code, verifier}` to the gateway's `/session`; the gateway performs the token exchange. Adding
the broker secret to that exchange is standard "confidential client + PKCE" defense-in-depth and
leaves the browser-facing flow unchanged.

### D3 — Enforce client authentication on the authorization_code grant (RFC 6749 §4.1.3)
<!-- v2: addresses CRITICAL-1, MAJOR-4, CRITICAL-2 -->
`exchange_authorization_code` gains the same client-authentication step the refresh grant
already runs. Reuse `refresh::client_auth_from_request` (already parsed in `token_post` and
passed into `token_inner`, currently ignored by the authz_code arm) and a shared
`authenticate_client(client, client_auth)` generalised from `authenticate_for_refresh`
(`refresh.rs:827`):
- **`brokered=TRUE`** → require the broker secret via `client_secret_basic` (basic-auth
  username = the `oac_` `client_id`, password = the broker secret), verified against the
  configured current/previous broker secret set (D8), constant-time. A `none`/public exchange,
  or a wrong secret, → `invalid_client`.
- **`brokered=FALSE`, confidential** (`client_secret_basic`/`post`) → verify the per-client
  `client_secret_hash` (existing `verify_client_secret`).
- **`brokered=FALSE`, public** (`none`) → PKCE-only, unchanged.

This is the control that makes D4 exploit-free: app JS can run its own `authorize` and even
capture a code redirected to its own origin, but it cannot present the broker secret, so it can
never exchange the code — the global-sub id_token never leaves the OP to anyone but the gateway.

### D4 — Global-identity recovery: `brokered` clients get the global principal subject; the gateway re-projects to `pws_`
<!-- v2: addresses CRITICAL-2, decision-3, Missing#7 -->
For a `brokered` client, `exchange_authorization_code` mints the id_token (and paired access
token) with the **global principal subject** (the user's global `usr_…` UUID string) via the
`issue_principal_*` path, instead of `pairwise_subject(user, sector)`. Gate strictly on
`client.brokered` read in `load_client`.

The gateway then does its **existing, unchanged** per-app projection:
`pws = derive_pairwise(pairwise_salt, global_user_id, route.sector_identifier)` — and returns
only `pws_` to app code (the id_token / global sub is held server-side and never forwarded).
Because `Issuer::pairwise_subject`, the gateway, and control all call the same `derive_pairwise`
with the same salt + sector, the projection is byte-identical across all three — control-parity
revocation (`app_user_identities`, family markers) and the anchor / relay writes (which key on
`global_user_id`) all keep working exactly as today.

**Why this is exploit-free (the v1 break, now closed):** the only holder of the global-sub
id_token is the gateway, because obtaining it requires the D3 confidential exchange. A malicious
creator app at `evil.zeroship.ai`:
- *cannot* phish another app's code — the redirect must exact-match the *other* app's registered
  callback host (D5), which it does not control;
- *cannot* exchange even its own `oac_evilapp` code — it lacks the broker secret (D3);
- therefore *never* receives an id_token carrying the global sub or the raw email. It sees only
  the gateway's response `{ user: pws_(user, evilapp_sector), email: relay-alias }` — exactly
  what it is entitled to, with zero cross-app correlation.

The global sub is thus confined to a single trusted, server-side hold on the gateway. Unlike v1
(which put a global-sub token behind an app-controllable redirect + ignored secret), there is no
path for an internet-facing party to obtain it.

**Access-token subject consistency:** for a `brokered` client the paired access token follows
the same subject (global), for OIDC id/access `sub` consistency. It is gateway-internal (never
handed to the browser or app; the app receives `pws_` via the `ZeroShip-User` header) and its
`aud` is `app:{app_id}` (D10), so it is not control-replayable.

### D5 — Redirect validation stays exact-match, per-app (RFC 9700)
<!-- v2: addresses MAJOR-5, withdraws v1-D2 -->
No change to the OP's redirect check: **exact string match** against the `oac_` client's
registered `redirect_uris` (`authorization_code.rs:226`). Those are the app's own
`{host}/__zeroship/auth/{popup-callback,callback}` set, registered per host by control
(`app_oauth_client.rs::redirect_uris_for_hosts`), and they mirror the gateway's own hardened
`is_registered_redirect_uri` (`browser_auth.rs:169`, "NOT a prefix/origin check"). v1's
apex-suffix / route-registry relaxation is **withdrawn entirely** — using per-app clients means
there is nothing to relax; a shared client was the only reason v1 needed to.

### D6 — Consent stays per-app; `brokered` is orthogonal to `skip_consent`
<!-- v2: addresses CRITICAL-3, MAJOR-2 -->
Consent is evaluated on the **per-app** `oac_` `client_id` (`consent_covers`,
`authorization_code.rs:257,455`), so §5.2 holds: creator apps run the first-grant consent
prompt (`skip_consent=FALSE`), then remembered consent serves subsequent logins silently. The
console keeps `skip_consent=TRUE` via the existing `[auth].trusted_oauth_clients` overlay
(`trusted_clients.rs`) — **unchanged**. The new `brokered` flag governs *authentication +
subject* only and does **not** touch consent; there is no `client_profile` enum and no second
source of truth for "first-party". Orthogonality table:

| client | `brokered` | `skip_consent` | subject |
|---|---|---|---|
| creator app (`oac_…`) | TRUE | FALSE | global (gateway re-projects → `pws_`) |
| first-party console (`oac_console`) | TRUE | TRUE | global (gateway re-projects → `pws_`) |
| control / builder / device | FALSE | (per existing) | pairwise-on-client-id (unchanged) |

### D7 — Keep `offline_access` + `refresh_allowed=TRUE` for `oac_` clients (the durable session)
<!-- v2: addresses MAJOR-1 -->
The 30-day session is the OP refresh token in `refresh_token_enc`, so v1's "PKCE-only, drop
`offline_access`" is withdrawn. `oac_` clients keep `offline_access` in their scope allowlist and
are provisioned with **`refresh_allowed=TRUE`** (the OP gates refresh issuance on
`client.refresh_allowed`, `authorization_code.rs:526`; today `app_oauth_client.rs:709` writes
`FALSE`, which P5a flips to `TRUE`). No anchor rework is needed — the anchor stores exactly the
OP refresh token the gateway already expects (`auth_token.rs:481`), and `?mint=1` rotation
(`refresh.rs` confidential auth) presents the broker secret like the code exchange (D3).

### D8 — The broker secret: a platform config secret with a rotation window
<!-- v2: addresses MINOR-1, MINOR-2, MINOR-3, MAJOR-2 -->
Rather than a per-client secret hash (which would force a re-hash of every `oac_` row on
rotation and flap during a rolling deploy), the broker secret is a **platform confidential
secret** the OP verifies for any `brokered` client:
- Config: `AUTH_GATEWAY_BROKER_SECRET` (current) + optional `AUTH_GATEWAY_BROKER_SECRET_PREVIOUS`
  on the **auth** service; the same value(s) as `GATEWAY_BROKER_SECRET` on the **gateway**. The
  OP verifies a presented secret against `{current, previous}` (constant-time). No
  `client_secret_hash` is stored on `oac_` rows for the broker path.
- **Entropy mandate (m1):** the secret is a generated **≥256-bit** value; boot rejects a short
  value or the dev sentinel, mirroring `validate_pairwise_salt` (`secrets.rs`).
- **Rotation (m2/m3):** set `PREVIOUS = old`, `current = new` on both services, roll, then drop
  `PREVIOUS`. No per-client re-hash, no flap; the two-secret window spans the rolling deploy.
- **Bootstrap ordering (m2):** there is no shared client to bootstrap. A login that arrives
  before control has provisioned an app's `oac_` client already returns the shipped
  `client_not_provisioned` 503 (`auth_token.rs:112`), which the SDK treats as retryable. Client
  provisioning is unchanged except that `app_oauth_client.rs` now writes `brokered=TRUE`,
  `refresh_allowed=TRUE`, and `token_endpoint_auth_method='client_secret_basic'` for `oac_`
  rows (the broker path verifies the platform secret, not a per-row hash).

Blast radius: the broker secret lives only in the two trusted first-party runtime services
(gateway presents, auth verifies) — the same trust tier as the pairwise salt, the anchor
encryption key, and the worker key. Control does **not** need it (it no longer writes a per-app
secret hash for the broker path). App code never sees it.

### D9 — Client-model representation: one boolean `brokered`, control/builder untouched
<!-- v2: addresses MAJOR-2, MAJOR-3 -->
Add `brokered BOOLEAN NOT NULL DEFAULT FALSE` to `zeroship.oauth_clients` (a V0064-style
`ALTER`). `load_client` selects it; the authz_code exchange branches on it (D3, D4). No
`client_profile` enum, no reuse/overload of `skip_consent`. **Control, builder, and the device
flow are not modified** — they stay `brokered=FALSE` and keep pairwise-on-client-id, so P5a makes
**no** change to their id_token `sub` (fixes M3's false "no-op" claim by simply not making the
change). The principal/global path remains wired to the device flow only, plus the new
`brokered` branch of the authz_code exchange.

### D10 — RFC 9207 `iss`, `aud`, and `nonce` across the re-home
<!-- v2: addresses Missing#3, Missing#5, Missing#6 -->
- **RFC 9207 `iss` (Missing#3):** the OP already appends `iss` to the auth response
  (`authorization_code.rs:771`). P5a-3 adds callback-side validation: `finish_callback`
  (flow 1) and the SPA relay (flow 2) reject a callback whose `iss` ≠ the expected OP issuer.
  This mix-up defense matters *more* here given the sibling pluggable-IdP project introduces
  multiple upstream IdPs.
- **`aud` (Missing#5):** `oac_` clients carry an `app_id`, so `resource_audience()` yields
  `aud = app:{app_id}` (`authorization_code.rs:88`), never the control-replayable `"zeroship"`
  (which only a client with *no* `app_id`, like the withdrawn shared `"gateway"`, would have
  gotten). id_token `aud` = the `oac_` `client_id`.
- **`nonce` (Missing#6):** the OP requires `nonce` for `openid` and binds it in the id_token
  (`authorization_code.rs:249`, `issuer.rs:348`). Flow 1 stashes and validates it
  (`oidc_rp.rs:278`, `Some(&stash.nonce)`) — survives the re-home. Flow 2 passes `None`
  (`auth_token.rs:447`) and relies on the code binding + PKCE + `at_hash`/`c_hash` to bind the
  id_token to the exact exchange — unchanged shipped behavior.

---

## How the two flows map onto the per-app `oac_` client

**Flow 1 (interactive cookie/stash), re-homed:**
1. Unauthenticated HTML hit on `{app}.zeroship.ai`; gateway resolves `route.client_id`
   (`oac_app`) and `route.sector_identifier`.
2. 302 → OP `/authorize?client_id=oac_app&redirect_uri={app-host}/__zeroship/auth/callback&…`
   (server-side PKCE verifier/state/nonce in the stash cookie).
3. OP: exact-match redirect (D5), per-app consent (D6), issues code with `iss` (D10).
4. `/callback`: gateway exchanges at OP `/token` with `client_id=oac_app`, the PKCE verifier,
   **basic-auth `(oac_app, broker_secret)`** (D3). OP verifies the broker secret, mints the
   id_token with the **global** sub (D4), `aud=oac_app`.
5. Gateway verifies id_token (sig/iss/aud/exp/nonce), recovers `global_user_id`, derives `pws_`,
   sets the signed session cookie, forwards only `pws_` identity onward.

**Flow 2 (SPA/popup), re-homed:**
1. SDK computes its own PKCE challenge; gateway assembles
   `/authorize?client_id=oac_app&redirect_uri={app-host}/__zeroship/auth/popup-callback&…`.
2. OP: exact-match redirect, per-app consent, issues code with `iss`.
3. Browser posts `{code, verifier}` to `POST /__zeroship/auth/session`; gateway exchanges at OP
   `/token` with `client_id=oac_app` + verifier + **basic-auth `(oac_app, broker_secret)`**.
4. OP verifies broker secret, mints global-sub id_token + `offline_access` refresh token (D7),
   `aud=oac_app`.
5. Gateway verifies id_token, recovers `global_user_id`, encrypts the refresh token into the
   anchor, writes `gateway_sessions` + `app_user_identities`, derives `pws_`, relay-swaps the
   email, returns only `{ user: pws_, email: relay-alias }`.

The **only** new step in either flow is the gateway presenting the broker secret on the exchange,
and the OP verifying it + minting the global sub for `brokered` clients. Everything else is the
shipped path.

---

## Scope of P5a implementation after this note

- **P5a-1 (schema + provisioning):** add `oauth_clients.brokered` (default FALSE); change
  `app_oauth_client.rs::upsert_db_rows` to write `brokered=TRUE`, `refresh_allowed=TRUE`,
  `token_endpoint_auth_method='client_secret_basic'` for `oac_` rows; add the
  `AUTH_GATEWAY_BROKER_SECRET[_PREVIOUS]` / `GATEWAY_BROKER_SECRET` config knobs + entropy
  validation. Independently testable (an `oac_` row loads with `brokered=TRUE`; a bare public
  exchange is rejected; the broker secret authenticates).
- **P5a-2 (OP code paths):**
  (a) authenticate `brokered` clients on the authz_code grant against the broker-secret set
  (D3, generalising `authenticate_for_refresh`); (b) mint the **global** sub for `brokered`
  clients in `exchange_authorization_code` + the paired access token (D4). Faithful tests:
  `brokered` client → global sub in id_token; missing/wrong broker secret → `invalid_client`
  (a captured code is inert to app JS); `brokered=FALSE` clients unchanged (pairwise + existing
  auth); refresh grant likewise broker-authenticated.
- **P5a-3 (gateway re-home):** `OidcRp` holds the broker secret (not a `"gateway"`
  client_id/secret); Flow 1 threads `route.client_id`; both flows present basic-auth on the
  exchange; flip issuer to the OP `iss` and repoint `/oauth2/auth`→`/authorize`,
  `/oauth2/token`→`/token`; add RFC 9207 callback `iss` validation (D10); reshape the
  Hydra-shaped tests (`oidc_rp_e2e.rs`, `browser_auth_test.rs`) to OP-shaped, including a
  regression that app-JS-driven code exchange (no broker secret) fails.

---

## Open questions for the next critic

1. **Access-token subject for `brokered` clients (D4).** v2 makes the paired access token
   global-subjected for id/access `sub` consistency, arguing it is gateway-internal and
   `aud=app:{id}`. Is keeping it **pairwise** (one fewer global-sub artifact) the better
   defense-in-depth trade, accepting an id/access `sub` divergence that no resource server for
   this client consumes?
2. **Shared broker secret vs. per-app derived secret (D8).** v2 uses one platform broker secret
   for all `oac_` clients, arguing per-app derivation buys nothing because the gateway holds the
   master and is the sole confidential party. Is there a threat (e.g. a future non-gateway
   confidential exchanger, or per-app audit isolation) that justifies per-app secrets after all?
3. **`brokered` + console (D6).** The console is `brokered=TRUE` *and* `skip_consent=TRUE`. Is a
   global-subject, consent-skipping first-party client materially different from the direct
   first-party clients (control/builder) that are `brokered=FALSE`, or should the console-as-app
   be reconciled with them?
4. **Flow 1 liveness.** v2 re-homes Flow 1 onto `oac_` for completeness. If Flow 1
   (`finish_callback`) is in fact dead post-BFF-redesign, should P5a delete it rather than
   re-home it (pre-launch, no back-compat)?
5. **RFC 9207 enforcement point (D10).** Should callback `iss` validation be a hard reject in
   both flows from day one, or gated until the pluggable-IdP project lands the multi-upstream
   surface that makes mix-up attacks reachable?
6. **`brokered` naming.** Is `brokered` the clearest column name for "gateway authenticates as
   itself and receives the global sub", or does it invite confusion with unrelated "broker"
   usages elsewhere?

---

## v3 — adopted implementation decisions (post re-critic, 79/100 "SOUND BASIS TO IMPLEMENT")

The re-critic (`.p5a-client-model-critique-v2.md`, 79/100, v1 CRITICAL verified closed against
code) greenlit v2 and specified guardrails. These are ADOPTED and supersede the affected v2
decisions. No further design round — these fold the critic's own recommendations in.

- **A1 — broker-secret model = per-app HKDF, derive-and-compare (resolves OQ#2, MAJOR-2, MAJOR-3, MINOR-2/3).**
  The per-app broker secret is `HKDF-SHA256(master_broker_secret, info=client_id)`. The OP does
  **not** store a per-client secret hash for brokered clients (`client_secret_hash` stays NULL);
  instead it **derives the expected secret from the master + `client_id` at verify time and
  constant-time compares** the presented secret. Rotation is therefore **config-only**: the OP
  accepts `master_current` OR `master_previous` (a rolling window), the gateway sends
  `master_current`-derived; **auth-first 3-phase** deploy (OP learns the new master before the
  gateway sends it) so no `invalid_client` flap, no per-client re-hash. A single master leak is
  contained per-app only in the sense the derived secrets differ, but the master compromise =
  gateway compromise (already the trust root); per-app derivation buys audit isolation + limits a
  *derived*-secret leak to one app. The HKDF helper lives in `zeroship_core::auth`
  (both the OP verify path and the gateway derive path call it). Master entropy validated at boot
  (≥256-bit, reject the dev sentinel), mirroring `validate_pairwise_salt`. **Never logged.**
- **A2 — the load-bearing gate (resolves MAJOR-1).** The OP's authz_code grant authenticates
  **gating on `client.brokered` FIRST**, before any `token_endpoint_auth_method` switch: a
  `brokered` client MUST present a valid derived broker secret or the exchange fails
  `invalid_client` — there is no `'none'` path for a brokered client. Enforced in THREE places so a
  single regression can't reopen the exploit: (i) a DB CHECK
  `brokered = FALSE OR token_endpoint_auth_method = 'client_secret_basic'`; (ii) `load_client`
  hard-errors a `brokered` client whose `token_endpoint_auth_method != 'client_secret_basic'`;
  (iii) the grant handler's brokered-first gate. **D3 (enforce) and D4 (global sub) land in the
  SAME commit** (P5a-2) — the global-sub minting must never exist without its gate.
- **A3 — token subjects (resolves MINOR-1, OQ#1).** Build `Issuer::issue_principal_id_token`
  (an id_token carrying the **global principal `sub`**, mirroring `issue_principal_access_token`;
  today `issue_id_token` hardcodes pairwise at `issuer.rs:344`). For a brokered client: the
  **id_token `sub` = global principal id** (held server-side by the gateway, never forwarded), the
  **access token `sub` stays PAIRWISE per-app** (defense-in-depth; a leaked access token is still
  per-app-opaque). **MUST-VERIFY in P5a-2/3:** the gateway must read `global_user_id` from the
  **id_token** (`auth_token.rs:467` parses `claims.sub` as a UUID — confirm the source token is the
  id_token; if it currently reads the access token, switch it to the id_token so the access token
  can stay pairwise). If that switch is infeasible, fall back to global sub on both — but prefer
  pairwise access.
- **A4 — migration placement.** Fold `brokered BOOLEAN NOT NULL DEFAULT FALSE` + the A2 CHECK into
  the existing `V0064` `ALTER zeroship.oauth_clients` block (co-located with
  `client_secret_hash`/`token_endpoint_auth_method`), NOT a new `V0065` — pre-launch, no
  back-compat, test DB is rebuilt fresh (per the no-ALTER-drift stance).
- **A5 — RFC 9207 `iss` (resolves OQ#5).** Hard-reject a callback whose `iss` mismatches, in BOTH
  flows, from day one (cheap + correct; don't defer to the pluggable-IdP project).
- **A6 — Flow-1 liveness (OQ#4).** During P5a-3, VERIFY whether Flow 1 (`finish_callback`) is live
  post-BFF-redesign. If dead, DELETE it rather than re-home (pre-launch, no back-compat).
- **A7 — console (OQ#3) + `brokered` naming (OQ#6):** keep as-is for P5a (console brokered+skip_consent;
  `brokered` name retained). Not P5a-blocking.

### Implementation slices (atomicity-safe ordering)
- **P5a-1 (safe plumbing, zero behavior change — nothing is brokered yet):** V0064 gets the
  `brokered` column + CHECK (all rows FALSE); `zeroship_core::auth` gets the HKDF derive helper +
  master-secret config (current+previous, entropy-validated, never-logged); `load_client` reads
  `brokered` + enforces the A2(ii) refusal. Verify: migrations apply, load_client round-trips,
  nothing else changes.
- **P5a-2 (the OP core — D3+D4 ATOMIC):** brokered-first confidential derive-and-compare auth on
  the authz_code grant (A2) + `issue_principal_id_token` global-sub for brokered clients (A3), same
  commit. **PINNED regression tests:** (a) an app-driven code exchange WITHOUT the broker secret →
  `invalid_client`, for BOTH the cookie and SPA flows; (b) a brokered client's id_token `sub` =
  global principal, access token `sub` = pairwise; (c) a non-brokered `oac_` client is unchanged
  (pairwise id_token, no secret required — still PKCE); (d) exact-match redirect still enforced.
- **P5a-3 (activation + gateway re-home):** control marks `oac_` clients `brokered=TRUE` +
  `client_secret_basic` at creation; the gateway derives + sends the broker secret on BOTH flows,
  reads `global_user_id` from the id_token (A3), adds RFC 9207 `iss` (A5); flip `oidc_rp.rs:87`
  issuer trailing-slash, re-home endpoint URLs `/oauth2/auth`→`/authorize` +
  `/oauth2/token`→`/token` onto the OP base, repoint verify arms to OP JWKS; reshape the
  Hydra-shaped gateway tests (`oidc_rp_e2e.rs`, `browser_auth_test.rs`) to OP-shaped; handle A6.
