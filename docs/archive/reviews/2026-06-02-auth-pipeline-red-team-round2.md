The commit history confirms the verdict structure: F1/F4 in `5473889c`, F2/F7 in `86af568a`, F3 in `69dda8a2`, migrations in `7ac21796`, and the 0032 grant. The closure verdicts and dual-verifier findings are internally consistent and grounded against the live stack. Synthesizing the round-2 verdict.

```markdown
# Auth Pipeline Red-Team — Round 2 Verdict (2026-06-02)

## VERDICT

**Closure:** All seven in-scope regressions (F1, F2, F3, F4, F6, F7) are remediated **at the layer pass-3 targeted**, but **F1 is NOT fully closed** — the fix is over-narrow and leaves a confirmed residual on the symmetric interactive-login minter. F2, F3, F4, F6, F7 are clean closures with no residual bypass.

**Did pass-3 introduce a NEW regression?** **Yes — two.** The migration-integrity fix (`7ac21796`) introduced a **HIGH over-restriction** (`6.0`): an inert `validCheckSum:ANY` that bricks re-migration of any already-migrated stack. The F3 fix (`69dda8a2`) introduced a **LOW over-restriction** (`7.0`): `fleet_wide_reader` omits the `billing` role, so billing staff get an inconsistent (empty) `/api/apps` list despite fleet-wide Cedar authority.

**Bottom line: NOT clean — another fix pass is required** (one HIGH residual + one HIGH new regression, both dual-confirmed).

---

## Closure table

| Regression | Now blocked? | Blocking control | Residual |
| --- | --- | --- | --- |
| **F1** cookie eviction | **Partial** | Per-app family marker in `token_revocations` written by `password_reset::complete` revoked_families CTE; enforced by `is_family_revoked_since`. Effective only when the cookie mint persists `app_user_identities`. | **YES (high)** — interactive OIDC callback minter (`issue_interactive_session_cookie`) never writes `app_user_identities`, so its `__Host-zeroship_app_session` cookie survives reset for full ~15-min TTL. See finding **0.0**. |
| **F2** lockout recovery | **Yes** | Three owner-present clear paths (reset CTE / magic-link redeem / OAuth success) clear `failed_login_count`+`locked_until`; wrong-password path only increments. | none |
| **F3** self-authority / over-grant | **Yes** | `self_service.cedar` scoped to `resource is Resource` (synthetic `Resource::"*"`), never matches concrete App; per-app authority flows only through `app_members`-bound policies. Cross-tenant DENY holds live. | none (see new findings 2.0, 7.0 for sibling defects in the fix) |
| **F4** mint TOCTOU | **Yes** | `update_rotated_family` rows-affected → `Ok(0)`→`LoginRequired` (durable backstop) + post-refresh family-marker re-check read direct from DB. Faithful live race test fails closed. | none |
| **F6** admin grant | **Yes** | Changeset 0032 `GRANT UPDATE` on `platform_admin_roles`/`platform_policies` to `zeroship_control`; Cedar `TeamWrite` gate remains the access control. Verified live. | none |
| **F7** timing | **Yes** | `record_login_failure_dummy()` issues one throwaway UPDATE round-trip per failure arm, matching the real sub-threshold arm. Live SQL trace shows identical shape. | Narrow sibling (5.0, low) — threshold-crossing 5th attempt issues an un-equalized 2nd UPDATE the test/counter can't see; subsumed by the in-band lock disclosure. |
| **Migration integrity** | **No** | Intended masking control (`validCheckSum:ANY`) is non-functional. | **YES (high)** — see finding **6.0**. |

---

## Surviving NEW findings (ordered by final severity)

Final severity is set from the two verifier verdicts (REP = reproduce lens, CTL = control lens).

### HIGH

#### 0.0 — F1 residual: interactive OIDC callback cookie mint skips `app_user_identities`, so its session survives a password reset
- **Kind:** missed-sibling · **Final severity: HIGH** · **Status: Confirmed** (REP=confirmed/high, CTL=confirmed/high)
- **Chain:** Unauthenticated HTML client → `start_oidc_redirect` → Hydra → `GET /__zeroship/auth/callback` → `handle_auth_callback` mints the signed `__Host-zeroship_app_session` cookie via `issue_interactive_session_cookie`, which derives `pws_`+signs but **never calls `identities::upsert` and writes no anchor**. The reset's `revoked_families` CTE JOINs `app_user_identities` → 0 rows → 0 `token_revocations` markers → cookie gate `is_family_revoked_since` returns NotRevoked → **cookie accepted for full ~15-min TTL after the victim's reset.**
- **Why the claimed backstop is absent:** `credential_version` is bumped to 1 but the **gateway never reads it** — the only reference in `crates/gateway/src` (non-test) is a comment at `auth_token.rs:617`. No code backstop exists.
- **Live repro:** Two seeded victims under the verbatim `complete()` CTE — SDK-path victim (with identity row + anchor) → 1 marker written, anchor revoked, cookie rejected (closure good); interactive-path victim (no identity row, no anchor) → 0 markers, cookie accepted.
- **Scope:** This is the primary browser-login path for any creator app whose end-users log in without the JS SDK. Pass-3 patched only `mint_session_from_code` (SDK `POST /session`) and left the symmetric interactive minter unpatched — the exact gap F1 was meant to close.
- **Severity rationale (high, not critical):** post-compromise persistence — attacker must already hold/steal the cookie; the bug defeats "reset kills all sessions" for this minter, bounded by cookie TTL. Not an instantaneous bypass or cross-tenant escalation.
- **Fix:** have `issue_interactive_session_cookie` (or `handle_auth_callback` pre-mint) call `identities::upsert(client_id, global_user_id, pws_sub)` like the SDK path, so the reset CTE learns the `(client_id, pws_)` to revoke.
- **Files:** `crates/gateway/src/router/dispatch.rs`, `crates/gateway/src/auth_token.rs`, `crates/auth/src/identity/password_reset.rs`, `crates/gateway/src/router/auth.rs`

#### 6.0 — `validCheckSum:ANY` on 0025 platform-roles is silently ignored → re-migration bricks the already-migrated stack
- **Kind:** over-restriction (NEW, introduced by pass-3 commit `7ac21796`) · **Final severity: HIGH** · **Status: Confirmed** (REP=confirmed/high, CTL=confirmed/high)
- **Chain:** Live DB stores 0025 `platform-roles` checksum `9:451fe25…` (pre-fix body). Pass-3 edited the changeset body → new checksum `9:f2fe307…` and added `validCheckSum:ANY` as an **inline `--changeset` attribute** to mask the drift. **Liquibase's SQL-formatted parser does not recognize `validCheckSum` as an inline attribute — it must be its own `--validCheckSum <value|ANY>` line — so it is silently dropped.** `liquibase update` then fails: `platform-roles was: 9:451fe25… but is now: 9:f2fe307…`. The compose `migrate` service runs `update` with `restart: on-failure`; control/auth/gateway/builder/hydra-migrate all `depends_on: migrate: service_completed_successfully` → migrate crash-loops → **whole stack cannot re-up.**
- **Live repro:** `liquibase/liquibase:4.31 update` against the live DB → `ValidationFailedException` on exactly this one changeset; all other 0025/0028/0031/0032 changesets validate clean. A/B root-cause proof: inline `validCheckSum:ANY` FAILS; inline literal checksum FAILS; **dedicated `--validCheckSum ANY` line SUCCEEDS**; dedicated literal-checksum line SUCCEEDS. Fresh DBs migrate 76/76 idempotently (store `f2fe307`), which is why fresh-stack smoke masks it.
- **Severity rationale (high):** durable availability/operability failure — any re-up of the already-migrated production-shaped stack bricks every gated service; slipped past green fresh-DB smoke. No auth bypass.
- **Fix:** replace the inline `validCheckSum:ANY` on `0025_roles_rls.sql:83` with a dedicated `--validCheckSum ANY` line directly under the `--changeset` header.
- **Files:** `db/changelog/changesets/0025_roles_rls.sql`, `docker-compose.yml`

### MEDIUM / LOW (contested or low-impact)

#### 5.1 — Status-code enumeration sibling: locked real account → 403 vs absent account → 401
- **Kind:** missed-sibling · **Final severity: LOW** · **Status: Contested** (REP=confirmed/**medium**, CTL=**partial**/low)
- **Chain:** After 5 wrong-password attempts a real account locks; the next attempt takes the ineligible arm → `Ineligible` → **403** (`"account temporarily locked"`). An absent email can never lock → always `InvalidCredentials` → **401**. A binary, single-request account-existence oracle, louder than the F7 timing delta and in the same info-disclosure class. Predates F7 (rooted in the L5/F2 lockout response design); F7 equalized timing only.
- **Disagreement:** REP rates **medium** (deterministic single-request binary signal, doubly observable via status + body, attacker-inducible within the per-email bucket headroom). CTL rates **low/partial**: from a **single source IP** the cap-5 EIP rate-limit bucket exactly matches the threshold-5 lockout, so attempt 6 (the first 403-revealing attempt) returns 429 — the trivial single-IP attack is masked. The residual survives only with **IP diversity** (or a spoofable `X-Forwarded-For`, since the auth service honors forwarded headers) to keep the EIP bucket fresh while the cap-10 EMAIL bucket permits attempts 6–10.
- **Final severity set to LOW:** the single-IP path (the as-described attack) is blocked by the EIP limiter; the surviving multi-IP path costs IP diversity, reveals only account existence (no credential/session/cross-tenant access), and triggers a self-inflicted lockout. Account existence enumeration via lock-state observability is partly inherent to having lockout at all. Recommended hardening: equalize the lock-state response (or return a uniform 401 with the lock signaled out-of-band).
- **Files:** `crates/auth/src/identity/credentials.rs`

#### 7.0 — `billing` platform role: reads any app individually but `/api/apps` list wrongly scoped to owned apps only
- **Kind:** over-restriction / missed-sibling (NEW, introduced by F3 fix `69dda8a2`) · **Final severity: LOW** · **Status: Confirmed** (REP=confirmed/low, CTL=confirmed/low)
- **Chain:** `billing.cedar` grants `apps:read` on a **bare `resource`** (no `is Resource` constraint), so it matches both `Resource::Any` (list gate) and concrete `App` (`get_app` gate). `get_app` therefore lets billing read **any single app by id**. But `list_apps`'s new `fleet_wide_reader` SQL is `role IN ('admin','readonly','support')` — **`billing` omitted** — so a memberless billing staffer falls to `list_apps_for_owner` → `[]`. The fix comment's premise ("admin/readonly/support are the only roles whose policy permits unconstrained `apps:read`") is **factually false** — `billing.cedar` does too.
- **Live repro:** seeded `platform_admin_roles.role='billing'` + one foreign app with no `app_members` row; `fleet_wide_reader('billing')`→FALSE, `list_apps_for_owner(billing)`→0 apps, fleet total→1. Billing's list returns `[]` for an app it is authorized to (and can) read individually.
- **Severity rationale (low):** over-restriction / consistency defect — billing **under-reads** (sees fewer apps than authorized). No cross-tenant leak (`AppRecord.api_key` is `#[serde(skip_serializing)]`), no over-grant, no bypass. Unguarded by tests.
- **Fix:** add `'billing'` to the `fleet_wide_reader` role set + a regression test asserting a billing staffer sees the fleet-wide list.
- **Files:** `crates/control/src/api.rs`, `policies/platform/billing.cedar`

