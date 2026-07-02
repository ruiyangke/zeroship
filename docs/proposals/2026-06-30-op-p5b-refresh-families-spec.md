# P5b OP spec — refresh-token families: rotation, reuse detection (CLI / programmatic only)

**Status:** proposal (design-only, uncommitted draft). Pre-launch; no back-compat (AGENTS.md "Development status").
**Scope:** add `grant_type=refresh_token` to the self-contained OP (`crates/auth/src/op/`) and the `zeroship.oauth_refresh_tokens` family store, for the **CLI / programmatic face only** — the `zeroship login` device-token deploy flow and confidential OAuth clients that request `offline_access`. The **browser / console face gets NO refresh family** (OQ-1 → Option B, see Revision log). The browser face is a bounded, server-authoritative gateway session with just-in-time (JIT) access-token re-mint; its contract is stated here in §8 and its implementation is deferred to **P5c** (the gateway re-home).
**Author seam:** `crates/auth/src/op/refresh.rs` (new), `crates/auth/src/op/authorization_code.rs` (issuance + `TokenResponse`), `db/migrations/V0064__oauth_refresh_tokens.sql` (new). The gateway's `do_refresh` is **removed**, not swapped (§8).

This spec was authored to be torn apart by a design-critic. Every storage rule, every rotation step, and every threat carries a named gating test. Where this spec and P0 (`2026-06-30-op-p0-spec-threat-model.md`) or the canonical schema redesign (`2026-06-30-auth-schema-redesign.md`) disagree, **the canonical names win** and the divergence is called out explicitly (the one deliberate canonical *amendment*, `family_absolute_expires_at`, is flagged as such in §2.2 / §6, not silently labelled "canonical").

---

## Revision log (round 7)

Round-6 audit (78/100) confirmed the deadlock-proof closure sound, with two mechanical gaps + one MINOR. **round 6→7:** (NEW-1) FK `ON DELETE CASCADE` is an uncoverable refresh-row writer — kept (option a) and explicitly carved out of the §5.4 universal as a narrow ADMIN hard-delete path (worst case = clean transient 5xx, no oracle/escaped-family/logout; app deletion SHOULD revoke under `user-lock(U)` first so the cascade is a no-op); the §5.4 guarantee is restated to cover **every application writer** + carve-out. (NEW-2) every `user-lock(U)` site now reads `hashtext(user_id::text)` verbatim (fixed §7 ×2 + the round-5 A-1 summary line; `hashtext(uuid)` would not compile). (MINOR item-3) pinned that `auth_credential_version` is stamped by the `/authorize` handler (auth-code) and the device-approval handler (device-code) at the moment of credential proof, threaded grant → code/device_code → refresh issuance. No other substance changed.

---

## Revision log (round 5)

This round closes the round-4 critique (76/100; 1 CRITICAL / 1 MAJOR / 6 MINOR; verdict **"needs another round"**). The blocker: the §5.4 deadlock proof was **not sound** — it asserted a universal ("**every** refresh-row writer takes `user-lock(U)` first ⇒ no `40P01` on **any** refresh path") that two real writers violated. This round makes the universal genuinely true by bringing **every** writer onto the one total order with **zero hand-waved carve-outs**, then restates the proof and its writer-coverage table.

| # | Sev | Finding | Resolution |
| --- | --- | --- | --- |
| A-1 | CRIT | The **sweeper (§6.1)** is a refresh-row `DELETE` writer absent from the §5.4 table; it takes no user-lock and can `40P01`-deadlock the credential-bump bulk revoke (both touch live-but-past-ceiling rows of user `U` in different scan orders → producible cycle). | **Resolved.** The sweeper now runs **per-user under `user-lock(U)`**: it enumerates distinct `user_id`s with sweepable rows, then for each user, in its own short transaction, takes `pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))` **before** a single set-based `DELETE` of that user's sweepable rows — the same pattern as the bulk revoke. It is now a first-class row of the §5.4 writer table; the total order `user→family→row` covers it; the producible `40P01` is gone. §6.1 rewritten, §5.4 table + proof updated. |
| A-2 | MAJOR | **Issuance (§3)** `INSERT`s a fresh root family, absent from the table, takes no user-lock; **and** the user-lock alone does not close the "escaped family" hole — a device-grant login on a stale `credential_version` racing a password reset creates a root the bulk revoke misses. | **Resolved.** Issuance now (1) takes `user-lock(U)` first (bringing it under the total order), and (2) **re-reads `users.credential_version` under that lock** and aborts (`invalid_grant`) if it no longer equals the `credential_version` the grant was authenticated against (a net-new `auth_credential_version` carried on the auth-code / device-code grant). In **both** interleavings the hole closes: issuance-after-revoke sees the bump and refuses; issuance-before-revoke commits a live root the revoke's `WHERE user_id=U AND revoked_at IS NULL` then catches. §3 rewritten, §5.4 writer table + "correctness bonus" rewritten, T9 updated. |
| MINOR-1 | MINOR | §5.2 column name `token_subject` is wrong; the real schema (`V0002:228`, live writer `oauth_grants_handlers.rs:240`) uses **`sub`**. | **Resolved.** Every reference corrected to `sub`. |
| MINOR-2 | MINOR | §5.4 needs a two-arg, xact-scoped advisory helper; the existing `crates/auth/src/advisory_lock.rs` is **session-scoped, single-i64-key** (`pg_advisory_lock($1)`). | **Resolved.** Specified a **net-new** `with_xact_advisory_lock2(conn, ns, key)` wrapping `pg_advisory_xact_lock(int4, int4)` on the **writing** connection; added to §5.4, §10, P5b-3. |
| MINOR-3 (B-1/B-2) | MINOR | P5b-7 residual is `max(cache TTL, NTP skew + 1s iat granularity)`, not "= cache TTL"; and **PATs are not covered** by the `token_revocations` read (separate credential path). | **Resolved.** Residual restated honestly across §5.2 / Open decision #1 / OQ-7 / residuals; explicit PAT-not-covered call-out added. |
| MINOR-4 (D-1) | MINOR | AEAD idem-cache ciphertext **physical** dwell is until the next **sweep** (≤5–15 min), not "≤30s"; each leaked successor is a **live chain-head** token. | **Resolved.** Dwell restated as **≤ the idem-reap cadence**; the sweeper gains a **prioritized, frequent idem-reap pass** (recommend ~60s) decoupled from the 5–15 min family-delete pass; §2.2 / T6 / §5.3 / §6.1 / residuals corrected; the "live chain-head until reaped" consequence stated honestly. |
| MINOR-5 | MINOR | `hashtext`→int4 collisions (32-bit space). | **Noted.** One line in §5.4: collisions degrade to coarser **cross-user serialization** (perf), **never** deadlock, because every path takes the user-lock *before* any row lock; the user tier is coarser than per-user at scale. |

**PostgreSQL grounding for the fix.** Advisory locks are held in the same shared lock manager as heavyweight locks and **participate in the deadlock detector**, so an advisory lock and a row lock can together form a `40P01` cycle — which is *exactly* why an un-ordered writer (the round-4 sweeper) could deadlock the bulk revoke, and why the only cure is to put **every** writer on one consistent total order (PostgreSQL manual §13.3.5: "the best defense against deadlocks is generally to … be certain that all applications … acquire locks on multiple objects in a consistent order"). `pg_advisory_xact_lock(key1 integer, key2 integer)` is transaction-scoped (released only at `COMMIT`/`ROLLBACK` — "there is no provision for manual release"), giving the two-namespace (`$NS_USER`, `$NS_FAM`) keyspace §5.4 needs; the existing session-scoped single-key helper cannot express it, so a new helper is added (MINOR-2).

**Honest residual after round 5:** (1) the P5b-7 cross-RS access-token window is `max(cache TTL, NTP skew + 1s iat granularity)`, and **PAT-authenticated deploys are a separate credential path** the family kill does not reach. (2) The idem-cache ciphertext dwell under (DEK compromise **and** DB dump) is ≤ the idem-reap cadence (~60s recommended), each leaked successor a live chain-head until rotated/killed. (3) The T1 idle-victim and bounded-idempotency-window residuals are unchanged and intrinsic. (4) Under `hashtext` collisions the user tier serializes two distinct users — a perf cost, never a deadlock.

---

## Revision log (round 3)

This round closes the round-2 critique (74/100; 0 CRITICAL / 3 MAJOR / 4 MINOR — the three foundational CRITICALs were confirmed dissolved). It fixes two **guarantee-invalidating** defects and one **silently-regressed contract**, plus four polish items. The two scope-changing decisions are consolidated into a new **§ Open operator decisions** below.

| # | Sev | Finding | Resolution |
| --- | --- | --- | --- |
| A | MAJOR | The credential-bump **bulk** revoke (`WHERE user_id=…`, cross-family) takes no advisory lock and can `40P01`-deadlock a concurrent rotation (the T9 path) → violates the §5.4 "no 40P01 leaks" guarantee. | **Resolved.** Introduce a strict two-level lock hierarchy **user-lock → family-lock → row-locks** (§5.4). *Every* refresh-row writer takes the per-`user_id` advisory lock **first**; rotation and single-family revoke then take the per-family lock; the cross-family credential-bump revoke takes only the user lock and runs a set-based UPDATE. Because the user lock is exclusive and is the outermost lock in a total order, no two transactions ever hold row locks on the same user's families concurrently → the circular wait is impossible. Proof in §5.4. The user lock taken *before* any child INSERT also closes the correctness hole (no family can be created mid-bump and escape the revoke). §4 step 2, §7, T9/T10 reconciled. |
| B | MAJOR | **False** claim that the control plane consults `token_revocations`. Verified: control only **writes** the marker (`oauth_grants_handlers.rs:240`); it has no `revoked_after_for` read on the request path. So a revoked CLI **deploy** family's `at+jwt` keeps authorizing `zeroship deploy` for ≤900s. Deploy tokens push code — this is the headline CLI guarantee and it was unenforced *and* mis-stated. | **Resolved (build it).** The false "already consults / do consult" wording is removed (§5.2, residuals). A **control-plane revocation read** is specified on the deploy/authz request path: the control bearer guard (`oauth_guard_from_bearer`) calls `revoked_after_for(client_id, pairwise_sub)` (cache-backed, short TTL) and rejects an `at+jwt` whose `iat` predates the family's `revoked_after`. The residual shrinks from ≤900s to **= the cache TTL** (recommend 10s). Added as a scheduled deliverable **P5b-7** with a faithful control-path gating test. Surfaced as **Open operator decision #1** (immediate-enforcement vs accept-the-≤900s-residual) — recommend immediate for deploy tokens. |
| C | MAJOR | Option B reused the **IdP login** session (12h absolute / 30-min idle, `login.rs:14-15`) verbatim for the console face — a silent regression from the retired 30-day, no-idle browser anchor (`anchors.rs:57`), and under-specified for a P5c build. | **Resolved.** §8.1 now specifies a **dedicated console/app session** distinct from the short interactive IdP login session: its own absolute + idle lifetimes, server-authoritative, revocable on `credential_version` bump + logout, with **step-up re-auth** for sensitive actions. Recommended default **30-day absolute ceiling + 7-day sliding idle** (preserves the old 30-day reach, replaces "no idle" with a 7-day idle so abandoned consoles die in a week, and is vastly better than 30-min idle). Surfaced as **Open operator decision #2** with the exact numbers to tune. This is the P5c console contract. |
| D | MINOR | `idem_response_enc` stores the raw successor refresh token at rest, contradicting T6's unqualified "only HMAC stored, unforgeable". | **Resolved.** The cache is sealed with **AEAD** (XChaCha20-Poly1305 / AES-256-GCM) under a file/KMS-held DEK, AAD-bound to `(token_hash, refresh_family_id)` so a ciphertext cannot be relocated; dwell minimized (≤`idem_window`, swept). T6 is **re-stated honestly** (§2.2, T6): the only raw secret ever at rest is the AEAD-sealed successor, and a DB-only dump *without the key* still yields zero usable tokens. A stronger raw-free alternative (deterministic successor derivation, no ciphertext at rest) is noted for a future tightening. |
| E | MINOR | Scope ratchet: copying the narrowed request into the child's `granted_scopes` lets a one-time narrow permanently lower the ceiling — diverges from RFC 6749 §6 ("originally granted by the resource owner"). | **Resolved.** Add an immutable **`family_granted_scopes`** column (the original grant ceiling, copied verbatim to every child like `family_absolute_expires_at`). Each rotation validates `requested ⊆ family_granted_scopes` (the *original* grant, not the previous token's scope), so a client may re-widen back up to — never above — the original grant. RFC 6749 §6 compliant. §3, §4 step 7/8, T3, DDL updated. |
| F | MINOR | Key-rotation runbook is graceful-only; no emergency key-compromise path. | **Resolved.** §2.4 adds an **emergency** fork: on `refresh_hash_key` compromise, drop the leaked version from the keyring **immediately** (do not wait the 30-day overlap) → families minted under it fail closed → accept the bounded mass re-auth. Graceful vs emergency explicitly distinguished. |
| G | MINOR | No DX note that confidential OIDC clients get no `id_token` on refresh. | **Resolved.** §1 adds an explicit contract note: identity refresh is via `/userinfo`, **not** the refresh response; confidential OIDC client libraries expecting a refreshed `id_token` must call `/userinfo`. |

**Honest residual after round 3:** the cross-RS access-token window is now **= cache TTL (~10s)** at the gateway *and* (once P5b-7 lands) the control plane — not fully zero, because we deliberately avoid per-request introspection for latency. Any *future* resource server that neither reads `token_revocations` nor caps the access-token TTL re-opens a ≤900s window for itself; that is an RS obligation, stated in §5.2. The T1 idle-victim and bounded-idempotency-window residuals are unchanged and intrinsic (see § What this round could NOT fully resolve).

---

## Open operator decisions

Two round-3 fixes are genuine product/operations forks. Both are specified with a recommended default so a build can proceed; the operator confirms or tunes.

**Decision #1 — CLI/deploy access-token revocation enforcement (MAJOR-B).**

