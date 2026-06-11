# Auth & Security Remediation Plan — 2026-06-11

> Companion to `docs/reviews/2026-06-11-auth-security-review.md`. Every finding below was
> **independently re-verified against source by a fresh-model (fable) pass**, which also
> composed the solution. Verdicts: all confirmed; **none refuted**; two downgrades
> (NEW-3 lower than filed, CT-C2 reframed). The team is open to deep refactor — each item
> notes whether the recommendation is a focused fix or a refactor and why.
>
> Status: DESIGN APPROVED FOR IMPLEMENTATION — _pending operator scope decision._

## Priority & sequencing

| # | Item | Sev | Shape | Blast radius |
| --- | --- | --- | --- | --- |
| 1 | CT-A1 + RT-5 chain | HIGH | control refactor + runtime patch | creator → uncapped/unpreemptable worker, co-tenant DoS |
| 2 | NEW-1 gateway RPC fail-closed | HIGH | 6-line patch (+rank guard) | class risk (AI-gen manifests); 0 apps today |
| 3 | CT-B1/B2/B4 billing integrity | HIGH | billing trust-boundary refactor | fee→0, earnings forgery, meter forgery |
| 4 | NEW-2 SSRF (+RT-2) | MED | runtime patch | creator fetch → cloud IMDS on IPv6/NAT64 |
| 5 | CT-A2 deploy limits | MED | control patch | resource exhaustion |
| 6 | CT-A3/A4 app-name DNS/reserved | MED | control patch + changeset | subdomain squat/collision |
| 7 | low batch: CT-C1, CT-C2, platform_policies, CONTROL_KEY, NEW-3 | LOW | small patches | defense-in-depth |

Recommended order: **2 (cheap, closes a reopened class) → 1 (the chain) → 4 → 5 → 6 → 3 (largest) → 7 batch.**
1, 2, 4–7 touch disjoint crates and can land independently; 3 is the biggest (new server-side Stripe client).

---

## 1. CT-A1 + RT-5 — plan self-escalation → uncapped worker  *(VERIFIED, both ends)*

**Confirmed:** `plan_id` is free-text (`api.rs` create_app/set_plan → `registry.rs:160-167/341-347`), `unlimited`/`enterprise` map to cpu/wall/heap = `None` (`registry.rs:519-535`), and the `app_owner.cedar` grant is **unconstrained** so a plain creator's `BillingWrite` passes — they can `PUT plan="unlimited"`. Downstream, `runtime.rs:1382` only creates the CPU timer `if cpu_limit.is_some()`, and `wall_timeout` is consumed only on the async `Pending` path (`serve.rs:761`/`handler.rs:254`), so a synchronous `while(true){}` on a `None`-cpu plan hangs the OS thread that LRU-multiplexes many tenants — zero preemption.

**Fix — control (small refactor):** new `crates/control/src/plan_catalog.rs` as the single source of truth: `PlanDef { id, cpu_ms, wall_ms, heap_mb, self_serviceable }` (free/pro self-serviceable; unlimited/enterprise not). `registry.rs:519 runtime_limits_for_plan` reads from it (delete the duplicated match); **unknown plan → reject** (not silent-free). Gate both write sites after authz, before registry: unknown `plan_id` → 400; `!self_serviceable` → require platform billing/admin role (factor `is_billing_staff()` out of the existing `fleet_wide_reader` query at `api.rs:167`) → else 403. **Keep `app_owner.cedar` intact** — the tier restriction is a handler-side role check, not a policy weakening, so legitimate free↔pro self-upgrade still works. Optional DB hardening: `apps.plan_id` FK → a `plans` table.

**Fix — runtime (low-risk patch, reuses the existing zero-tokio POSIX-timer watchdog):** add `HARD_CPU_CEILING` (e.g. 30s, configurable). At `runtime.rs:1382` drop the `is_some()` gate — **always** create the timer; compute `eff = cpu_limit.unwrap_or(CEILING).min(CEILING)`; `arm_cpu_timer` arms `eff`. Belt-and-suspenders: clamp `None`→ceiling in `cache.rs:204` too. A sync busy-loop is necessarily CPU-bound (all I/O is async via the pump), so the always-armed `CLOCK_THREAD_CPUTIME_ID` timer is the correct primary backstop; a parallel wall-clock timer is optional.