#### 2.0 — 0031 backfill `ON CONFLICT DO UPDATE SET role='owner'` can promote a lone editor/viewer to owner
- **Kind:** over-grant · **Final severity: LOW** · **Status: Contested** (REP=confirmed/low, CTL=**partial**/low — mechanism reproduced, security consequence refuted)
- **Chain:** `0031_app_members_owner_backfill.sql:38` ends `ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner'` (its own header comment wrongly says `DO NOTHING`). The SELECT does not filter on the member's current role, so a sole editor/viewer of an owner-less app is escalated to owner on (re-)run. Live BEGIN..ROLLBACK: seeded lone editor → `INSERT 0 1` → role became `owner`.
- **Why low / refuted as exploitable:** the **only** application writer to `app_members` is `registry::create_app`, which hard-codes `role='owner'`. There is no editor/viewer member-add API, so the "lone non-owner, no owner" precondition is **unreachable through app code**; it only bites synthetic/hand-seeded rows. Live `app_members` is empty; zero matching apps. The changeset runs once at deploy. Even when fired, it grants ownership only over an app where you are already the sole member — not cross-tenant, not cross-user.
- **Fix:** change line 38 to `DO NOTHING` (matching the comment and intent) or add `WHERE m.role = 'owner'`.
- **Files:** `db/changelog/changesets/0031_app_members_owner_backfill.sql`