| Option | Mechanism | Residual after a family kill | Cost |
| --- | --- | --- | --- |
| **(A) Immediate enforcement — RECOMMENDED for deploy tokens** | Control bearer guard reads `revoked_after_for(client_id, pairwise_sub)` on every authenticated control/deploy request, cache-backed (short TTL). Mirrors the gateway, which already does this. | **`max(cache TTL, NTP skew + 1s iat granularity)`** (recommend 10s TTL; the skew term is sub-second under NTP). A killed family stops deploying within ~10s. <!-- corrected round 5 (B-1) --> Does **not** cover PAT-authenticated deploys (B-2) — separate path. | One indexed lookup per request, served from a short-TTL in-process cache; ~1 DB hit per (client,subject) per TTL. |
| (B) Accept the access-token-TTL residual | Control does not read the marker; rely solely on the 900s `at+jwt` TTL expiring. | **≤ 900s.** A revoked deploy token keeps pushing code for up to 15 min. | Zero added work; no cache. |

**Recommendation: (A).** Deploy tokens can push code; a 15-minute window in which a *revoked* token still deploys is not acceptable for the platform's highest-value credential. Build it as **P5b-7**. (B) is defensible only if the access-token TTL is dropped well below 900s for the deploy audience, which hurts the common case.

**Decision #2 — Console/app session lifetimes (MAJOR-C).**

| Knob | Old browser anchor (retired) | IdP login session (do **not** reuse) | **Recommended console default** |
| --- | --- | --- | --- |
| Absolute ceiling | 30 days | 12 hours | **30 days** (preserve the old reach) |
| Idle (sliding) | none ("survive long idle gaps") | 30 minutes | **7 days** (abandoned console dies in a week) |
| Step-up re-auth for sensitive actions (rotate secret, delete app, change billing/payout) | — | — | **required**, independent of session age |
| Revocation triggers | `credential_version` mismatch | `revoked_at` + `credential_version` | **both** + explicit logout |