**Watch-outs:** the console + builder bootstrap apps run on `enterprise` and provision via the **SQL upsert path, not the gated HTTP handlers** — keep them on that path; pick `HARD_CPU_CEILING` high enough not to clip legitimate enterprise/bench workloads (`runtime.rs:1429` bench config uses `cpu_limit=None`). Measure the per-dispatch `timer_settime` cost on the always-arm path (no fabricated numbers).

**Tests (must fail pre-fix):** plain creator `PUT plan=unlimited`/`enterprise` → 403; `pro` → 200; unknown plan → 400; staff `unlimited` → 200; **faithful e2e**: `unlimited` app with `fetch(){while(true){}}` terminates within ceiling AND a co-resident app still serves after; unit: timer created when cpu_limit None, `None`→ceiling clamp.

---

## 2. NEW-1 — `*` anon catch-all defeats the RPC fail-closed default  *(VERIFIED; 0 apps today, HIGH class risk)*

**Confirmed mechanism:** `build_inheritance_chain` prepends `*` to every chain incl. `rpc:` (`compiled.rs:562-565`); any ancestor with `auth` sets `auth_declared` (`:453`); the flip is `if !auth_declared && rpc` (`:523`). A `*: {auth:anon, publicly_accessible:true}` parent therefore suppresses the flip → undeclared `rpc:` procedures resolve Anon. **Blast radius corrected: zero apps exposed today** — passthrough has no `rpc:` keys (404s), the vite-plugin doesn't auto-emit `*`, the builder declares none. The risk is the *class*: AI-generated manifests will naturally produce `*`-anon + an unannotated procedure.