#### 5.0 — F7 residual: threshold-crossing wrong-password attempt issues an un-equalized 2nd UPDATE the regression counter can't see
- **Kind:** missed-sibling · **Final severity: LOW** · **Status: Contested** (REP=confirmed/low, CTL=**refuted**/low — structural claim true, security claim refuted)
- **Chain:** On the 5th consecutive wrong-password attempt against a real account, `record_login_failure` issues `UPDATE…RETURNING` (round-trip 1, counter bumped at `users.rs:204`) then a **second conditional `UPDATE locked_until`** (round-trip 2, `users.rs:224`, **not** counted). Equalized/absent/locked arms always do one round-trip, so the 5th-attempt real path is one round-trip slower. The F7 test drives only one sub-threshold attempt (`real_delta==1`), so neither the counter nor the test sees it.
- **Why low / security refuted:** the extra UPDATE fires only on the 5th+ failure against a real, unlocked, password-bearing account the attacker is already driving — within the already-identified real arm, not the real-vs-absent distinction F7 closed. The only state it signals (the lock transition) is returned **in-band** on the next request (`Ineligible`→403), and the ~2ms delta is swamped by the ~700–850ms Argon2 verify (the threshold attempt measured as the *fastest* of five). No covert secret leaks; subsumed by the 403/401 functional oracle (5.1).
- **Fix (hardening):** fold `locked_until` into the first UPDATE via `CASE`/`RETURNING` so no arm ever does 2 round-trips, and bump the counter symmetrically; extend the test to drive the threshold.
- **Files:** `crates/auth/src/store/users.rs`, `crates/auth/src/identity/credentials.rs`, `crates/auth/tests/account_lockout_test.rs`