**Recommendation: 30-day absolute + 7-day sliding idle + step-up.** This keeps the weekly-active developer effectively always-logged-in (matching the retired 30-day anchor's convenience) while replacing "no idle ever" with a 7-day idle that reaps abandoned sessions, and gates the genuinely dangerous actions behind a fresh re-auth regardless of session age. It is a **distinct** session kind from the 12h/30-min interactive IdP login session (which stays as-is for the login UI itself). Numbers are operator-tunable; this is the P5c console contract.

---

## Revision log (round 2)

This round folds in the operator's **OQ-1 → Option B** ruling and resolves the round-1 critique (60/100; 3 CRITICAL / 7 MAJOR / 5 MINOR).

**OQ-1 → Option B — the browser/console face gets no refresh family.** Refresh families are now **CLI / programmatic only** (device-token deploy + confidential `offline_access` clients). The browser face is a **bounded server session + JIT access-token re-mint**: the gateway (trusted BFF) holds a server-authoritative session (idle + absolute expiry, instantly revocable via `revoked_at` / `credential_version`) and mints short-lived access tokens on the request path from that live session — **no refresh token, no cron, no `app_session_anchors` rotation**. This reuses the session **machinery** (`crates/auth/src/store/sessions.rs::validate`: slides idle, checks `revoked_at IS NULL` and `credential_version = users.credential_version`) — but **NOT** the short interactive IdP login-session *lifetimes* (`sessions/login.rs`: 30-min idle / 12-h absolute). **Superseded by round-3 MAJOR-C:** the console face gets a **dedicated** session policy (recommended 30-day absolute / 7-day idle + step-up — §8.1, Open operator decision #2), not the login session's lifetimes. The gateway-side build is **P5c**; this spec specifies only the boundary/contract (§8).

**The worst CRITICAL is dissolved, not patched.** Round-1 CRITICAL #4 (the no-grace-window-vs-per-thread-single-flight browser **reload storm** that converted ordinary multi-tab reloads into family-kills and forced re-logins) **no longer exists**: there are no browser refresh tokens, so there is no multi-tab reload storm against a refresh family at all. The browser face does session-`validate` + JIT mint, which is idempotent and never rotates a refresh token. Likewise CRITICAL #1 (false reconciliation with P0) and CRITICAL #2 (public-vs-confidential `do_refresh` contradiction + per-app secret-distribution subsystem) are **dissolved by Option B** — see the table below.

**Reconciliation with P0 — now genuine, not a "wording amendment".** Option B *agrees* with P0 §2.4 / §446 / §551 / §559 ("The BFF/browser face receives no OAuth refresh token … the existing BFF `anchors.rs` pattern is retired for this face"). The round-1 spec's proposed "amendment to P0 §2.4" is **removed**. This spec no longer overturns P0's browser-face decision; it implements P0's CLI-only refresh decision (P0 §4.1).

### Per-finding resolution

| # | Sev | Finding | Resolution |
| --- | --- | --- | --- |
| 1 | CRIT | False reconciliation with P0 (reversal dressed as wording fix) | **Dissolved.** Option B adopts P0 verbatim; the §0.1 "amendment" is deleted. §0 reframed: one model, holders = CLI + confidential `offline_access` clients, **not** the browser. |
| 2 | CRIT | Confidential-vs-public contradicts real `do_refresh`; hidden per-app `client_secret` subsystem | **Dissolved.** The browser/gateway no longer does OAuth refresh — `do_refresh` is *removed*, not swapped (§8). The only confidential refresh clients are genuinely-registered confidential OAuth clients that custody **their own** secret (standard OAuth); there is no per-app gateway-app secret distribution. |
| 3 | CRIT | `id_token` on refresh needs a `nonce` that is deliberately not stored | **Resolved: omit `id_token` on refresh** (OIDC Core §12.2 makes it OPTIONAL; if present its `nonce` MUST equal the original, which the family does not store → omit). Identity facts come from `/userinfo` / the access token. §4 step 9 rewritten. |
| 4 | MAJOR | Per-thread single-flight cannot absorb cross-thread browser reload storm → spurious family kills | **Dissolved** by Option B (no browser refresh). For the CLI, benign lost-response retries are handled by **bounded idempotency** (§5.3), not by single-flight. |
| 5 | MAJOR | Family-kill multi-row `UPDATE` can deadlock the single-row `FOR UPDATE`; abort surfaces as non-`invalid_grant` | **Resolved.** A transaction-scoped **family advisory lock** (`pg_advisory_xact_lock(ns, hashtext(family_id))`) serializes the whole family *before* any row lock, so the multi-row kill can never deadlock against another presenter (§4 step 2, §5.4). |
| 6 | MAJOR | `family_absolute_expires_at` is not canonical; presented as "canonical" | **Resolved (honest amendment).** The column is flagged as a **deliberate canonical amendment** (§2.2), justified (avoids root-row walk), and the schema-redesign + P0 §3.2 are noted as needing the same column. Not labelled "canonical". |
| 7 | MAJOR | Three independent 30d clocks; anchor death orphans an OP family | **Mostly dissolved** by Option B (no browser anchor → no anchor/OP orphan). CLI has a single store with two clocks (idle + family ceiling), both enforced by the OP itself — no cross-store orphan (§6). |
| 8 | MAJOR | Cross-arm access-token kill relies on unproven pairwise equality + under-specified marker fan-out | **Resolved.** Single source of truth for pairwise (`AUTH_PAIRWISE_SALT` + `app_oauth_clients.sector_identifier`); fan-out granularity specified as one `token_revocations` row **per `(client_id, pairwise_sub)`** the user has a live family in; CLI access-token enforcement (which RS reads the marker, and the ≤TTL residual) specified (§5.2, §7). |
| 9 | MAJOR | HMAC key has no rotation/versioning → key roll = mass logout | **Resolved.** Decide OQ-2 → keyed HMAC-SHA256, file/KMS-held, fenced. Add a `hash_key_version` column + a small verify keyring with a 30d overlap window (mirrors the JOSE signing-key overlap, P0 §612). New tokens use the active key; old keys verify until all families minted under them expire (≤30d). No mass logout (§2.1, §2.4). |
| 10 | MAJOR | No grace → lost-response retry false-kills; T1 overstates theft protection | **Resolved.** **Bounded idempotency** (§5.3): a presentation of a just-rotated token whose successor is still live, within a short window, replays the cached response (same successor) instead of killing the family. Genuine replay (window elapsed *or* chain advanced) still kills. T1 residual (idle-victim silent thief rotation up to the ceiling) named honestly (§9 T1). |
| 11 | MINOR | Grant block drops the `DO $g$ … IF EXISTS pg_roles` guard | **Resolved.** §2.3 mirrors `V0063` role-existence guard exactly. |
| 12 | MINOR | "standard-SQL-first" inaccurate (`TEXT[]`/`BYTEA`/partial-unique are PG) | **Resolved.** Restated as "PG-native, consistent with `V0063`; CHECK domain over a PG `ENUM` type" (§2.3). |
| 13 | MINOR | `/revoke` missing RFC 7009 §2.1 client-binding check | **Resolved.** §7 adds the client-binding check; revoke only if the token was issued to the authenticated client; uniform 200 otherwise (no oracle, no cross-client DoS). |
| 14 | MINOR | No test for the *legit* concurrency case (storm must NOT kill) | **Resolved.** New gating test `legit_lost_response_retry_recovers_without_family_kill` (§5.3, §11 P5b-4). |
| 15 | MINOR | Sweeper referenced but undefined | **Resolved.** §6.1 defines owner (`zeroship_auth`), cadence, retention horizon, idem-cache reaping, and the benign partial-unique-index interaction. |

**Missing-concepts** raised by the critic: family-kill-rate metric + alerting → §9.1 (new); HMAC key rotation → §2.4; per-app gateway-app secret → dissolved (§0, §8); anchor-death ⇒ OP revoke → dissolved (§8); CLI access-token revocation enforcement → §5.2/§7; reload-storm reliability budget → dissolved + idempotency (§5.3).

---

## 0. One refresh model, two holders — neither is the browser

There is **one** `zeroship.oauth_refresh_tokens` table and **one** rotation/reuse algorithm. It serves two holders that differ only in *who holds the opaque token* and *how the client authenticates* — never in the storage model or the reuse-detection logic.

| | **CLI / device-token** (public) | **Confidential `offline_access` client** |
| --- | --- | --- |
| Example | `zeroship login` → deploy token (RFC 8628 device grant) | a creator's server-side integration registered with a secret |
| Who holds the raw refresh token | the CLI process, on disk (`~/.config/zeroship`) | the confidential client, server-side |
| OAuth client type | **public** (RFC 8252 §8.5) — auth by possession of the token + `client_id`, no secret | **confidential** — authenticates to `/token` with its **own** registered `client_secret` (`client_secret_basic`/`_post`) |
| `offline_access` required to mint | yes | yes |
| Holds a per-app gateway secret? | n/a | **no** — it custodies its *own* secret, the same one it used at `/authorize`. There is no platform-issued per-app secret. |

**The browser / console face is NOT in this table.** Under Option B it has no refresh token. Its longevity anchor is a server-authoritative gateway session, and reload-recovery is session-`validate` + JIT access-token mint, not refresh rotation (§8). This is exactly P0 §4.1's per-face decision.

**The OP `/token` refresh path is byte-identical for both holders.** The table, the family advisory lock, the atomic consume, the bounded-idempotency replay, the family-kill `UPDATE`, the `invalid_grant` signal — all shared. The only difference is client authentication (public possession vs confidential secret) in step 1.

> **Why this shape (industry grounding).** Server-held BFF longevity from a session rather than a browser-exposed refresh token is the Curity/Duende "Token Handler / BFF" pattern and the OAuth 2.0 for Browser-Based Apps BCP recommendation: the browser should never hold a refresh token; a confidential backend mediates. Option B takes the stronger form — the backend holds *no* refresh token either, just a revocable session — because the gateway is co-located with the IdP and can validate a session row directly (the IdP already does this for `idp_sessions`). Refresh-token rotation with reuse detection (this spec, for the CLI) is the RFC 9700 §4.14 / OAuth 2.1 §6.1 recommendation for the public/native face that *cannot* hold a server session.

---

## 1. Endpoint contract: `POST /token`, `grant_type=refresh_token`

Today `token_inner` (`authorization_code.rs:308`) rejects every grant ≠ `authorization_code` with `unsupported_grant_type`. P5b makes `/token` a dispatcher on `grant_type`:

```
grant_type=authorization_code  -> exchange_authorization_code   (existing)
grant_type=refresh_token       -> exchange_refresh_token         (new, op/refresh.rs)
```

**Request** (`application/x-www-form-urlencoded`):

| Param | CLI / device-token (public) | Confidential `offline_access` client |
| --- | --- | --- |
| `grant_type` | `refresh_token` | `refresh_token` |
| `refresh_token` | required | required |
| `client_id` | required | required (or via client auth) |
| `client_secret` | absent | required (`client_secret_basic` or `_post`), verified against `oauth_clients.client_secret_hash` |
| `scope` | optional; subset of granted only | optional; subset only |

**Success response** — RFC 6749 §5.1 shape, `Cache-Control: no-store`, `Pragma: no-cache`, `application/json`:

```json
{
  "access_token": "<at+jwt>",
  "token_type": "Bearer",
  "expires_in": 900,
  "refresh_token": "zrt_<base64url(32 random bytes)>",
  "scope": "openid offline_access ..."
}
```
<!-- Changed in round 2 (addressing CRITICAL #3): no `id_token` in the refresh response. OIDC Core §12.2 makes it OPTIONAL on refresh; if present its `nonce` MUST equal the original Authentication Request nonce, which the family deliberately does not store (§2.2). We therefore OMIT it. Identity facts on refresh come from `/userinfo` or access-token claims. -->

<!-- Added in round 3 (addressing MINOR-G): DX contract note for confidential OIDC clients. -->
> **DX note — no `id_token` on refresh (confidential OIDC clients).** A `refresh_token` grant returns **no `id_token`** (rationale in §4 step 9 / threat model). Several confidential-client OIDC libraries expect a refreshed `id_token` and will error or silently fail to refresh session claims if they assume one. For zeroship's OP the contract is explicit: **identity refresh is via `/userinfo`** (or the `at+jwt`'s own claims), not the refresh response. A confidential `offline_access` client that needs current identity claims after a refresh MUST call `/userinfo` with the new access token; it must not block on an `id_token` in the `/token` response.

This requires adding `refresh_token: Option<String>` to `TokenResponse` (`authorization_code.rs:57`) with `#[serde(skip_serializing_if = "Option::is_none")]`, and extending `TokenRequest` (`authorization_code.rs:47`) with `refresh_token`, `client_secret`, `scope`.

**Error response** — RFC 6749 §5.2, the existing `oauth_error_response` (`authorization_code.rs:693`). The load-bearing one is `invalid_grant` (HTTP 400, `{"error":"invalid_grant"}`). **Reuse, expiry, revocation, family-dead, wrong-client, and wrong-family ALL collapse to one indistinguishable `invalid_grant`** — no oracle (mirrors the auth-code rule at `authorization_code.rs:371-374`). Client-authentication failure (confidential) is `invalid_client`. A *transient* failure (5xx / serialization abort) is **never** `invalid_grant` (§5.4) — the family advisory lock guarantees we never leak a deadlock abort as `invalid_grant`.

Conformance: RFC 6749 §1.5, §6, §5.1/§5.2; OAuth 2.1 §4.3; conformance-map §4 `/token` checklist "Support `grant_type=… refresh_token …`" + "Refresh-token rotation".

---

## 2. Storage model — `zeroship.oauth_refresh_tokens`

Canonical table (schema-redesign §`oauth_refresh_tokens`; P0 §3.2), with two **explicitly-flagged amendments** (§2.2): `family_absolute_expires_at` and `hash_key_version`. **Net-new** (Hydra owns refresh families today; under Option B + the OP, Hydra is removed from this path). The auth-code store `V0063` (`oauth_authorization_codes`) is the structural template.

### 2.1 DDL (design — no SQL committed)

```
CREATE TABLE zeroship.oauth_refresh_tokens (
    token_hash             BYTEA       NOT NULL PRIMARY KEY,        -- HMAC-SHA256(refresh_hash_key[version], raw); raw NEVER stored
    hash_key_version       SMALLINT    NOT NULL,                    -- which refresh_hash_key minted this hash (§2.4, key rotation)
    refresh_family_id      TEXT        NOT NULL,                    -- THE family. No families table.
    client_id              TEXT        NOT NULL REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,  -- ON DELETE CASCADE is a refresh-row writer the advisory hierarchy cannot cover (FK machinery takes no advisory lock); kept deliberately and carved out of the §5.4 universal as a narrow ADMIN hard-delete path (NEW-1)
    user_id                UUID        NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,                 -- ditto; app-level account/client deletion SHOULD revoke the family under user-lock(U) first (§5.4 carve-out / §7) so the cascade is usually a no-op
    granted_scopes         TEXT[]      NOT NULL,                    -- scopes THIS token is issued for (per-rotation request, ⊆ family ceiling)
    family_granted_scopes  TEXT[]      NOT NULL,                    -- AMENDMENT (MINOR-E): original grant ceiling; copied verbatim to every child, NEVER changed; the RFC 6749 §6 "originally granted" bound
    issued_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at             TIMESTAMPTZ NOT NULL,                    -- sliding idle window, capped at family ceiling
    family_absolute_expires_at TIMESTAMPTZ NOT NULL,               -- AMENDMENT: root issued_at + 30d; copied to every child, NEVER slid
    rotated_at             TIMESTAMPTZ NULL,                        -- consume marker: non-null on a presented token == candidate replay
    replaced_by_token_hash BYTEA       NULL,                        -- forward rotation chain (successor lookup for idempotency)
    idem_response_enc      BYTEA       NULL,                        -- §5.3 bounded-idempotency: AEAD-sealed (XChaCha20-Poly1305/AES-256-GCM, file/KMS DEK, AAD = token_hash||refresh_family_id) successor response, set on rotation, swept promptly (§6.1)
    idem_expires_at        TIMESTAMPTZ NULL,                        -- §5.3 idempotency window (short, e.g. 30s)
    revoked_at             TIMESTAMPTZ NULL,
    last_used_at           TIMESTAMPTZ NULL,
    CONSTRAINT oauth_refresh_tokens_idle_le_ceiling
        CHECK (expires_at <= family_absolute_expires_at)
);

-- one live token per family (the rotation invariant; replaces the old WHERE status='active')
CREATE UNIQUE INDEX oauth_refresh_tokens_one_active_per_family
    ON zeroship.oauth_refresh_tokens (refresh_family_id)
    WHERE rotated_at IS NULL AND revoked_at IS NULL;

CREATE INDEX oauth_refresh_tokens_family_idx
    ON zeroship.oauth_refresh_tokens (refresh_family_id, client_id, user_id);

CREATE INDEX oauth_refresh_tokens_expires_at_idx
    ON zeroship.oauth_refresh_tokens (expires_at);
```

### 2.2 Reconciliations against the canonical column list (the critic will check these)

| Canonical / prompt | What this spec stores | Why |
| --- | --- | --- |
| `token_hash = HMAC-SHA256(...)` (canonical) | **HMAC-SHA256(refresh_hash_key[version], token)** + `hash_key_version` | Keyed MAC (matches P0 §2.4): a DB-only dump cannot verify or forge a token without the file/KMS-held key. `hash_key_version` makes the key **rotatable** without mass logout (§2.4) — this is the round-1 #9 fix. |
| `expires_at` only (canonical), root row = ceiling (P0 §559) | `expires_at` (slides) **+** `family_absolute_expires_at` (fixed) | **Deliberate canonical amendment**, not "canonical". Storing the ceiling on every child avoids walking to the root row on every rotation (root may be swept while children live). Two clocks let us keep a *shorter* sliding idle under the 30d ceiling (§6, OQ-5). **The schema-redesign §`oauth_refresh_tokens` and P0 §3.2 must add `family_absolute_expires_at` + `hash_key_version` to stay the source of truth** (this round flags it instead of silently diverging — round-1 #6 fix). |
| `replaced_by_token_hash` (canonical, forward chain) | `replaced_by_token_hash` + `refresh_family_id` | Forward pointer + family id make a backward `parent_token_hash` redundant. The forward pointer is also the **successor-liveness probe** for bounded idempotency (§5.3). |
| `rotated_at` (canonical) | `rotated_at` | "consumed" *is* "rotated" for a refresh token; one marker. |
| pairwise / sector binding | `(user_id, client_id)` + persisted pairwise `sub` snapshot | Pairwise `sub = derive_pairwise(AUTH_PAIRWISE_SALT, user_id, sector)` (`issuer.rs:349`), sector from `app_oauth_clients.sector_identifier`. The OP re-derives it when minting access tokens, and the refresh row also persists the derived snapshot solely for family-kill marker writes from set-based SQL paths. This is a deliberate implementation amendment to the earlier "never persisted" wording: `app_oauth_clients.sector_identifier` is immutable after insert, so the snapshot cannot de-align from future mints for the same client. |
| `granted_scopes` (canonical, per-token) **+** `family_granted_scopes` (amendment) | per-token issued scope **and** the immutable original-grant ceiling | **Deliberate canonical amendment (MINOR-E).** `granted_scopes` is what *this* token carries; `family_granted_scopes` is the scope the resource owner **originally granted**, copied verbatim to every child and never changed. Rotation validates the request against `family_granted_scopes` (RFC 6749 §6 "originally granted"), so a one-time narrow does **not** permanently lower the ceiling — a later rotation may re-widen back up to, but never above, the original grant (§4 step 7). The schema-redesign + P0 §3.2 must carry this column too. |
| `idem_response_enc` / `idem_expires_at` | new, nullable; **AEAD-sealed** | Bounded-idempotency cache (§5.3). **AEAD** (XChaCha20-Poly1305 / AES-256-GCM) under a file/KMS-held DEK (sibling custody to `refresh_hash_key`), AAD-bound to `(token_hash, refresh_family_id)` so the ciphertext cannot be relocated to another row; fenced to `zeroship_auth`, logical TTL ≤`idem_window` (~30s), physically reaped on the prioritized idem-reap pass (§6.1 Pass 2). **T6 reconciliation (MINOR-D / round-5 D-1):** this is the *only* raw secret ever at rest. The at-rest invariant is therefore stated precisely (see T6): token **verifiers** are one-way HMAC, and the one raw secret — the sealed successor — is AEAD ciphertext whose key never touches Postgres, so a **DB-only dump still yields zero usable tokens**. The marginal exposure is named, not denied, and corrected for **physical** dwell: under (file-key **and** DB dump together), an un-reaped sealed successor decrypts to a **live chain-head refresh token**, exposed for **≤ the idem-reap cadence (~60s recommended)** — not the 30s *logical* window, since the ciphertext physically dwells until reaped — usable until that successor is itself rotated/killed. A raw-free alternative (deterministic successor derivation from `(DEK, predecessor_hash)`, nothing cached at rest) is a future tightening that removes even this. |

### 2.3 Grants — refresh tokens are secret-fenced (`zeroship_auth` ONLY)

Identical fence to `V0063` (`oauth_authorization_codes`, lines 28-44) and schema-redesign line 81 ("intentionally not granted to worker/app/gateway"). <!-- Changed in round 2 (addressing MINOR #11): role-existence guard mirrors V0063 exactly. -->

```
DO $g$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
    GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.oauth_refresh_tokens TO zeroship_auth;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
    REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_control;
  END IF;
  -- …same guarded REVOKE for zeroship_gateway, zeroship_worker, zeroship_app …
END
$g$;
```

Only `zeroship_auth` can resolve a `token_hash` to a family. The gateway never reads this table (under Option B it has no anchor store either — §8).

<!-- Changed in round 2 (addressing MINOR #12): the "standard-SQL-first" claim was inaccurate. -->
**PG-native, consistent with `V0063`.** `TEXT[]`, `BYTEA`, `TIMESTAMPTZ`, and the partial unique index are PostgreSQL features (the platform DB is PG). The state machine is modelled with a **CHECK domain / nullable-timestamp markers rather than a PG `ENUM` type** (matching the `pkce_method = 'S256'` CHECK style in `V0063:13`) — that is the "no enum type" rule, not a portability claim.

### 2.4 `refresh_hash_key` rotation (round-1 #9)

The token hash is a **keyed** MAC, so naive key rotation would orphan every stored hash → mass logout. Mitigation (mirrors the JOSE signing-key overlap window, P0 §612):

- `refresh_hash_key` is a small **keyring** loaded at boot from `REFRESH_HASH_KEY_FILE` (or KMS), each entry `{version, key}`. Sibling custody to `AUTH_SIGNING_KEY_FILE`. Never in Postgres.
- Exactly one **active** version mints new hashes; the row records `hash_key_version`.
- On verification, HMAC with the key named by the row's `hash_key_version`. Old versions stay in the keyring for **verify-only** until every family minted under them has expired (≤ 30d ceiling), then the version is dropped.
- **Graceful rotation runbook** (key not compromised, hygiene roll): add new active version → all *new* roots use it → keep the retired version **verify-only** → wait one full ceiling window (30d) → drop the retired version. No live family is invalidated. Decide **OQ-2 = keyed HMAC-SHA256, versioned.**

<!-- Added in round 3 (addressing MINOR-F): emergency key-compromise path distinct from the graceful 30d overlap. -->
- **Emergency rotation runbook** (a `refresh_hash_key` version **leaks**). The graceful 30d verify-overlap is exactly *wrong* here — you must stop honoring the leaked version **immediately**, not in 30 days. Steps: (1) add a fresh active version (new roots mint under it); (2) **drop the compromised version from the keyring now** (remove it from both mint and verify), do **not** keep it verify-only. Consequence: every live family whose `hash_key_version` = the compromised version can no longer be verified → those refresh tokens **fail closed** (`invalid_grant`) on next presentation → a **bounded mass re-auth** of exactly the affected families (re-login mints a fresh root under the new version). This is the accepted, deliberate cost of containing key compromise; the blast radius is the set of families minted under the leaked version (≤ one 30d ceiling window's worth), surfaced by the §9.1 family-kill/`invalid_grant` rate. Distinct from a **signing-key** (JOSE) compromise, which is handled by the access-token signing-key revocation in P0 §612 — the two key custodies and emergency paths are independent.

---

## 3. Issuance — minting the root refresh token at the authorization-code (or device) grant

When `exchange_authorization_code` (`authorization_code.rs:346`) — or the device-code grant — succeeds AND `offline_access ∈ granted_scopes` AND `client.refresh_allowed`, mint a root refresh token **inside the existing `/token` transaction** (`token_inner` already wraps `BEGIN/COMMIT`) alongside the access token.

<!-- Rewritten in round 5 (CRITICAL A-2): issuance is a refresh-row writer. Round 4 caught that it was ABSENT from the §5.4 hierarchy (falsifying the universal proof) AND that the user-lock alone does not close the escaped-family hole — a device-grant login on a stale credential_version racing a password reset creates a root the bulk revoke misses. Fix: issuance now takes user-lock(U) FIRST (puts it on the total order) AND re-checks credential_version under the lock (rejects a root minted on stale credentials). -->

1. After the access token is minted, check `granted_scopes.contains("offline_access")` and `client.refresh_allowed`. If either is false → `TokenResponse { refresh_token: None, .. }` (unchanged) — no refresh row is written, so the lock below is not taken.
2. **Take the outer user-lock (the §5.4 hierarchy, round-5 A-2).** `SELECT pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))` on the `/token` transaction's connection, **before** any refresh-row write. Issuance writes a *fresh* `family_id` and inserts exactly one row, so it does **not** take the family-lock — but it MUST take the user-lock so it sits on the one total order `user→family→row` that the §5.4 universal proof quantifies over (taking a prefix of the order — user-lock then straight to the row tier — is legal ordered-locking; what is forbidden is acquiring out of order). This also serializes issuance against a concurrent credential-bump bulk revoke (§7), which holds the same `user-lock(U)`.
3. **Re-check `credential_version` under the lock (closes the escaped-family TOCTOU, A-2).** Re-`SELECT users.credential_version FROM zeroship.users WHERE id = user_id` **under the held user-lock** and compare it to `auth_credential_version` — the `credential_version` that was current when this grant was *authenticated*, carried on the auth-code / device-code grant record (a **net-new** column on the grant; see §10). If they differ, a password reset / credential bump has landed since authentication → **abort issuance** (`invalid_grant`, no root written).

   <!-- Added in round 7 (item 3): pin down WHICH handler stamps auth_credential_version, and that it must capture credential_version at the moment of credential proof. -->
   **Where `auth_credential_version` is stamped (threaded grant → code/device_code → refresh issuance).** It is written **once, at the moment the user proves credentials**, by the handler that *mints the grant artifact*:
   - **Auth-code flow:** the `/authorize` handler (`crates/auth/src/op/authorization.rs`) stamps `auth_credential_version = users.credential_version` **as read at the instant of credential proof for this authentication** onto the `oauth_authorization_codes` row it issues. For an auth code minted from an *already-valid* SSO/login session with no fresh credential proof, the correct value is the **session's** captured `credential_version` (the value at that session's last credential proof), **not** `users.credential_version` re-read at code-issuance time — re-reading at issuance would mask a reset that landed mid-session. The token handler (`exchange_authorization_code`) then copies that column verbatim from the code row into the refresh-issuance path (§3 step 3), where it is the baseline for the under-lock re-check.
   - **Device-code flow:** the **device-approval** handler (the `/device` verification step where the user authenticates and approves, RFC 8628) stamps `auth_credential_version` from the `credential_version` proven *at approval* onto the `oauth_device_codes` (device_code) row; the device token-exchange handler carries it forward into refresh issuance identically.

   In both flows the value reflects `users.credential_version` **at the moment of credential proof** (login / device-verification), is threaded immutably grant → code/device_code → refresh issuance, and is never re-derived downstream — so the §3 re-check compares against the correct baseline. Why this is airtight against the bulk revoke (§7), in both interleavings:
   - *Issuance acquires `user-lock(U)` after the revoke* → the revoke has already committed and bumped `users.credential_version`; issuance reads the bumped value, the re-check fails, and **no stale-credential root is created**.
   - *Issuance acquires `user-lock(U)` before the revoke* → issuance commits a **live** root; the revoke then runs `UPDATE … WHERE user_id=$U AND revoked_at IS NULL`, which **covers that fresh root** and revokes it.
   The user-lock alone is insufficient (round-4 A-2: serializing issuance *behind* the revoke would still let it insert a fresh family under stale creds); the lock **plus** the under-lock `credential_version` re-check is what removes the hole.
4. `raw = "zrt_" + base64url(32 random bytes)` (`rand::RngCore`, mirror `generate_code` at `authorization_code.rs:634`). 256-bit entropy. No embedded metadata (P0 §2.4).
5. `family_id = "rfam_" + typed_id` — server-generated, fresh on every grant (a fresh login starts a fresh family). Never client-supplied.
6. `token_hash = HMAC-SHA256(refresh_hash_key[active].key, raw)`; record `hash_key_version = active`.
7. `family_absolute_expires_at = NOW() + 30 days`; `expires_at = min(NOW() + idle_window, ceiling)` (§6).
8. `INSERT` one row `{token_hash, hash_key_version, family_id, client_id, user_id, granted_scopes, family_granted_scopes, expires_at, family_absolute_expires_at}`. At the root, **`family_granted_scopes = granted_scopes`** — the scope the resource owner granted at consent (MINOR-E); it is the immutable ceiling for the whole family. No `rotated_at/revoked_at/idem_*`.
9. Return `raw` in `TokenResponse.refresh_token`. The raw value lives only in the HTTP response; the DB has only the hash. `COMMIT` releases the user-lock.

**Binding set:** `{family_id, client_id, user_id, granted_scopes, sub_snapshot}`. Pairwise `sub` is re-derived per rotation from `(user_id, client_id→sector)` for access-token minting; the refresh row persists the same derived snapshot only so family-kill and bulk credential-bump paths can write `token_revocations` without needing the in-memory issuer salt. `app_oauth_clients.sector_identifier` is immutable after insert, so the snapshot stays aligned with future mints. **No `nonce` is stored** (we omit `id_token` on refresh — §4 step 9).

Conformance: RFC 6749 §1.5, §5.1; OAuth 2.1 §1.3.2; OIDC Core (offline_access). Conformance-map "Refresh-token rotation: issue a new RT on each use."

---

## 4. Rotation — `exchange_refresh_token`

One transaction, serialized per family by an advisory lock so the multi-row family-kill can never deadlock (round-1 #5). Full algorithm (`op/refresh.rs`):

1. Parse form; **authenticate the client.** Public CLI: possession of the token + `client_id` (RFC 8252 §8.5). Confidential: verify `client_secret` against `oauth_clients.client_secret_hash`. On client-auth failure → `invalid_client`.
2. `BEGIN`. Compute `token_hash` (try each keyring version newest-first; `hash_key_version` on the matched row confirms). Read the row's `(refresh_family_id, user_id)` (cheap lookup). If no row → `invalid_grant` (`ROLLBACK`). **Take the two locks, outermost first** (the strict hierarchy that makes *every* writer deadlock-free, including the cross-family credential-bump revoke — round-3 MAJOR-A): first the per-user lock `SELECT pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))`, then the per-family lock `SELECT pg_advisory_xact_lock($NS_FAM, hashtext(refresh_family_id))`. <!-- Changed in round 3 (MAJOR-A): the round-2 fix took only the family lock, which the cross-family bulk revoke (§7) never queues on → it could still 40P01-deadlock rotation. Adding the OUTER user lock (taken by both rotation and bulk revoke) gives a total lock order user→family→row; proof in §5.4. --> The locks are transaction-scoped (`_xact_`), released on `COMMIT`/`ROLLBACK`.
3. Re-`SELECT` the presented row `FOR UPDATE` (the family lock already serializes the family; the row lock is belt-and-suspenders + gives `RETURNING` semantics). Read `rotated_at, revoked_at, expires_at, family_absolute_expires_at, family_granted_scopes, replaced_by_token_hash, idem_expires_at, client_id, user_id, granted_scopes`.
4. **Validity gate.** If `client_id`/`user_id` binding ≠ authenticated client, OR `revoked_at IS NOT NULL` on this row, OR **any** row in the family has `revoked_at IS NOT NULL` (family dead), OR `expires_at < NOW()`, OR `family_absolute_expires_at < NOW()` (ceiling) → `invalid_grant` (after the §5 replay/idempotency branch has had its chance).
5. **REUSE / IDEMPOTENCY BRANCH (the CVE core, §5).** If `rotated_at IS NOT NULL` on the presented row:
   - **Bounded-idempotency retry** (round-1 #10): if `idem_expires_at > NOW()` **AND** the successor row (`replaced_by_token_hash`) exists with `rotated_at IS NULL AND revoked_at IS NULL` (chain has **not** advanced) → this is a **legit lost-response retry**, not theft. **AEAD-open** `idem_response_enc` (verifying the AAD `token_hash‖refresh_family_id` binds it to *this* row), **re-mint a fresh access token** for the successor's scopes, `COMMIT`, and return `{access_token: fresh, refresh_token: <sealed successor raw>, scope, expires_in}`. **No rotation, no new row, no kill.**
   - **Genuine replay** (window elapsed, OR successor already rotated/revoked, OR no successor) → **family kill** (§5.2): revoke every live row in the family + write the `token_revocations` markers, `COMMIT`, return `invalid_grant`.
6. **Rotate (active token).** Atomic one-time consume, modelled on `V0063`'s `UPDATE … WHERE consumed_at IS NULL … RETURNING`:
   ```
   UPDATE zeroship.oauth_refresh_tokens
      SET rotated_at = NOW(), replaced_by_token_hash = $new_hash,
          idem_response_enc = $enc, idem_expires_at = NOW() + $idem_window
    WHERE token_hash = $presented_hash AND rotated_at IS NULL AND revoked_at IS NULL
    RETURNING refresh_family_id, client_id, user_id, granted_scopes, family_granted_scopes, family_absolute_expires_at;
   ```
   Zero rows returned ⇒ a concurrent rotation already consumed it (impossible under the family lock, but fail-closed) ⇒ §5 family-kill.
7. **Scope (RFC 6749 §6 — the ceiling is the *original* grant, MINOR-E).** `new_scopes = requested` if supplied, **validated `requested ⊆ family_granted_scopes`** (the immutable original-grant ceiling, *not* the predecessor's `granted_scopes`); a superset → `invalid_scope`. If `scope` is omitted, default to **`family_granted_scopes`** (RFC 6749 §6: "if omitted is treated as equal to the scope originally granted"). This means a client that requested a narrow subset on one rotation may **re-widen back up to** — never above — the original grant on a later rotation. The ratchet is removed: narrowing is per-token, never permanent to the family. (Practical note: narrowing *down* below the original is always allowed; the ceiling only blocks going *above* what the resource owner consented to.)
8. `INSERT` exactly one child: same `family_id, client_id, user_id`, `granted_scopes = new_scopes`, **`family_granted_scopes` copied verbatim** (original-grant ceiling, never changes), `family_absolute_expires_at` copied verbatim (ceiling never slides), fresh `expires_at = min(NOW()+idle_window, family_absolute_expires_at)`, `hash_key_version = active`. The partial unique index guarantees ≤ one live token; a second concurrent insert would violate it (and cannot happen under the family lock).
9. Mint a fresh access token (`issue_access_token`, `issuer.rs:243`) with pairwise `sub` **re-derived** (stable). **Do NOT mint an `id_token`** (round-1 #3): OIDC Core §12.2 makes it OPTIONAL on refresh, and if present its `nonce` MUST equal the original — which is not stored. Identity facts come from `/userinfo`.
10. Set `idem_response_enc` = `AEAD_seal(DEK, nonce, {successor_raw, expires_in, scope}, AAD = predecessor_token_hash ‖ refresh_family_id)` on the **predecessor** (done in step 6's UPDATE), so a lost response is recoverable in step 5 without re-rotating. The DEK is file/KMS-held, never in Postgres (MINOR-D / T6).
11. `COMMIT` (releases the advisory lock). Return `{access_token, refresh_token: new_raw, scope, expires_in}`.

**Scope on rotation is narrowing-only** (step 7) — closes scope-escalation (T3).

Conformance: OAuth 2.1 §4.3, §6.1; RFC 9700 §4.13, §4.14, §2.2.2; RFC 6749 §6; conformance-map §4 "issue a new RT on each use, invalidate the old one."

---

## 5. Reuse detection + family kill — the highest-CVE-density surface

### 5.1 The detection predicate

A presented refresh token is a **candidate replay** iff its row has `rotated_at IS NOT NULL` or `revoked_at IS NOT NULL`. A candidate replay is a **benign retry** iff (a) the idempotency window is open (`idem_expires_at > NOW()`) and (b) the chain has not advanced (the successor is still the family's single live token). Otherwise it is a **genuine replay** → family kill. Detection is exact column reads under the family advisory lock + `FOR UPDATE`, not a heuristic.

RFC 9700 §4.14: "If a refresh token is compromised … the authorization server cannot distinguish between a malicious actor and the legitimate client … the authorization server MUST revoke all tokens of that refresh-token family." §4.13/§4.14 also acknowledge that automatic reuse detection has **false positives** from legitimate concurrency / lost responses; bounded idempotency (§5.3) is the standards-blessed way to absorb them without weakening theft detection.

### 5.2 The kill — and what it actually invalidates

On genuine replay (or any explicit revoke), revoke **every live row** sharing `refresh_family_id`:

```
UPDATE zeroship.oauth_refresh_tokens SET revoked_at = NOW()
 WHERE refresh_family_id = $1 AND revoked_at IS NULL;
```

AND write the access-token kill markers. Because `token_revocations.sub` is the **per-(client, sector) pairwise subject**, the fan-out is **one marker per `(client_id, pairwise_sub)`** the user has a live family in (round-1 #8): for a single-client family kill that is one row; for a credential-bump kill (§7) it is one row per distinct `client_id` in the user's live families:

<!-- Corrected in round 5 (MINOR-1): the column is `sub`, not `token_subject`. Verified against db/migrations/V0002__auth.sql:228 (PRIMARY KEY (client_id, sub)) and the live writer crates/control/src/oauth_grants_handlers.rs:240. -->
```
INSERT zeroship.token_revocations(client_id, sub, revoked_after)
  VALUES ($client_id, derive_pairwise(AUTH_PAIRWISE_SALT, $user_id, sector($client_id)), NOW())
  ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after;
```

**Pairwise coherence contract:** the OP mint derives the subject from `AUTH_PAIRWISE_SALT` + `app_oauth_clients.sector_identifier`, and refresh rows persist that derived subject as a revocation-marker snapshot. They cannot disagree for a client because there is exactly one salt and `app_oauth_clients.sector_identifier` is immutable after insert (`V0064` trigger). This is asserted by `pairwise_sub_stable_and_equal_across_mint_and_revoke_check`.

<!-- Rewritten in round 3 (MAJOR-B): the round-2 text falsely claimed BOTH the gateway and control plane already consult token_revocations. Verified FALSE for control (oauth_grants_handlers.rs:240 only WRITES the marker). Below states exactly who reads it, when, and the residual. -->
**Who enforces it for the CLI face — precise, not aspirational** (round-1 #5 / round-3 MAJOR-B): a killed family's *refresh* token dies immediately (next rotation sees `revoked_at`). Already-minted **access tokens** (`at+jwt`, 900s) are bearer and self-contained, so they remain valid until they expire **unless** the resource server reads `token_revocations`. The two resource servers on the CLI path differ **today**:

- **(a) Gateway** (proxying CLI/programmatic app calls): **already enforces.** It calls `revoked_after_for(conn, client_id, sub)` (`auth_token.rs:1156`, `router/auth.rs:123`, `core/src/wrapper_revocation.rs:70`) and rejects an `at+jwt` whose `iat` predates the marker. No new work.
- **(b) Control plane** (the `zeroship deploy` / admin RS — the **headline** CLI face): **does NOT enforce today.** Verified: `crates/control/src/oauth_grants_handlers.rs:240` only **writes** `token_revocations`; there is no `revoked_after_for` read on the control request path (`token_handlers.rs` has no revocation check). So a revoked deploy family's `at+jwt` keeps pushing code for up to its 900s TTL. **This is a build gap, not existing behavior.**

**The fix (P5b-7, recommended default — see Open operator decision #1):** add a control-plane revocation read on the authenticated request path. The control bearer guard (`oauth_guard_from_bearer`, or the control authz guard) — after verifying the `at+jwt` signature and extracting `client_id`, the pairwise `sub`, and `iat` — calls `revoked_after_for(client_id, pairwise_sub)` **served from a short-TTL in-process cache** (recommend 10s), and rejects (`401`) when `iat < revoked_after`. With this, a killed CLI/deploy family stops authorizing within **roughly the cache TTL (~10s)**, not ≤900s.

<!-- Corrected in round 5 (MINOR-3 / B-1): the residual is NOT exactly "= cache TTL". -->
**Honest residual on this read (B-1, clock skew + `iat` granularity).** The true residual is `max(cache_TTL, clock_skew + iat_granularity)`, **not** "= cache TTL". `iat` is stamped by the OP/issuer clock while `revoked_after` is stamped by the revoke writer's clock; positive mint-clock skew (mint clock ahead of revoke clock) lets a token minted *just before* a revoke carry `iat > revoked_after` and slip the `iat < revoked_after` test, and `iat` is 1-second-granular (JWT `NumericDate`) while `revoked_after` is sub-second. This is the **same** property the existing gateway check already has (`wrapper_revocation.rs:100`), so P5b-7 adds no new weakness — but the prose must say "cache TTL **plus** NTP skew / 1s `iat` granularity," not "= cache TTL." Mitigation is operational: NTP-discipline the OP and control/gateway clocks; the residual is then bounded by the skew budget (sub-second to a few seconds).

<!-- Added in round 5 (MINOR-3 / B-2): PATs are a separate credential path. -->
**PATs are NOT covered by this read (B-2).** `zeroship deploy` also authenticates with PATs / `ZEROSHIP_TOKEN` (AGENTS.md), which are **not** `at+jwt`s minted from a refresh family and carry **no** `(client_id, pairwise_sub, iat)` against which `token_revocations` is keyed. Killing a refresh **family** therefore does **not** stop a PAT-authenticated deploy — PAT revocation is a **separate** credential path (its own store + revoke), out of scope for P5b. An operator must not assume "kill family ⇒ deploy stops" for a PAT-authenticated deploy; that is a distinct story, named here so the gap is explicit, not silent.

Any *future* RS that reads neither the marker nor caps the access-token TTL re-opens a ≤900s window **for itself** — an explicit RS obligation, not a platform guarantee. The residual is therefore `max(cache_TTL, skew+iat_granularity)` at both the gateway and (once P5b-7 lands) the control plane, for the `at+jwt` path only.

### 5.3 The race: legit lost-response retry vs attacker replay — bounded idempotency

Two presentations of the **same** active or just-rotated token can occur:

- **Legit lost-response retry** — the canonical false positive (round-1 #10): the client sends refresh, the OP rotates + responds, the response is dropped in transit, the client retries the *same* token. This is the dominant benign case for the CLI (which has no single-flight).
- **Attacker replay** — a stolen token presented after the legit holder rotated.

We distinguish them with **bounded idempotency** (Stripe idempotency-key pattern; Auth0/Okta/Duende all expose a small rotation leeway for precisely the lost-response retry; RFC 9700 §4.14 false-positive allowance):

1. On rotation, the predecessor row caches `idem_response_enc = AEAD_seal(DEK, nonce, {successor_raw, expires_in, scope}, AAD = token_hash || refresh_family_id)` and `idem_expires_at = NOW() + idem_window` (recommend **30s**, ≥ a couple of network RTT + client retry budget). The cache holds raw successor material → **AEAD-sealed** (XChaCha20-Poly1305 or AES-256-GCM) under a file/KMS-held DEK (sibling custody to `refresh_hash_key`, never in Postgres), AAD-bound to `(token_hash, refresh_family_id)` so a dumped ciphertext cannot be relocated onto another row, fenced to `zeroship_auth`. <!-- Changed in round 3 (MINOR-D): "encrypt(...)" → explicit AEAD with AAD binding + named DEK custody; T6 reconciled in §2.2/T6. --> **Logical vs physical dwell (corrected round 5, MINOR-4/D-1):** `idem_expires_at` is the *logical* replay window (≤30s — after it, a retry kills); but the ciphertext **physically dwells** in the row until a reaper clears it. The reaper is therefore split (§6.1): a **prioritized idem-reap pass** (recommend ~60s) NULLs `idem_response_enc`/`idem_expires_at` once `idem_expires_at < NOW()`, *decoupled* from the heavier 5–15 min family-delete pass — so physical dwell is **≤ the idem-reap cadence**, not ≤30s and not ≤15 min. Each not-yet-reaped successor, if decrypted (requires the DEK **and** a DB dump), is a **live chain-head refresh token** usable until it is itself rotated/killed — which is why the idem-reap must be frequent and is prioritized over family-delete.
2. A retry presenting the predecessor (now `rotated_at IS NOT NULL`) within the window **and** whose successor is still the family's single live token → the OP **replays the cached response**: same successor refresh token, a freshly-minted access token. No rotation, no new row, no kill. The client recovers transparently.
3. If the window has elapsed **OR** the chain has advanced (the successor was itself rotated/revoked — i.e. the legit client already moved on) **OR** there is no successor → genuine replay → **family kill**.

Why this is safe: the moment the legit client successfully uses the successor, the chain advances and any later presentation of the predecessor is a guaranteed kill — so the idempotency window is effectively the *shorter* of 30s and "until the chain advances". The residual that remains is intrinsic and named by RFC 9700 §4.14: an attacker who replays the predecessor *within* the window and *before* the legit client uses the successor obtains the same successor the legit client holds — indistinguishable from a lost-response retry by construction. The window is small and the family ceiling bounds the blast radius; family-kill-rate alerting (§9.1) surfaces anomalies.

**No unbounded grace, no "accept-and-re-rotate".** We never issue a *second* live successor (that would violate `one_active_per_family` and create a fork). Idempotency replays the *existing* successor; it never rotates again.

> The round-1 browser reload storm that motivated rejecting all grace **no longer exists** (Option B: no browser refresh). The remaining benign case is the CLI lost-response retry, which bounded idempotency handles correctly.

### 5.4 The advisory-lock hierarchy (deadlock-free for *every* writer) — round-1 #5 + round-3 MAJOR-A

<!-- Rewritten in round 3 (MAJOR-A): the round-2 design took only a per-family lock, which the cross-family credential-bump bulk revoke (§7) never queues on, so rotation-vs-bulk-revoke could still 40P01-deadlock — the exact T9 race. The fix is a strict TWO-LEVEL hierarchy. -->

Every transaction that writes a refresh row acquires advisory locks in **one fixed total order**:

```
user-lock(user_id)   →   family-lock(refresh_family_id)   →   Postgres row locks (FOR UPDATE / UPDATE)
  pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))
  pg_advisory_xact_lock($NS_FAM,  hashtext(refresh_family_id))
```

`$NS_USER` and `$NS_FAM` are **distinct** fixed namespace constants passed as the two-arg `pg_advisory_xact_lock(int4, int4)` form, so the user-lock keyspace and family-lock keyspace never collide.

<!-- Added in round 5 (MINOR-2): the existing helper cannot express this; specify the net-new one. -->
**The advisory helper this needs is net-new (MINOR-2).** The existing `crates/auth/src/advisory_lock.rs` exposes only a **session-scoped, single-i64-key** form (`pg_advisory_lock($1)` / manual `pg_advisory_unlock`). §5.4 needs a **transaction-scoped, two-arg** form. Add `with_xact_advisory_lock2(conn, ns: i32, key: i32)` issuing `SELECT pg_advisory_xact_lock($1, $2)` (`int4, int4`) on the **same connection that does the writing** (advisory locks are per-session; the lock and the `UPDATE`/`INSERT`/`DELETE` must share the physical connection). It needs **no** explicit unlock — `pg_advisory_xact_lock` is released automatically at `COMMIT`/`ROLLBACK` (PostgreSQL: "there is no provision for manual release" of transaction-level advisory locks), which is exactly why a writer that aborts cannot leak a held lock. The two namespaces are compile-time `i32` constants; the keys are `hashtext(user_id::text)` / `hashtext(refresh_family_id)` (both already `int4`).

**Writer-coverage table — EVERY *application* refresh-row writer (round 5 closes A-1/A-2; round 7 scopes the universal — NEW-1).** An "application refresh-row writer" is any statement **issued by `crates/auth`** that `INSERT`/`UPDATE`/`DELETE`s a `zeroship.oauth_refresh_tokens` row. All six take `user-lock(U)` before touching any of `U`'s rows. (The one writer that is *not* an application statement — the Postgres FK `ON DELETE CASCADE` machinery on a hard user/client delete — is a separate ADMIN path explicitly carved out below; it cannot take an advisory lock, and its worst case is a clean transient 5xx, never an oracle / escaped family / spurious logout.)

| Writer | user-lock(U) first? | family-lock(F)? | row-tier work |
| --- | --- | --- | --- |
| **Rotation** (`exchange_refresh_token`, §4) | **yes** (step 2) | yes (step 2) | `FOR UPDATE` on the presented row + multi-row kill on replay |
| **Issuance** (root mint, §3 — *added to the table in round 5, A-2*) | **yes** (§3 step 2) **+ `credential_version` re-check under the lock** (§3 step 3) | no — fresh `family_id`, single `INSERT` (a legal *prefix* of the order: user → row) | one `INSERT` (+ FK `FOR KEY SHARE` on `users(U)`) |
| **Single-family revoke** (`/revoke`, §7) | **yes** | yes | family kill `UPDATE` |
| **Credential-bump / password-reset bulk revoke** (§7, cross-family) | **yes** | n/a (no single family) | set-based `UPDATE … WHERE user_id=$U AND revoked_at IS NULL` |
| **Sweeper / reaper** (§6.1 — *added to the table in round 5, A-1*) | **yes** (per-user batch: one `user-lock(U)` per user, one txn) | n/a | set-based `DELETE … WHERE user_id=$U AND <sweepable>` |
| **Idempotency-cache write** (§5.3) | **yes — via rotation** (it is the *same* `UPDATE` in §4 step 6, inside the rotation txn that already holds `user-lock(U)`+`family-lock(F)`) | yes — via rotation | no separate statement |

Issuance and the bulk revoke and the sweeper take a **prefix** of the total order (user-lock, then straight to the row tier, skipping the family tier). Skipping a *later* tier is legal ordered-locking; the only rule is **never acquire a later-tier lock before an earlier-tier one**, and no writer does. The idem-cache write takes no lock of its own because it executes *inside* the rotation transaction.

**Why this is deadlock-free (proof — now genuinely universal).** Deadlock requires a cycle in the wait-for graph. Order all lockable objects by the total order above (user-locks `<` family-locks `<` rows). Every writer acquires locks in strictly ascending order, so within the *advisory* tiers there can be no cycle (classic ordered-locking; advisory locks live in the same lock manager and participate in the deadlock detector, so this ordering is load-bearing, not cosmetic). The only remaining tier is **rows**. Claim: **no two transactions ever hold conflicting row locks on the same user's refresh rows simultaneously.** Proof: by the table above, **every** statement that writes any row of user `U` — rotation, issuance, single-family revoke, bulk revoke, **and the sweeper** — first holds `user-lock(U)`; `user-lock(U)` is exclusive, so at most **one** transaction at a time is past it, hence at most one transaction is ever in `U`'s row tier. With a single transaction in `U`'s row tier there is no second party to wait on → no row-tier cycle, hence **no `40P01` on any refresh path written by an application writer.** ∎

The universal now holds across the **application** writer set because that set is **closed**: there is no seventh *application* writer. The round-4 counterexamples are both gone — the sweeper (A-1) and issuance (A-2) are in the table and take `user-lock(U)` like everyone else. In particular the producible round-4 race — sweeper `DELETE` vs bulk-revoke `UPDATE` both touching `U`'s live-but-past-ceiling rows in different scan orders — cannot form: the second of the two to want `user-lock(U)` blocks on the *advisory* lock (no row lock held yet), so they run strictly one-after-the-other, never interleaved at the row tier.

<!-- Added in round 7 (NEW-1): the FK ON DELETE CASCADE machinery is a refresh-row DELETE writer that cannot take an advisory lock; carved out of the application-writer universal as a narrow ADMIN path. Option (a) — keep CASCADE, document honestly. -->
**The one writer outside the universal — FK `ON DELETE CASCADE` (NEW-1, ADMIN path, carved out by design).** The §2.1 DDL declares `user_id … REFERENCES zeroship.users(id) ON DELETE CASCADE` and `client_id … REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE`. A **hard** `DELETE FROM zeroship.users WHERE id=$U` (GDPR erasure) or `DELETE FROM zeroship.oauth_clients WHERE client_id=$X` (creator deletes an integration) cascades into `oauth_refresh_tokens` **without** taking `user-lock(U)` — the cascade is executed by Postgres's FK machinery, which has no hook to call `with_xact_advisory_lock2`. So the cascade *can* form a row-tier `40P01` cycle against a concurrent rotation / bulk-revoke on the same user/client. We keep `ON DELETE CASCADE` (it is the correct referential-integrity behavior for hard deletes) and carve it out of the universal explicitly rather than weaken the schema:
> - **Scope of the universal.** The "no `40P01`" guarantee covers **every application writer** (the six in the table). It does **not** extend to the FK-cascade hard-delete, which is a distinct, low-frequency **ADMIN** path (account erasure / app deletion), not a request-path refresh operation.
> - **Worst case is benign.** If the cascade and an application writer deadlock, one side gets a clean, transient `40P01` serialization failure. The application victim (rotation / revoke) maps a transient abort to **5xx, never `invalid_grant`** (§1, §5.4) — so there is **no oracle, no escaped family, no spurious user-facing logout, no security regression**. It is a liveness nit on an admin operation, retried by the admin caller.
> - **Usually a no-op.** Application-level account/client deletion **SHOULD revoke the user's (or client's) refresh families under `user-lock(U)` first** (the §7 bulk-revoke / `/revoke` path) **before** issuing the parent `DELETE`. After that revoke commits, the rows are already `revoked_at`-stamped and out of the live set, so the subsequent cascade is an ordinary cleanup `DELETE` with no concurrent application writer contending for them — the cascade is effectively a no-op and the window for even the benign `40P01` shrinks to near-zero. Zeroship's documented account path is in any case **soft** (`users.disabled_at`, §7), so the user cascade is off the live path today; hard deletes are the rare admin/erasure case.

This also kills the original round-2 race: rotation in family F2 (holding `user-lock(U)`, `family-lock(F2)`, `FOR UPDATE` on F2.r1, wanting F2.r2) cannot collide with a concurrent bulk revoke, because the bulk revoke cannot acquire `user-lock(U)` until rotation commits. The T9 `password_reset_during_rotation` scenario is deadlock-free.

**`hashtext` collisions degrade to serialization, never deadlock (MINOR-5).** `hashtext` maps to a 32-bit `int4`, so two distinct `user_id`s can collide on the user-lock key (and two `family_id`s on the family-lock key); at scale collisions are expected. The consequence is purely **performance** — two unrelated users briefly serialize on one advisory key — **never** a deadlock, because every path takes the (possibly-colliding) advisory key *before* any row lock, so no transaction ever holds a row lock while waiting on an advisory key. The user-lock tier is therefore coarser than strict per-user at scale; acceptable given low per-user refresh-write concurrency. (A wider keyspace, e.g. folding the UUID into the two `int4` args directly instead of `hashtext`, is an available future tightening if collision-serialization ever shows up in the §9.1 metrics.)

**Correctness bonus (no escaped family) — now complete for roots too (A-2).** Round 4 noted the round-3 "no escaped family" argument covered only rotation-created **children**, not issuance-created **roots**. With issuance under `user-lock(U)` **and** the under-lock `credential_version` re-check (§3 step 3), a bulk revoke holding `user-lock(U)` is guaranteed that (a) no child can be created mid-revoke (rotation is queued on the lock), and (b) no **root** minted on stale credentials can slip past it — in the after-revoke interleaving issuance reads the bumped `credential_version` and refuses; in the before-revoke interleaving the root is live when the revoke's `WHERE user_id=$U AND revoked_at IS NULL` runs and is caught. The "a family created mid-reset survives the bump" hole is closed for **both** roots and children.

**No `40P01` leaks (guarantee reconciled — universal over the application writer set, FK cascade carved out — NEW-1).** Since the **application** writer set is closed and every member is covered by the hierarchy, **no `40P01` deadlock abort is producible on any refresh path written by an application writer.** The "deadlock abort surfaces as a non-`invalid_grant` 5xx" oracle leak therefore cannot happen for any application writer. Every protocol outcome is `invalid_grant` / `invalid_client` / success; a genuinely transient failure (connection drop, statement timeout) maps to 5xx, never `invalid_grant`. This is the §5.4 / T10 guarantee, complete for every application writer including the credential-bump, issuance, and sweeper paths that earlier rounds missed. The **sole** path outside this universal is the FK `ON DELETE CASCADE` admin hard-delete (carved out above): even there the worst case is a clean transient 5xx — never `invalid_grant`, never an escaped family — and app-level deletion is specified to revoke the family under `user-lock(U)` first so the cascade is usually a no-op.

**Concurrency cost (honest).** The outer user lock serializes a single user's concurrent refresh-row writers (a laptop CLI rotating while a confidential `offline_access` client rotates, for the *same* user; the periodic sweeper holding `user-lock(U)` for that user's brief delete batch). Per-user refresh-write concurrency is genuinely low and each lock is held only for the brief writing transaction, so this is acceptable; it does **not** serialize across users (different `user_id` → different lock, modulo the `hashtext` collisions noted above). The sweeper's per-user batching means it never blocks the whole table — only one user's rows at a time, briefly.

### 5.5 Ordering note (fail-closed)

The family-already-dead check (step 4) and the replay/idempotency branch (step 5) both end observationally identically to the client. Internally, evaluate the step-5 branch when `rotated_at IS NOT NULL` even if the family is also revoked — the kill `UPDATE` is idempotent (`WHERE revoked_at IS NULL` matches nothing on a second pass).

Conformance: RFC 9700 §4.13, §4.14, §2.2.2; OAuth 2.1 §6.1; conformance-map §3 A6, §4 "on reuse of a retired RT revoke the entire token family", §5 MUST-NOT-cut.

---

## 6. Lifetimes — sliding idle window + absolute family ceiling (CLI only)

Under Option B there is **no gateway anchor clock** for any refresh family — the browser face has no refresh family at all (§8). The round-1 "three 30d clocks" problem is gone. The CLI/confidential family has exactly **two** clocks, both in the OP store, both enforced by the OP:

| Clock | Value | Slides? | Enforced by |
| --- | --- | --- | --- |
| Per-token **idle** `expires_at` | `issued_at + idle_window` (OQ-5: recommend **7d** sliding under the 30d ceiling for a tighter theft window on idle CLIs; 30d = ceiling collapses to one clock) | yes, reset on each rotation | step 4 (`expires_at < NOW()` → `invalid_grant`) |
| Family **absolute** ceiling `family_absolute_expires_at` | root `issued_at + 30d` | **never** | step 4 + copied verbatim to every child (step 8) |

Re-auth (fresh device/code grant) is the only way past the ceiling → fresh family, new root, new ceiling. Rotation can never extend it (step 8 copies verbatim) — closes T8.

Conformance: RFC 6749 §1.5; RFC 9700 §4.14 (long-lived refresh tokens MUST be rotated — satisfied).

### 6.1 Sweeper — round-1 #15, brought under the §5.4 hierarchy in round 5 (A-1)

Owner: `zeroship_auth` (the only role with `DELETE`). The sweeper is a **refresh-row writer** (it `DELETE`s, and a `DELETE` takes tuple locks that conflict with the bulk revoke's `FOR NO KEY UPDATE`), so round 4 (A-1) correctly flagged that the round-3 sweeper — which took **no** advisory lock — could `40P01`-deadlock the credential-bump bulk revoke when both touched a user's live-but-past-ceiling rows in different scan orders. Round 5 puts the sweeper on the **same total order as every other writer**.

<!-- Rewritten in round 5 (CRITICAL A-1): the sweeper now takes user-lock(U) per-user batch, so it is a first-class member of the §5.4 hierarchy and can no longer deadlock the bulk revoke. -->
**Two decoupled passes, both per-user under `user-lock(U)`:**

**Pass 1 — family-delete (heavy, recommend every 5–15 min).** Reuse the existing auth reaper schedule that sweeps `oauth_authorization_codes`; same crate. It runs **per user**, not as one table-wide `DELETE`:

1. Enumerate the distinct `user_id`s that have sweepable rows (a read; takes no conflicting lock):
   `SELECT DISTINCT user_id FROM zeroship.oauth_refresh_tokens WHERE family_absolute_expires_at < NOW() OR (rotated_at IS NOT NULL AND <past retention horizon>)`.
2. For **each** such `user_id`, in **its own short transaction** on one connection: `SELECT pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))` **first**, then one set-based delete of *that user's* sweepable rows:
   `DELETE … WHERE user_id = $U AND (family_absolute_expires_at < NOW() OR (rotated_at IS NOT NULL AND issued_at < NOW() - $retention))`, then `COMMIT` (releases the user-lock). Retention horizon e.g. **24h** — keep recently-rotated rows briefly for audit/forensics.

Because each batch holds `user-lock(U)` before deleting any of `U`'s rows, the sweeper is exactly the same shape as the bulk revoke and is covered by the §5.4 proof: at most one transaction is ever in `U`'s row tier, so the sweeper-vs-bulk-revoke `40P01` (round-4 A-1) cannot form. The sweeper never holds more than one user-lock at a time (one user per transaction), so it introduces no cross-user lock-ordering question.

**Pass 2 — idem-reap (light, prioritized, recommend ~60s — corrects MINOR-4/D-1).** A separate, **more frequent** pass that NULLs the encrypted-secret columns as soon as the logical window closes, decoupled from the heavy 5–15 min delete so the **physical** ciphertext dwell ≈ this cadence, not the 15-min family-delete interval:

- Per user under `user-lock(U)` (same batching as Pass 1):
  `UPDATE … SET idem_response_enc = NULL, idem_expires_at = NULL WHERE user_id = $U AND idem_response_enc IS NOT NULL AND idem_expires_at < NOW()`.
- This is **prioritized** because every not-yet-reaped `idem_response_enc`, if the DEK is also compromised, decrypts to a **live chain-head refresh token** (usable until rotated/killed) — so the at-rest exposure window under (DEK + DB dump) is bounded by *this* cadence (§2.2 / T6 residual). Run it on the tightest schedule the reaper budget allows.

Both passes only ever write **non-live** tuples (Pass 1 deletes expired/over-retention rows; Pass 2 only NULLs idem columns on already-`rotated` rows). Interaction with the partial unique index is therefore **benign**: the index only covers `rotated_at IS NULL AND revoked_at IS NULL` (live) rows, and neither pass touches a live tuple in a way that could transiently admit two live tokens.

---

## 7. Revocation — RFC 7009 `/revoke` + the teardown fan-out

`/revoke` is advertised in discovery but currently missing. P5b lands it (shared with P0 §1.8). Refresh-side behavior:

1. `POST /revoke` accepts `token`, optional `token_type_hint`, plus client auth (confidential) / `client_id` (public). RFC 7009 §2.1.
2. **Client-binding check (round-1 #13, RFC 7009 §2.1):** resolve the refresh-hash to a row, then verify the row's `client_id` equals the **authenticated** client. If it was issued to a *different* client → return **200 without revoking** (no cross-client DoS, no oracle). Only on a match → **family kill**: `UPDATE … SET revoked_at=NOW() WHERE refresh_family_id=$1 AND revoked_at IS NULL` + the `token_revocations` marker(s) (§5.2), taken **under the `user-lock → family-lock` hierarchy** (§5.4) — `pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))` then `($NS_FAM, hashtext(refresh_family_id))` — so a single-family `/revoke` cannot deadlock a concurrent rotation or bulk revoke either.
3. Return **HTTP 200 even for an unknown/already-invalid/foreign token** (RFC 7009 §2.2 — uniform, no oracle).

**Family revoke is also triggered by:**

- **Password change / credential bump (the T9 path — round-3 MAJOR-A):** when `zeroship.users.credential_version` is bumped, **acquire `user-lock(user_id)` first** (`pg_advisory_xact_lock($NS_USER, hashtext(user_id::text))`, the outermost lock of the §5.4 hierarchy), then revoke **all** refresh families for that `user_id` (across clients) as a single **set-based** statement `UPDATE … SET revoked_at=NOW() WHERE user_id=$U AND revoked_at IS NULL` (no per-family lock, no `FOR UPDATE`), + write **one `token_revocations` row per distinct `(client_id, pairwise_sub)`** in those families (§5.2 fan-out). Holding `user-lock(U)` for the whole bump makes it **deadlock-free against any concurrent refresh-row writer** — rotation, issuance, single-family `/revoke`, and the sweeper all take the same `user-lock(U)` before touching a row (§5.4 proof, round-5 closed writer set). No family created mid-reset escapes the revoke: a child cannot be created mid-bump (rotation is queued on the lock), and a **root** cannot be minted on stale credentials (issuance re-checks `credential_version` under the same lock and refuses if it was bumped — §3 step 3, round-5 A-2). This closes the password-reset TOCTOU for **both** roots and children: a leaked refresh token dies on reset (the family's next rotation sees `revoked_at`), and live access tokens die via the per-client markers (enforced at the gateway today and at the control plane via P5b-7 — §5.2). The browser/console face is covered separately and automatically: its server session is invalidated the instant `credential_version` no longer matches (`store/sessions.rs::validate` clause `credential_version = users.credential_version`; §8).
- **Logout / RP-initiated end-session** and **Back-Channel Logout** (P5d): revoke the refresh families for `(client_id, user_id)`.
- **Account disable** (`users.disabled_at`), **explicit grant revoke** (delete `oauth_grants` row), **admin session termination**.

Conformance: RFC 7009 §2.1, §2.2; OIDC Back-Channel Logout (P5d).

---

## 8. The browser / console face — bounded session + JIT mint (contract; build = P5c)

<!-- Rewritten in round 2 (OQ-1 → Option B; addressing CRITICAL #1, CRITICAL #2, MAJOR #4, MAJOR #7). The browser face has NO refresh family. do_refresh is removed, not swapped. -->

Option B retires the OAuth-refresh anchor for the browser face entirely (P0 §446/§551). This spec defines only the **contract**; the gateway-side implementation is **P5c (the gateway re-home)**.

### 8.1 The model

<!-- Rewritten in round 3 (MAJOR-C): the round-2 text reused the IdP LOGIN session (12h/30-min) verbatim for the console face — a silent regression from the retired 30-day no-idle browser anchor. We now specify a DEDICATED console/app session policy. -->
- The gateway (trusted BFF, co-located with the IdP) holds a **server-authoritative session** for the browser/console face. It **reuses the session *machinery*** (`store/sessions.rs::validate`: slides idle on each use; rejects when `revoked_at IS NOT NULL`, when `credential_version ≠ users.credential_version`, or when either clock has passed) but is a **distinct session kind with its own lifetimes** — it is **NOT** the short interactive IdP login session (`sessions/login.rs:14-15`, 12-h absolute / 30-min idle), which stays as-is for the login UI itself. Reusing the login session verbatim would silently regress the retired browser anchor (`anchors.rs:57`: 30-day absolute, **no idle slide**) into a console that forces re-login every 12h and logs out after 30 min idle — a drastic, unacceptable UX regression for a developer console (long deploys, stepping away mid-task). The **console session** therefore carries its own `idle_expires_at` (sliding), `abs_expires_at` (fixed), `credential_version`, `revoked_at`.
  - **Recommended default (Open operator decision #2): 30-day absolute ceiling + 7-day sliding idle**, plus **step-up re-auth** for sensitive actions (rotate/reveal a secret, delete an app, change billing/payout) regardless of session age. Rationale: 30-day absolute preserves the retired anchor's reach (the weekly-active developer is effectively never logged out before the ceiling); the 7-day idle *replaces* "no idle ever" so an abandoned console dies within a week (strictly safer than the old anchor, vastly better than 30-min); step-up bounds the damage of a hijacked long-lived session to read-ish operations. The exact numbers are operator-tunable — see § Open operator decisions.
  - **This is the P5c console contract.** P5c builds the session kind + lifetimes + step-up gate against these values; it does not silently inherit the login session.
- **Reload recovery** = `session_store::validate(session_id)` + **JIT mint** of a short-lived access token from that live session, **on the request path**. No refresh token, no rotation, no cron, no `app_session_anchors` blob.
- **Instant revocation:** bump `revoked_at` (logout/admin) or `credential_version` (password reset) → the very next `validate` fails closed → 401 → re-auth. This is strictly *stronger and simpler* than refresh-family revocation: there is no opaque token to leak, no family to walk, no cross-store marker to propagate.

### 8.2 What changes in the gateway (P5c, not this build)

- `crates/gateway/src/auth_token.rs::do_refresh` and the Hydra `refresh_token_public` call are **removed** — there is no browser OAuth refresh to perform. (This is why round-1 CRITICAL #2 dissolves: there is no public-vs-confidential `do_refresh` to reconcile, and no per-app `client_secret` to custody.)
- `crates/gateway/src/anchors.rs` (the `app_session_anchors` rotation store, single-flight, breadcrumb) is **retired for the browser face**, per P0 §551. No "anchor death ⇒ OP `/revoke`" wire is needed (round-1 #7) because there is no anchor and no OP family for this face.

### 8.3 Why no orphans, no storm (round-1 #4, #7 dissolved)

There is no second store and no second clock for the browser face: the session *is* the state. A reload storm produces N concurrent `validate` calls, which are **idempotent** (they only slide `idle_expires_at` forward and read) — they never rotate a single-use credential, so they can never trigger a family kill or a forced logout. The cross-store orphan (anchor dies on its own clock while an OP family lives) cannot occur because neither an anchor nor an OP family exists for this face.

### 8.4 Boundary acceptance

The CLI/confidential `/token` refresh path (this spec) and the browser session path (P5c) share **only** the IdP user/session/`credential_version` substrate and `token_revocations` semantics. They do not share a refresh family, an anchor, or a rotation algorithm. Acceptance: deleting `crates/gateway/src/anchors.rs` + `do_refresh` must not affect any test in §11 (those exercise the OP `/token` path directly), proving the two faces are decoupled.

---

## 9. Threat model

| # | Threat | Mitigation | Gating test |
| --- | --- | --- | --- |
| T1 | **Refresh token theft + replay** | Rotation makes the stolen token single-use; genuine replay (window elapsed or chain advanced) hits §5 → family kill. **Residual (named, RFC 9700 §4.14):** if the legit holder is *idle*, a thief who steals the live token can rotate `N+1'→N+2'→…` silently up to the 30d ceiling; this is intrinsic to bearer rotation. Bounded by the family ceiling + surfaced by family-kill / unusual-rotation alerting (§9.1). | `refresh_replay_after_rotation_kills_family` ; `idle_victim_theft_bounded_by_ceiling` |
| T2 | **Family fixation** | `family_id` server-generated at issuance (§3 step 3), never client-supplied; reuse keys on the row's family. | `refresh_family_id_is_server_generated_not_client_controlled` |
| T3 | **Scope escalation on rotation** | Step 7: `requested ⊆ family_granted_scopes` (the **original** grant ceiling, copied verbatim per child — MINOR-E); superset → `invalid_scope`; omitted → original grant (RFC 6749 §6). Per-token narrowing is allowed and **reversible** up to — never above — the original grant; no permanent ratchet. | `refresh_rotation_rejects_scope_above_original_grant` ; `refresh_rotation_narrowing_is_reversible_within_original_grant` |
| T4 | **Sector / pairwise leakage** | Row binds `client_id`; pairwise `sub` is derived per `(user_id, sector)` from the single salt/sector source and persisted as a revocation snapshot under an immutable sector contract (§5.2) — **stable across rotations, never crossed between sectors**. | `refresh_token_bound_to_client_id` ; `pairwise_sub_stable_and_equal_across_mint_and_revoke_check` |
| T5 | **Rotation race / lost-response false kill** | Family advisory lock serializes the family (§5.4, deadlock-free); bounded idempotency (§5.3) recovers benign retries; genuine replay kills. | `concurrent_refresh_same_token_one_rotates_other_recovers_or_kills` ; `legit_lost_response_retry_recovers_without_family_kill` ; `one_active_per_family_unique_index_enforced` ; `family_kill_does_not_deadlock_under_concurrent_replays` |
| T6 | **DB compromise at rest** | **At-rest invariant (stated precisely — MINOR-D):** token *verifiers* are one-way `HMAC-SHA256(refresh_hash_key[version], token)`; the **only** raw secret ever at rest is the bounded-idempotency successor, **AEAD-sealed** (XChaCha20-Poly1305/AES-256-GCM, file/KMS DEK, AAD = `token_hash‖refresh_family_id`) for ≤`idem_window` and swept. Key material is never in Postgres. A **DB-only dump yields zero usable tokens** (HMAC is one-way; AEAD ciphertext is opaque without the DEK). Named marginal exposure (round-5 D-1 correction): file-key **and** DB dump together expose un-reaped sealed successors — each a **live chain-head** token — for **≤ the idem-reap cadence (~60s)**, the *physical* dwell, not the 30s *logical* window; small, bounded, not denied (a future raw-free deterministic-derivation variant removes even this). Table fenced to `zeroship_auth`. | `refresh_token_stored_as_keyed_hash_never_plaintext` ; `refresh_tokens_table_revoked_from_nonauth_roles` ; `idem_cache_is_aead_sealed_with_aad_binding` ; `db_only_dump_yields_no_usable_token` |
| T7 | **Client impersonation** | Public: token possession + `client_id` binding. Confidential: `client_secret` vs `client_secret_hash`. Binding mismatch → `invalid_grant`; `/revoke` enforces client-binding (§7). | `refresh_rejects_wrong_client_id` ; `confidential_refresh_requires_client_secret` ; `revoke_foreign_client_token_returns_200_without_revoking` |
| T8 | **Ceiling bypass via rotation** | `family_absolute_expires_at` copied verbatim every child (step 8), never recomputed; gate at step 4. | `rotation_does_not_extend_family_ceiling` |
| T9 | **Credential-bump TOCTOU** (password reset races a rotation **or a fresh login/issuance**) | The bulk revoke takes the **outer `user-lock(U)`** (§5.4/§7) before its set-based `WHERE user_id` UPDATE. **Every** refresh-row writer takes the same `user-lock(U)` before any row touch → all serialized, **deadlock-free** (no `40P01`; round-3 MAJOR-A + round-5 A-1 sweeper). **No family — root or child — created mid-reset escapes the revoke** (round-5 A-2): a racing *rotation* that loses the lock sees the revoked family on its next read; a racing *issuance* (device-grant login on stale creds) re-reads `credential_version` under the lock and **refuses** if bumped (§3 step 3), and if it instead committed first its live root is caught by the revoke's `WHERE user_id=$U`. The console face is killed via `validate`'s `credential_version` clause. | `op_family_killed_on_credential_version_bump` ; `password_reset_during_rotation_fails_closed` ; `bulk_credential_bump_revoke_does_not_deadlock_concurrent_rotation` ; `issuance_on_stale_credential_version_is_rejected_under_lock` ; `root_minted_before_reset_is_caught_by_bulk_revoke` ; `sweeper_does_not_deadlock_concurrent_bulk_revoke` |
| T10 | **Oracle via error differentiation** | All refresh failures collapse to one `invalid_grant`; deadlock-free path means no `40P01` leaks as 5xx-vs-`invalid_grant` (§5.4); logs carry `refresh_family_id`/`client_id`/`user_id`, never token values. | `refresh_errors_are_indistinguishable_invalid_grant` |
| T11 | **HMAC key roll → mass logout** | **Graceful:** versioned keyring + 30d verify overlap (§2.4); rotation never invalidates a live family. **Emergency (key compromise, MINOR-F):** drop the leaked version immediately → only families minted under it fail closed (bounded re-auth), deliberate and contained. | `refresh_hash_key_rotation_does_not_invalidate_live_families` ; `emergency_key_drop_fails_closed_only_affected_families` |

### 9.1 Operational signal — family-kill rate as a first-class metric (round-1 missing concept #1)

Family kills are **simultaneously** the theft indicator and the false-positive indicator, so the *rate* is the single most important operational signal. Required:

- Metric `op_refresh_family_kill_total{reason="replay|revoke|credential_bump|logout"}` and `op_refresh_idempotency_replay_total` (benign-retry recoveries). Emitted by `op/refresh.rs` (`crates/auth` already exports observability via `crates/core`).
- **Alert:** a sustained rise in `reason="replay"` kills (especially for one `client_id`/`user_id`) is a theft signal; a rise correlated with deploy/CI activity is a benign-retry/idempotency-window-too-small signal → tune `idem_window`. Runbook owner: auth on-call.
- This metric, not a log grep, is how OQ-5 (idle window) and the `idem_window` are tuned post-launch.

---

## 10. Net-new vs reused

| Component | Status | Notes |
| --- | --- | --- |
| `zeroship.oauth_refresh_tokens` table + indexes + grants (`V0064`) | **net-new** | Canonical + flagged amendments (`family_absolute_expires_at`, `family_granted_scopes`, `hash_key_version`, `idem_*`); modeled on `V0063`. |
| Control-plane revocation read (`revoked_after_for` in `oauth_guard_from_bearer`, cache-backed) | **net-new (P5b-7)** | MAJOR-B: control today only *writes* `token_revocations`; this adds the read so a killed deploy family stops authorizing within the cache TTL (§5.2). |
| AEAD DEK for the idempotency cache (`REFRESH_IDEM_KEY_FILE` or shared file/KMS custody) | **net-new secret** | MINOR-D: seals `idem_response_enc` (XChaCha20-Poly1305/AES-256-GCM, AAD-bound). Sibling custody to `refresh_hash_key`. |
| `crates/auth/src/op/refresh.rs` (`exchange_refresh_token`) | **net-new** | Rotation + reuse-detection + bounded idempotency + family advisory lock. |
| `with_xact_advisory_lock2(conn, ns, key)` in `crates/auth/src/advisory_lock.rs` | **net-new helper (round-5 MINOR-2)** | Two-arg, **xact-scoped** `pg_advisory_xact_lock(int4, int4)` on the **writing** connection (auto-released at COMMIT/ROLLBACK). The existing helper is session-scoped single-i64-key and cannot express the `(NS, hashtext)` two-namespace keyspace §5.4 needs. Used by rotation, issuance, `/revoke`, bulk revoke, and the sweeper. |
| `auth_credential_version` on the auth-code / device-code grant record | **net-new column (round-5 A-2; stamping pinned round 7)** | The `users.credential_version` captured **at the moment of credential proof**, carried on the grant so issuance (§3 step 3) can reject a root minted on stale credentials after a racing password reset. **Stamped by the grant-minting handler:** the `/authorize` handler (`op/authorization.rs`) for the auth-code flow (= the session's `credential_version` at last credential proof for a no-fresh-proof SSO code, **not** re-read at code issuance) and the **device-approval** handler for the device-code flow; the token-exchange handlers copy it verbatim into refresh issuance (§3 step 3). |
| `TokenResponse.refresh_token`, `TokenRequest.{refresh_token,client_secret,scope}` | **net-new fields** | On existing structs (`authorization_code.rs:47,57`). |
| `grant_type` dispatch in `token_inner` | **modified** | Today rejects ≠ `authorization_code`. |
| Root-token mint in `exchange_authorization_code` / device grant | **modified** | §3 issuance under `offline_access`. |
| `oauth_clients.client_secret_hash`, `.refresh_allowed`, `.token_endpoint_auth_method` | **net-new columns** | Confidential client auth (schema-redesign:313). **No** per-app gateway secret — confidential clients custody their own. |
| `REFRESH_HASH_KEY_FILE` keyring (file/KMS) | **net-new secret** | Sibling of `AUTH_SIGNING_KEY_FILE`; versioned (§2.4). |
| `/revoke` endpoint (refresh arm, client-binding check) | **net-new** (shared w/ P0) | RFC 7009. |
| `zeroship.token_revocations` marker | **reused** | Family-kill writes one row per `(client_id, pairwise_sub)`. |
| `app_oauth_clients.sector_identifier` + `AUTH_PAIRWISE_SALT` | **reused** | Single source of truth for pairwise re-derivation. |
| Session **machinery** (`store/sessions.rs::validate`) | **reused** | The browser/console face's longevity anchor (§8). |
| Console **session kind + lifetimes** (distinct from `sessions/login.rs` 12h/30-min) | **net-new (P5c)** | Dedicated 30-day-abs / 7-day-idle + step-up policy (§8.1, MAJOR-C). NOT the interactive IdP login session. P5c builds it. |
| `crates/gateway/src/anchors.rs` + `auth_token.rs::do_refresh` | **removed (P5c)** | Browser OAuth-refresh anchor retired (P0 §551); not swapped. |

---

## 11. Phasing + acceptance tests

Ordered sub-steps, each with named gating tests. Land in order. **No browser-refresh build in this epic** (that face is P5c). Per `feedback_faithful_e2e_tests`: e2e exercises the real OP `/token` path, not a shim.

**P5b-1 — Migration + grants.** `V0064__oauth_refresh_tokens.sql` (table incl. `family_absolute_expires_at`, `hash_key_version`, `idem_*`; partial unique index; family index; guarded grants; CHECK). `oauth_clients.client_secret_hash`/`refresh_allowed`/`token_endpoint_auth_method`. Also update schema-redesign §`oauth_refresh_tokens` + P0 §3.2 to carry the two flagged amendment columns (keep the source of truth in sync).
- Gating: `oauth_refresh_tokens_one_active_per_family_unique_index_enforced`; `refresh_tokens_table_revoked_from_nonauth_roles`; `grant_block_is_role_existence_guarded`; schema-render parity test (DB-free) per `reference_migrate_pg5440_testdb`.

**P5b-2 — `TokenResponse` + issuance (under the §5.4 lock + credential re-check).** Add fields; carry `auth_credential_version` on the auth-code / device-code grant; mint the root token under `offline_access` in `exchange_authorization_code` + the device grant, taking `user-lock(U)` first (`with_xact_advisory_lock2`) and re-reading `users.credential_version` under it, refusing if bumped (§3 steps 2–3, round-5 A-2).
- Gating: `token_response_includes_refresh_token_when_offline_access`; `no_refresh_token_without_offline_access`; `refresh_family_id_is_server_generated_not_client_controlled`; `refresh_token_stored_as_keyed_hash_never_plaintext`; `no_id_token_in_refresh_response`; `issuance_on_stale_credential_version_is_rejected_under_lock`; `root_minted_before_reset_is_caught_by_bulk_revoke`.

**P5b-3 — Rotation (happy path + scope + client auth) + the xact advisory helper.** Add `with_xact_advisory_lock2(conn, ns, key)` (`pg_advisory_xact_lock(int4,int4)`, xact-scoped, on the writing connection — round-5 MINOR-2). `op/refresh.rs` happy path with the two-level `user→family` advisory hierarchy; narrowing scope; public + confidential client auth.
- Gating: `refresh_rotation_happy_path_returns_new_token_no_id_token`; `refresh_rotation_narrowing_is_reversible_within_original_grant`; `refresh_rotation_rejects_scope_above_original_grant`; `rotation_does_not_extend_family_ceiling`; `refresh_rejects_wrong_client_id`; `confidential_refresh_requires_client_secret`; `pairwise_sub_stable_and_equal_across_mint_and_revoke_check`.

**P5b-4 — Reuse detection + bounded idempotency + family kill.** Replay branch; idempotency replay; the deadlock-free family lock; the legit-retry recovery (round-1 #14).
- Gating: `refresh_replay_after_rotation_kills_family`; `legit_lost_response_retry_recovers_without_family_kill`; `concurrent_refresh_same_token_one_rotates_other_recovers_or_kills`; `family_kill_does_not_deadlock_under_concurrent_replays`; `bulk_credential_bump_revoke_does_not_deadlock_concurrent_rotation` (round-3 MAJOR-A — the cross-family writer under the user lock); `family_kill_writes_one_token_revocations_marker_per_client_subject`; `refresh_errors_are_indistinguishable_invalid_grant`; `op_family_killed_on_credential_version_bump`; `idem_cache_is_aead_sealed_with_aad_binding`; `idem_window_elapsed_then_replay_kills_family`.

**P5b-5 — `/revoke` (refresh arm) + key-rotation.** Land `/revoke` with the client-binding check; the keyring verify-overlap; the graceful and emergency key-rotation runbooks.
- Gating: `revoke_refresh_token_kills_family_returns_200`; `revoke_unknown_token_returns_200`; `revoke_foreign_client_token_returns_200_without_revoking`; `refresh_hash_key_rotation_does_not_invalidate_live_families`; `emergency_key_drop_fails_closed_only_affected_families`.

**P5b-6 — Ops + sweeper (per-user under the lock + prioritized idem-reap).** Family-kill-rate + idempotency-replay metrics (§9.1); the sweeper (§6.1) — **Pass 1** family-delete per-user under `user-lock(U)`, **Pass 2** prioritized idem-reap (~60s) also per-user under the lock (round-5 A-1 + D-1).
- Gating: `op_refresh_family_kill_metric_emitted`; `sweeper_deletes_expired_families_and_clears_idem_cache`; `sweeper_does_not_admit_two_live_tokens`; `sweeper_does_not_deadlock_concurrent_bulk_revoke` (round-5 A-1 — the per-user `user-lock` batch vs the credential-bump bulk revoke); `idem_reap_pass_clears_ciphertext_within_cadence`.

<!-- Added in round 3 (MAJOR-B): control-plane revocation read is a SCHEDULED deliverable, not asserted-as-existing behavior. -->
**P5b-7 — Control-plane access-token revocation enforcement (Open operator decision #1, recommended).** Add the `revoked_after_for(client_id, pairwise_sub)` read to the control bearer guard (`oauth_guard_from_bearer` / control authz guard), cache-backed (short TTL, recommend 10s): reject an `at+jwt` whose `iat` predates the family's `revoked_after`. This makes CLI/deploy family revocation effective within the cache TTL, not ≤900s. Per `feedback_faithful_e2e_tests`, the test exercises the **real control deploy path** (mint → kill family → assert the next `zeroship deploy`-class request is rejected after the cache TTL), not a shim.
- Gating: `control_plane_rejects_at_jwt_minted_before_family_revoke`; `control_plane_revocation_read_is_cache_backed_within_ttl`; `gateway_already_enforces_revoked_after_unchanged` (regression guard that the gateway path still enforces).

> The browser-face session + JIT mint contract (§8) is **P5c**; its tests live there (e.g. `gateway_browser_reload_reuses_session_no_refresh`, `credential_bump_invalidates_browser_session_on_next_validate`). They are named here only to mark the boundary.

Per `feedback_regression_test_per_fix` + `feedback_verify_full_suite_not_lib`: every threat in §9 has a regression test; verify the FULL per-crate suite (`cargo test -p zeroship-auth`), not just `--lib`.

### Conformance checklist tie-in (conformance-map §4 `/token`, §5A MUST-NOT-cut)

| Checklist item | Satisfied by |
| --- | --- |
| Support `grant_type=refresh_token` | §1, P5b-3 |
| Rotation: new RT each use, invalidate old, reuse→revoke family | §4 step 6/8, §5, P5b-3/P5b-4 |
| Response `Cache-Control: no-store`, `Pragma: no-cache`, `application/json` | §1 (`authorization_code.rs:686`) |
| Standard `error` codes (`invalid_grant`, `invalid_client`, `invalid_scope`) | §1, §5.1 |
| `/revoke` 200 for unknown/foreign token; client-binding; revokes the family + **derived access** (gateway today; control plane via P5b-7, within the cache TTL) | §7, §5.2, P5b-5, **P5b-7** |
| Public-client RT one-time-use + reuse detection (OAuth 2.1 §6.1) | §4/§5 |
| Audience + `typ` on the rotated access token (`at+jwt`) | reused `issue_access_token` (`issuer.rs:243`, RFC 9068) |
| Browser face: no RT to the browser (OAuth 2.0 Browser-Based Apps BCP) | §8 (Option B) |

---

## 12. Open questions for the operator

- **OQ-1 — RESOLVED → Option B.** The browser/console face gets **no refresh family**; it is a bounded server session + JIT mint (§8, build = P5c). The CLI/confidential `offline_access` face gets the rotating refresh family (this spec). This agrees with P0 §4.1.
- **OQ-2 — RESOLVED → keyed HMAC-SHA256, versioned.** `REFRESH_HASH_KEY_FILE` keyring + `hash_key_version` + 30d verify overlap (§2.4).
- **OQ-3 — RESOLVED → omit `id_token` on refresh** (§4 step 9; OIDC Core §12.2). Identity facts via `/userinfo`.
- **OQ-4 — RESOLVED → bounded idempotency, not unbounded grace** (§5.3). Default `idem_window = 30s`, tuned via the §9.1 metric. The strict-no-grace footgun (false kills on benign retries) is closed without weakening theft detection (a chain-advance closes the window early).
- **OQ-5 (§6):** idle window for the CLI family — **7d sliding under the 30d ceiling** (recommended, tighter theft window on idle CLIs) vs 30d (idle = ceiling, one clock). Operator pick; the §9.1 metric informs it post-launch.
- **OQ-6 (§8.4):** post-launch, surface an OP family id for cross-face observability — not required for Option B (the faces are decoupled). Low priority.
- **OQ-7 (round-3 MAJOR-B → see § Open operator decisions #1):** CLI/deploy access-token revocation enforcement — **immediate enforcement** (control reads `token_revocations`, residual = `max(cache TTL, NTP skew + 1s iat granularity)`, ~10s under disciplined clocks; **recommended**, built as P5b-7) vs accept-the-≤900s-AT-TTL residual. Recommend immediate for deploy tokens. Note (round-5 B-2): this covers only the `at+jwt` family path, **not** PAT-authenticated deploys.
- **OQ-8 (round-3 MAJOR-C → see § Open operator decisions #2):** console/app session lifetimes — recommended **30-day absolute + 7-day sliding idle + step-up re-auth** for sensitive actions (distinct from the 12h/30-min IdP login session; replaces the retired 30-day no-idle anchor). Numbers operator-tunable; this is the P5c console contract.

---

## What this round could NOT fully resolve (honest residuals)

- **T1 idle-victim residual** (§9 T1): an attacker who steals a *live* refresh token from an idle CLI can rotate silently up to the 30d ceiling. This is intrinsic to bearer-token rotation (RFC 9700 §4.14 names it). Full closure needs sender-constrained tokens (DPoP / mTLS), which is **out of scope** for P5b and noted as a future hardening (`offline_access` + DPoP). Mitigations in-scope: short idle window (OQ-5), the family ceiling, and family-kill-rate alerting (§9.1).
- **Bounded-idempotency window residual** (§5.3): within `idem_window` and before the chain advances, an attacker replaying the predecessor obtains the same successor as the legit client — indistinguishable from a lost-response retry by construction. Minimized (small window, chain-advance closes it early) but not eliminable while supporting lost-response recovery. This is the standard Auth0/Okta/Duende tradeoff.
<!-- Corrected in round 3 (MAJOR-B): the round-2 text falsely said the control plane "does consult" the marker. It does not today; P5b-7 adds the read. -->
- **Cross-RS access-token residual** (§5.2): a killed family's already-minted `at+jwt` stays valid until a resource server that reads `token_revocations` rejects it. **Gateway:** enforces today (`auth_token.rs:1156`) → residual = `max(cache TTL, NTP skew + 1s iat granularity)`. **Control plane (the deploy RS):** does **not** enforce today (write-only — `oauth_grants_handlers.rs:240`); **P5b-7** adds the cache-backed read, shrinking its residual from ≤900s to the same `max(cache TTL, skew + 1s iat granularity)` (~10s under NTP-disciplined clocks) — see Open operator decision #1. <!-- Corrected round 5 (B-1): not exactly "= cache TTL". --> The skew/granularity term is inherent to the existing `iat < revoked_after` compare (`wrapper_revocation.rs:100`), shared with the gateway, mitigated by clock discipline — not new to P5b-7. **PATs are a separate credential path** (round-5 B-2): `zeroship deploy` may authenticate with a PAT / `ZEROSHIP_TOKEN`, which is **not** an `at+jwt` keyed by `(client_id, pairwise_sub, iat)`; killing a refresh *family* does **not** stop a PAT-authenticated deploy. PAT revocation is its own store/path, out of scope for P5b, named here so "kill family ⇒ deploy stops" is not silently assumed for PATs. **Any future RS** that reads neither the marker nor caps the AT TTL re-opens a ≤900s window **for itself**; that is an explicit RS obligation, not a platform guarantee. Not eliminable to zero without per-request introspection (rejected for latency).
- **Idem-cache at-rest dwell** (§5.3, §6.1 Pass 2 — corrected round-5 D-1): under (DEK compromise **and** DB dump together), an un-reaped AEAD-sealed successor decrypts to a **live chain-head refresh token**, exposed for **≤ the idem-reap cadence (~60s recommended)** — the *physical* dwell, since the ciphertext persists past the 30s *logical* `idem_window` until the prioritized reap NULLs it. Bounded and minimized (tight reap cadence, DEK never in Postgres so DB-only dump exposes nothing), not eliminable while caching raw successor material for lost-response recovery; a future raw-free deterministic-derivation variant removes even this.

---

## Sources (RFCs verified against the conformance map, itself verified against IETF Datatracker / RFC Editor)

- RFC 6749 — OAuth 2.0 (refresh grant §1.5; §6 scope MUST NOT exceed — and if omitted equals — the scope **originally granted** by the resource owner, i.e. the ceiling is the original grant not the prior token, MINOR-E; token/error §5.1/§5.2): https://www.rfc-editor.org/rfc/rfc6749
- RFC 9700 (BCP 240) — OAuth Security BCP (Refresh Token Protection §4.13, §4.14, §2.2.2): https://www.rfc-editor.org/rfc/rfc9700
- draft-ietf-oauth-v2-1 — OAuth 2.1 (rotation §6.1, refresh grant §4.3): https://datatracker.ietf.org/doc/draft-ietf-oauth-v2-1/
- RFC 7009 — Token Revocation (`/revoke` §2.1 client-binding, 200-for-unknown §2.2): https://www.rfc-editor.org/rfc/rfc7009
- RFC 9068 — JWT Profile for Access Tokens (`at+jwt`): https://www.rfc-editor.org/rfc/rfc9068
- RFC 8252 (BCP 212) — OAuth for Native Apps (public client §8.5): https://www.rfc-editor.org/rfc/rfc8252
- RFC 8628 — OAuth Device Authorization Grant (the `zeroship login` flow): https://www.rfc-editor.org/rfc/rfc8628
- OAuth 2.0 for Browser-Based Apps (BCP draft) — no refresh token to the browser; BFF/token-handler pattern: https://datatracker.ietf.org/doc/draft-ietf-oauth-browser-based-apps/
- OIDC Core 1.0 — §12.2 (id_token on refresh OPTIONAL; nonce MUST match original), §8.1 (pairwise): https://openid.net/specs/openid-connect-core-1_0.html
- Internal: `docs/proposals/oauth2-standards-conformance.md`, `docs/proposals/2026-06-30-op-p0-spec-threat-model.md` (§2.4/§4.1/§446/§551/§559), `docs/proposals/2026-06-30-auth-schema-redesign.md` (§`oauth_refresh_tokens`)
- Internal code: `crates/auth/src/sessions/login.rs` (30m idle/12h absolute), `crates/auth/src/store/sessions.rs::validate`, `crates/auth/src/op/authorization_code.rs`, `crates/auth/src/op/issuer.rs`, `db/migrations/V0063__oauth_authorization_codes.sql`