**Fix — `(a)-refined`, 6 lines in `resolve_effective_policy` (NOT dropping `*` from rpc chains):** `*` legitimately contributes strengthen-only policy to rpc (rate_limit-min, cors-intersect, middleware-append, scopes-union) and `rpc.md` documents `*: {auth:admin}` as an intended rpc root, so keep it in the chain. Two edits:
- In the loop: set `auth_declared = true` **unless** `ancestor_key == "*" && key.starts_with("rpc:")` (the `*` web catch-all doesn't count as an rpc opt-out).
- The flip **must gain a rank guard**: `if !auth_declared && rpc && auth.rank() < User.rank()` — otherwise a `*: {auth:admin}` root would now be **downgraded admin→user** (it no longer sets `auth_declared`, so the naive flip would fire and clobber the inherited Admin). This is the subtle bug in the obvious fix.

Net invariant: `*`-anon → rpc resolves User; `*`-user/admin → strengthening preserved; `rpc:<family>`/self `auth:anon`+`publicly_accessible` opt-in preserved; self `override:["auth"]` weakening preserved; URL keys untouched.

**Mirror + docs (same patch, per the 65294240 fidelity discipline):** the same two edits in `apps/zeroship-builder/src/server/config-auth.test.ts` `effectiveAuth`; note in `gateway-routing.md` + `rpc.md`. Opportunistically fix the stale comments at `dispatch.rs:547` / `manifest.rs:299` that claim passthrough "serves everything" (tests prove it 404s everything — separate small ticket).

**Tests:** `*`-anon + `rpc:todos.list`(no auth) → User (RED pre-fix); `*`-admin + `rpc:foo` → Admin (RED against the naive fix); `*`-anon + `rpc:wizard`{anon,override} + `rpc:wizard.suggest` → Anon (own-chain opt-in survives); assert passthrough has no `rpc:` keys. e919af51's three existing tests stay green.

---

## 3. CT-B1/B2/B4 — billing & metering trust boundary  *(VERIFIED, all three)*

**Confirmed:** fee + creator_id are creator-controlled — `@zeroship/payments` runs in the creator's own worker bundle and sets `application_fee_percent` (settable to 0) and `metadata[creator_id]` (forgeable); the webhook reads the fee *back* (`stripe_handlers.rs:444`) and `record_payout` only checks `0≤fee≤gross` (`stripe_store.rs:226`) — no platform floor. The true connected account (`event.account`) is on the wire but **not even parsed**. Metering: `report_usage` keys on body `app_id` under the shared control-key with no app-set scoping and a blind additive upsert with **no idempotency key** (replays double-count). Connect `callback` links any `acct_` with no Stripe-API ownership proof.

**Fix — server-authoritative billing (focused refactor, ~4 sites + a new server-side Stripe client):**
- **Fee (B1):** move checkout-session creation server-side into a new control endpoint `POST /api/apps/:id/checkout` using the platform `STRIPE_SECRET_KEY` (new `AppState` field; control already has a `cyper` client). Server sets `application_fee_percent` from a server-held `PLATFORM_FEE_PERCENT` (=15) and derives `creator_id` from the authenticated app→owner and `acct_` from `creator_accounts`. Delete `applicationFeePercent` + the `metadata[creator_id]` contract from `sdks/payments/checkout.ts` (pre-launch: rename/delete, no shim). Defense-in-depth: parse `event.account`, switch the webhook to **reverse-lookup `creator_id` by account** (reject events whose account maps to no live link), and add a fee floor in `record_payout` (`net = gross - max(reported, expected)` + audit on underpayment).
- **Metering (B2):** add `report_id`/idempotency to `UsageReport` (`core/types.rs`) + a `usage_reports_seen` dedupe (no-op on replay); reject `app_id`s not in the deployed-app set in `report_usage`. Note the shared control-key remains the residual weakness (per-worker signed reports = the principled long-term fix). **Integrity only** — there's no live enforcement consumer (`crates/platform` is dead), so don't block on resurrecting spending limits; track that separately.
- **Connect (B4):** `GET /v1/accounts/{id}` ownership check before `link_account`, or bind linking to a real Connect OAuth/Account-Link return flow (replaces the placeholder `onboard`).

**Tests:** fee always = server rate regardless of client input; `record_payout(gross=10000, reported=0)` → stored fee ≥1500; forged `creator_id` with foreign `acct_` → recorded as the account's true owner or rejected; unknown account → parked; unknown app usage → rejected; duplicate `report_id` → counted once; foreign `acct_` link → rejected.

**Largest piece:** the server-side Stripe client in control doesn't exist yet (`onboard` is a placeholder). The ledger floor + attribution-by-account changes are small and self-contained and could land first.

---

## 4. NEW-2 (+RT-2) — SSRF NAT64 / v4-compat IPv6 + dev-mode presence checks  *(VERIFIED; RT-2 is 4 sites)*

**Confirmed:** `ssrf.rs:56-65` blocks v4-mapped but not `64:ff9b::/96` (NAT64) or `::a.b.c.d` (v4-compatible) — `64:ff9b::a9fe:a9fe`, `::7f00:1`, `::a9fe:a9fe` all pass. RT-2 is worse than recorded — **four** presence-only `ZEROSHIP_DEV` checks (`ssrf.rs:89`, `ssrf.rs:144`, `transport/client.rs:26`, `core/dev_auth.rs:74`), the last gating dev-auth on *any* value incl. `=0`.

**Fix:** v6 arm — block `segments[0]==0x0064 && segments[1]==0xff9b` (covers NAT64 + RFC 8215) and `segments[..6]==[0;6]` (all v4-compatible incl. `::`/`::1`). RT-2 — a pure `dev_mode_from(v) = v == Some("1")` + env wrapper, replace all four sites (vite sets `ZEROSHIP_DEV=1`, so `=="1"` is contract-faithful; dev_auth keeps its extra secret gate).

**Tests:** `blocks_nat64`, `blocks_v4_compatible`, `allows_public_v6` stays green; pure `dev_mode_from` unit (`None`/`""`/`"0"`→false, `"1"`→true, no env races).

---

## 5. CT-A2 — deploy rate-limit + concurrency cap  *(VERIFIED)*

**Confirmed:** `deploy()` (`api.rs:292`) has authz + content-type + 256MB size cap only; no rate-limit, no in-flight cap. The DB-backed `http_util::rate_limit` exists but hardcodes IP keying; `AuthzGuard.principal_id` makes per-creator keying trivial; no semaphore anywhere (zero-tokio — use a `Mutex<HashSet>` + `AtomicUsize`, not tokio::sync).

**Fix:** extract `rate_limit_key(db, key, quota)` from `http_util`; in `deploy` after authz, key on `control:deploy:user:{principal_id}` (e.g. 5/min). New `deploy_gate.rs`: `DeployGate { in_flight: Mutex<HashSet<Uuid>>, global: AtomicUsize, max_global }` with RAII `try_acquire(app_id)` → per-app 409 / global 503+`Retry-After`, acquired after content-type check so saturated callers never read the body. `AppState` field + `--max-concurrent-deploys` (default 8).

**Tests:** same-app re-acquire → busy; N+1 global → busy; permit drop releases; `rate_limit_key` PG-gated throttle on a `user:{uuid}` key.

---

## 6. CT-A3/A4 — app name = DNS label + reserved denylist  *(VERIFIED)*

**Confirmed:** `registry.rs:144-153` allows uppercase + `_` (invalid DNS labels — the name *is* the subdomain), `name TEXT UNIQUE` is case-sensitive (`foo`/`Foo` coexist), no reserved denylist (`console`/`auth`/`api` accepted on the shared apex). No rename path → create-time validation suffices.

**Fix:** `validate_app_name` = `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`, reject `xn--`, reject a `RESERVED_APP_NAMES` const (console, auth, api, www, admin, gateway, control, worker, oauth, login, billing, stripe, … + zeroship). New changeset `0034`: drop the case-sensitive unique, add `CREATE UNIQUE INDEX … (lower(name))` + `CHECK (name = lower(name))`. Lowercase the extracted subdomain in `dispatch.rs:74`. (Tightens charset — fixtures using `My_App` names change in the same PR; pre-launch, fine.)

**Tests:** rejects `Console`/`console`/`xn--foo`/`_x`/`x-`/64-char; accepts `my-app`; PG-gated: `create("foo")` then raw insert `Foo` → unique violation.

---

## 7. Low batch  *(all VERIFIED)*

- **CT-C1 reserved env names** — `env_store.rs` `reserved_key()` rejecting `APP_ID`, `ZEROSHIP_*`, `DATABASE_URL`, `WORKER_KEY`, `CONTROL_KEY`, `MASTER_KEY`, `NODE_OPTIONS`, `PATH`, `LD_PRELOAD` in set_var/set_secret/set_expose. (Bound confirmed: APP_ID self-shadows only the creator's own `process.env`, no cross-tenant impact — defense-in-depth.)
- **CT-C2 per-app key derivation** — `core/crypto.rs` `derive_app_key = HKDF-SHA256(master, info=app_id)`, per-app key in env_store (+ previous-keys for rotation), bump AAD prefix to v2. **Honest framing: does NOT mitigate master-key compromise** (re-derivable); gains = domain separation + groundwork for external KMS. Cost ≈0 pre-launch. Recommend doing it as groundwork.
- **platform_policies inert override** — confirmed truly inert (nothing merges the DB table into the enforced static set). **Recommend REMOVE** the write path (delete handlers + `Action::PlatformPoliciesWrite` + grant + `DROP TABLE`, short ADR) rather than wire it — static embedded Cedar is the stronger invariant; wiring DB→enforcement makes DB compromise an authz bypass for a capability nobody uses.
- **CONTROL_KEY strength floor** — `validate_control_key_material` (≥32B) in `core/config/secrets.rs`, wired in `main.rs` like the worker-key (note: distinct name from the existing `validate_control_key` constant-time *comparator*).
- **NEW-3 kv.list prefix** — `validate_prefix` (allows empty) in `list()`, add `{`/`}` to `escape_glob`. (Even lower than filed: braces can't widen the Redis MATCH or reroute the scan; pure hygiene/consistency.)

---

## Cross-cutting implementation notes

- **No tokio** (compio/io_uring only) — the deploy gate uses `std`/`Atomic`, the runtime watchdog reuses the existing POSIX-timer thread.
- **Pre-launch, no back-compat** — delete/rename outright (SDK fee contract, app-name charset, AAD v2, platform_policies); update every caller + fixture + doc in the same patch.
- **Faithful e2e + regression-test-per-fix** — each fix lands with a test that fails pre-fix and runs the real path (esp. the RT-5 busy-loop e2e and the billing fee floor).
- **Implementation isolation** — these touch overlapping crates (control, runtime); implement sequentially or in separate worktrees to avoid concurrent cargo conflicts. Commit per fix; do not push.