#### 3.0 — Pre-existing test-fixture fragility: 7 DB-gated anchor tests FK-fail on a freshly-migrated DB (NOT F4-caused)
- **Kind:** new-vuln (mislabeled; actually CI hygiene) · **Final severity: LOW** · **Status: Confirmed** (REP=confirmed/low, CTL=confirmed/low)
- **Chain:** `auth_token_anchors_test` (7 older tests) call `seed_user()`+`seed_relay_alias()` but never `seed_app_and_client()`, so the shared `oauth_clients` row `oac_myapp` is absent and `app_user_identities` INSERT FK-violates (E23503). `cleanup_f1` (line 1689) also DELETEs the shared `oac_myapp`, making the suite order-dependent. The F1/F4 tests self-seed and pass.
- **Correction to the claim:** both verifiers found the 7 tests fail **in isolation too** (missing seed is order-independent), so the dominant root cause is the missing `seed_app_and_client`, not `cleanup_f1`. Failures are hard FK violations at seed time → loud RED, never a false GREEN, so **nothing security-relevant is masked**; the F4/F1 product fixes are independently verified by the self-seeding tests.
- **Severity LOW:** CI signal erosion / fixture-completeness only. Not F4-caused (commit `5473889c` did not touch these 7 tests).
- **Fix:** have the 7 tests call `seed_app_and_client` (or a shared per-suite seed); make `cleanup_f1` not DELETE the shared `oauth_clients` row (or use `serial_test` / per-test unique client id).
- **Files:** `crates/gateway/tests/auth_token_anchors_test.rs`

#### 6.1 — 0031 header comment misdescribes its own behavior (DO NOTHING vs DO UPDATE; phantom `creator_id` source)
- **Kind:** missed-sibling (documentation) · **Final severity: LOW** · **Status: Confirmed** (REP=confirmed/low, CTL=confirmed/low)
- **Detail:** `0031` header (lines 20–22) claims `ON CONFLICT … DO NOTHING` and a `creator_id`-sourced backfill; the SQL is `DO UPDATE SET role='owner'` and there is no `apps.creator_id` column (the file body correctly derives from sole-membership). **CTL refutes the claim's own "harmless self-write" framing and the proposed "fix to DO NOTHING":** because the SELECT targets the exact row that conflicts, `DO NOTHING` would silently **no-op the promotion** and leave single-member apps ownerless — the very over-restriction the changeset exists to repair. The CODE is correct; only the COMMENT is wrong (doubly).
- **Fix:** correct the comment to say `DO UPDATE` and explain *why* `DO NOTHING` would be wrong (the PK row always pre-exists), to stop a future maintainer "fixing code to match comment" and reintroducing a lockout.
- **Files:** `db/changelog/changesets/0031_app_members_owner_backfill.sql`

---

## Bottom line

**The auth pipeline is NOT clean — one more fix pass is needed.** Pass-3 cleanly closed F2/F3/F4/F6/F7, but left a **HIGH F1 residual** (interactive OIDC-callback sessions survive password reset because the minter never persists `app_user_identities` and the claimed `credential_version` backstop is dead code) and introduced a **HIGH new regression** (the inert `validCheckSum:ANY` bricks re-migration of the live/already-migrated stack). Both are dual-confirmed against the live PG+Hydra stack and must be fixed before this pipeline can be called closed; the remaining LOW findings (billing list under-scope, 0031 over-grant + comment drift, F7 threshold round-trip, 5.1 lock-state oracle, anchor-test fixtures) are hardening follow-ups.