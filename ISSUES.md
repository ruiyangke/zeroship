# Known Issues

Open platform-level gaps with no full fix landed. Each entry is a self-contained note:
what's actually wrong/absent, the fix direction + rough effort, and dependencies.

> **2026-06-11 — platform capability gaps added.** A five-domain "what's missing" audit
> (`docs/reviews/2026-06-11-platform-gap-analysis.md`) found the core runtime/primitives/
> deploy/auth solid, but four clusters absent: the monetization engine, prod data durability,
> the prod/ops layer, and scale infrastructure. Those are tracked below as **ISS-29…ISS-52**
> (plus three pre-existing items — ISS-15/18/28 — re-scoped here as platform gaps, having
> been mis-filed as builder-coupled). Builder-UI-coupled issues remain
> [deferred](#deferred--superseded-by-the-builder-rewrite) pending the rewrite.

## Legend

**Status** — `open` · `re-scoped` (corrected 2026-06-11) · `partial` (substrate exists,
exposure missing).
**Tiers** — `T0` business model non-functional · `T1` launch-blocking infra · `T2` GA-blocking ·
`T3` capability completeness · `T4` post-launch / scale-time.

## Status at a glance

**Auth — all shipped 2026-06-11:** ISS-12 (GDPR erase, `3a9b2315`+`a0d23e8c`), ISS-10
(session visibility, `de38f943`), ISS-11 (TOTP 2FA, `7ab11964`), ISS-12b (orphaned-app
reaper, `42efc20a`). No open auth issues remain — the rest are platform epics + deferred
builder.

**Platform capability gaps (from the gap analysis):**

| Tier | Issues |
|---|---|
| **T0** monetization | ISS-18 metering pipeline · ISS-29 server-enforced fee · ISS-30 Stripe onboarding · ISS-31 billing/spend engine (+ retire dead `crates/platform`) |
| **T1** launch infra | ISS-32 prod object storage (S3/R2) · ISS-33 backups/DR · ISS-34 prod TLS/edge · ISS-35 DB connection proxy |
| **T2** GA | ISS-15 deploy history+rollback · ISS-36 custom domains · ISS-37 edge observability · ISS-38 CD + prod orchestration · ISS-39 prod secrets backend · ISS-40 dynamic worker fleet · ISS-43 WebSocket-in-gateway |
| **T3** capability | ISS-28 cron/scheduled primitive · ISS-41 end-user authz (P12) · ISS-42 `@zeroship/{email,ai}` SDKs · ISS-44 non-additive migrations · ISS-45 node-compat align · ISS-46 per-tenant fairness/quotas · ISS-47 `env.assets` writes |
| **T4** scale/later | ISS-48 teams/orgs · ISS-49 V8 snapshots · ISS-50 multi-region/HA · ISS-51 sandbox scale · ISS-52 stateless-worker migration |
| **test infra** | ISS-53 e2e_platform.sh broken vs current config · ISS-54 no gateway-E2E coverage for primitives (+storage/auth/kv examples) |

**Deferred (builder rewrite):** ISS-13, ISS-14, ISS-16, ISS-17, ISS-24 (+ monetization UI).

---

> **Operator decision still open (ISS-12):** confirm the billing-retention default in
> `account_reaper::user_has_financial_history` (anonymize creators-with-Stripe vs hard-delete).
> And confirm the dedicated `AUTH_TOTP_ENC_KEY` choice (ISS-11) vs HKDF from an existing secret.

---

> **Platform capability gaps** — detail + evidence: `docs/reviews/2026-06-11-platform-gap-analysis.md`.

## T0 — The monetization engine is non-functional end-to-end

"The platform takes 15%" cannot operate today. A creator *can* get paid (Stripe Connect
direct charges settle to them); the platform *cannot* collect its cut, and there is no usage
billing at all.

### ISS-18 · No metering pipeline (`env.meter` + usage producer)
**Status:** re-scoped (was "perf metering for HealthCanvas") · **Effort:** L · **Tier:** T0

No `env.meter` primitive is registered (worker registers only db/kv/storage/auth). The
control plane has a `POST /internal/usage` ingest + `UsageReport` type, but **nothing on the
worker/gateway emits one** — the pipe is open with nothing feeding it. `metering.rs` is a
1-line stub. Blocks metered billing AND observability. **Fix:** `MeterPlugin` + worker usage
producer (per-tenant scoped, idempotent — see CT-B2) + a real aggregation in `metering.rs`.

### ISS-29 · Platform fee (15%) not server-enforced
**Status:** open · **Effort:** M · **Tier:** T0 · ref CT-B1

The fee is a client-side default in creator-controlled code (`sdks/payments/checkout.ts`,
`applicationFeePercent ?? 15`), overridable to 0 / bypassable; the webhook only records what
Stripe reports (no floor, no creator↔account binding). **Fix:** move checkout-session
creation server-side so control stamps `application_fee_percent` from a platform-held rate
using the platform key; the creator receives only a session URL. Breaks the
`@zeroship/payments` SDK contract (pre-launch — rewrite it).

### ISS-30 · Stripe Connect onboarding is a placeholder
**Status:** open · **Effort:** M · **Tier:** T0 · ref CT-B4

`stripe_handlers.rs::onboard` returns a hardcoded Express URL; no `/v1/account_links` call,
no `STRIPE_SECRET_KEY` use, and `callback` links an `acct_` without verifying ownership.
**Fix:** real Account Link / OAuth onboarding + verify the returning `acct_` belongs to the
platform/creator before binding payouts.

### ISS-31 · No metered-billing / spending-limit engine (live)
**Status:** open · **Effort:** L · **Tier:** T0

A complete-looking metering/billing/enforcement engine (~2000 LOC) exists in `crates/platform`
but is **workspace-excluded and tokio-based** — it violates the zero-tokio invariant and can
never run; nothing in the live binaries enforces spending limits or bills off usage. **Fix:**
reimplement metering→pricing→spending-limit (`SpendAction::{Allow,Warn,Degrade,Block}`) in the
compio stack (consumes ISS-18), then **delete the dead `crates/platform`** so it stops
masquerading as a billing engine. Also: `plan_id` is free-text/self-escalatable (CT-A1) with
no pricing model — add a server-side plan catalog (see the remediation plan).

## T1 — Launch-blocking infrastructure

### ISS-32 · No production object storage (S3/R2)
**Status:** open · **Effort:** M–L · **Tier:** T1

`crates/plugin-storage` and `crates/bundle` ship **only `LocalFs`/`LocalDiskBlobStore`**;
`S3BlobStore` + the `s3` plugin backend are comment-only (no `s3.rs`). All app bundles + every
`env.storage` upload sit on one node's disk, no replication. **Fix:** implement `S3BlobStore` +
the plugin-storage `s3` backend behind the documented flag; make `--blob-store` accept an S3/R2
target. (Multi-worker `env.storage` correctness depends on this; today it needs a shared-FS hack.)

### ISS-33 · No platform backups / disaster recovery
**Status:** open · **Effort:** M · **Tier:** T1

The only backup doc is auth-scoped `pg_dump` (`auth-deploy.md §7`). No backup/PITR/WAL-archiving
for control, per-app tenant schemas, or the blob store; Postgres is a single instance. A disk
loss is irrecoverable. **Fix:** platform-wide DB backup + PITR (pgbackrest/wal-g), object-store
durability/backup, and a tested restore runbook.

### ISS-34 · No production TLS / edge / cert automation
**Status:** open · **Effort:** M · **Tier:** T1

Services bind plain HTTP; `ops/Caddyfile` is dev-only (`auto_https off`, `*.zeroship.localhost`).
No production TLS termination, no wildcard ACME for `*.zeroship.ai` / auth / console / api.
**Fix:** a prod edge (Caddy on-demand-TLS or equivalent) with wildcard + per-host certs.
(Custom-domain certs are ISS-36.)

### ISS-35 · DB connection exhaustion (no connection proxy)
**Status:** open · **Effort:** S–M · **Tier:** T1

The plugin-db pool is **8 connections per app**, one per isolate context; `8 × hundreds-of-
isolates × threads × nodes` blows past Postgres's ~1k ceiling almost immediately — caps real
concurrent app count to ~dozens. **Fix:** a transaction-mode proxy (PgBouncer/PgCat) in front
of Postgres (proposal Stage 1). Smallest fix, largest scale unlock.

## T2 — GA-blocking

### ISS-15 · No deploy history / rollback
**Status:** re-scoped (platform backing, was builder-coupled) · **Effort:** M · **Tier:** T2

Control stores a single overwrite-only `deploy_hash` (`registry.rs:296`); no `deploys` table,
no `rollback`. A bad AI-generated deploy is unrecoverable — uniquely dangerous when creators
can't hand-fix. **Fix:** a `deploys` history table + a rollback endpoint that re-points the
manifest/hash. (Rollback UI couples to the builder rewrite.)

### ISS-36 · No custom domains
**Status:** open · **Effort:** M · **Tier:** T2

The gateway resolves host→app by subdomain label only (`gateway/src/sync.rs:83 lookup_by_name`);
no domain→app table, DNS-TXT verification, or per-domain cert provisioning. **Fix:** a
custom-domains table + verification flow + on-demand TLS for verified domains.

### ISS-37 · Edge observability blackout
**Status:** open · **Effort:** M · **Tier:** T2

No `/metrics` on gateway/control/auth (only worker + sandbox), no latency histograms, no
distributed tracing across gateway→worker→control, no dashboards/alerting. Operators are blind
on the edge hot path. **Fix:** metrics endpoints + histograms on all services, trace
propagation, a scrape/dashboard/alert config.

### ISS-38 · No CD / prod orchestration / graceful shutdown
**Status:** open · **Effort:** M–L · **Tier:** T2

CI builds+tests but never publishes an image or deploys; no k8s/Nomad/Terraform for the platform
itself; gateway/control/auth lack `shutdown_timeout`/draining (in-flight requests dropped on
redeploy). **Fix:** image publish + release tagging, prod orchestration manifests, zero-downtime
rollout + graceful shutdown on all services.

### ISS-39 · No production secrets backend
**Status:** open · **Effort:** M · **Tier:** T2

The `SecretRef` Vault / AWS-SM backends are stubs (`BackendUnavailable`); prod secrets can only
come from env/files, with no rotation runbook for the core keys (MASTER/WORKER/CONTROL/Stripe/
OAuth/signing). **Fix:** implement one dynamic backend (compio HTTP — no tokio AWS/Vault SDK) +
a rotation procedure. (CONTROL_KEY also lacks a strength floor — see the security remediation plan.)

### ISS-40 · Static worker fleet — no health ejection / autoscaling
**Status:** open · **Effort:** M · **Tier:** T2

The CHWBL ring is built once at gateway boot from `--workers` (`gateway/main.rs:582`); no dynamic
membership, no health-based ejection, no autoscaler — a dead worker stays in the ring, scaling
needs a restart. **Fix:** dynamic worker registration + health-checked ejection; later, an
autoscaler/backpressure controller.

### ISS-43 · WebSocket subscriptions return 501 in the multi-node gateway
**Status:** open · **Effort:** M–L · **Tier:** T2

Only single-tenant `zeroship serve` supports WS; the multi-node gateway returns
`501 UNIMPLEMENTED` (`gateway/router/dispatch.rs`). Realtime apps don't work in prod. **Fix:**
wire subscription dispatch through the gateway (and a cross-worker pub/sub for fan-out — see
ISS-52). Streams/WebSockets are also TCP-pinned to their initial worker.

## T3 — Capability completeness

### ISS-28 · No scheduled / cron primitive
**Status:** re-scoped (platform primitive, was builder-coupled) · **Effort:** M · **Tier:** T3

No app-level scheduled-worker primitive; `crates/{control,auth}/cron` are process-lifetime
platform sweeps, not an app-job scheduler. "Scheduled workers" only fire when manually curl'd.
Blocks any app needing background/periodic work (digests, reminders, polling). **Fix:** a
scheduler + an `env.cron`/scheduled-handler primitive + the out-of-band invocation path.

### ISS-41 · End-user authorization (P12) unbuilt
**Status:** open · **Effort:** L · **Tier:** T3

The Cedar engine (`crates/authz`) is wired only into the control plane (platform RBAC, P9). The
P12 end-user layer — `env.authz`, bundle `policies.cedar`, `@zeroship/permissions` — is absent;
apps needing per-user access control must hand-roll it in `env.db`. **Fix:** per the active
`docs/proposals/authorization.md` P12 design (worker/runtime Cedar + bundle policy + SDK).

### ISS-42 · Missing ecosystem SDKs (`@zeroship/email`, `@zeroship/ai`)
**Status:** open · **Effort:** M · **Tier:** T3

`@zeroship/email` and `@zeroship/ai` are absent; `@zeroship/payments` is thin. Common
generated-app needs (send email, call an LLM) have no first-party wrapper. **Fix:** ship
`@zeroship/email` (fetch-based provider wrapper) + `@zeroship/ai` (Claude/OpenAI wrapper) as
npm packages over `fetch` (no new native primitive).

### ISS-44 · No non-additive migrations
**Status:** open · **Effort:** M · **Tier:** T3

Per-app schema migrations are additive-only (`CREATE TABLE`/`ADD COLUMN IF NOT EXISTS`); no
rename/type-change/drop/backfill. Limits real schema iteration. **Fix:** a richer migration
plan/apply path with safe expand-contract for destructive changes.

### ISS-45 · node-compat build↔runtime mismatch
**Status:** open · **Effort:** S–M · **Tier:** T3

The vite-plugin shims `node:{fs,http,child_process,module,timers,url}` that the runtime does
**not** register (`native_modules.rs` has async_hooks/buffer/crypto/zlib/os/path/util only) —
such imports build but fail/no-op at runtime. **Fix:** align the two surfaces (implement the
missing synthetics, or reject the import at build time with a clear error).

### ISS-46 · No per-tenant fairness / aggregate quotas
**Status:** open · **Effort:** M–L · **Tier:** T3

~200 isolates share a thread's executor with only an after-the-fact CPU-kill watchdog; one app
can starve co-tenants (no concurrency cap, no fair queue, load-blind LRU). No per-app aggregate
KV/storage quotas (both unbounded — KV-1, ST-2/3). **Fix:** per-isolate scheduling/concurrency
caps + per-tenant KV/storage byte/key quotas.

### ISS-47 · No `env.assets` runtime writes
**Status:** open · **Effort:** S–M · **Tier:** T3

The `runtime_assets` manifest field + gateway static-serve exist, but there is no runtime
primitive for an app to write assets at runtime (build-time assets only). **Fix:** an
`env.assets` CRUD primitive backed by the blob store.

## T4 — Post-launch / scale-time

### ISS-48 · No teams / collaborators / organizations
**Status:** open · **Effort:** L · **Tier:** T4 — single-owner apps only; no member/invite model.

### ISS-49 · No V8 snapshots (cold-start cost)
**Status:** open · **Effort:** L · **Tier:** T4 — `load_app` compiles+inits on first request; the
<10ms snapshot fast path (the economics of the stateless-worker model) is unbuilt.

### ISS-50 · Single-region, no HA / replication
**Status:** open · **Effort:** L · **Tier:** T4 — control plane + Postgres + Redis + blob store are
all SPOFs (acknowledged in `distributed.md`). No streaming replica, no multi-region route propagation.

### ISS-51 · Sandbox not scaled for many concurrent builds
**Status:** open · **Effort:** L · **Tier:** T4 — no warm pool (every build cold-starts), fixed
per-node capacity, no cross-node placement; egress unfiltered (SB-A1).

### ISS-52 · Stateless-worker migration incomplete
**Status:** open · **Effort:** L · **Tier:** T4 — the `plugins-workers-distributed` legs beyond
KV→Redis: storage-S3 (ISS-32), PgBouncer (ISS-35), sharding, config-bus reconfigure, cross-worker
realtime pub/sub, and the stateless-worker flip itself. In-flight work is also dropped on
evict/redeploy (no drain — W-2).

---

## E2E test findings (2026-06-11)

Issues surfaced while exercising the framework end-to-end. Detail + ledger:
`docs/reviews/2026-06-11-e2e-test-log.md`.

### ISS-53 · `tests/e2e_platform.sh` is broken against current code
**Status:** open · **Effort:** S–M · **Tier:** T2 (test infra)

The platform's own multi-node E2E harness has rotted and silently can't start the stack:
control refuses to boot because `WORKER_KEY` (≥32B) and `SIGNING_KEY_FILE` are now required
(server-config hardening) and the harness sets neither, using short keys (`test-ck`/`test-mk`).
Also stale: `zeroship deploy --key=` (now `--token=`), and an **unqualified
`DROP TABLE … apps CASCADE`** (line 108) that — with the `zeroship` search_path — drops
`zeroship.apps` + dependents, corrupting the target schema. **Fix:** modernize the harness
(`--dev-insecure` or real keys + a signing-key file; `--token`; schema-qualified / temp-DB
cleanup; deploy against a clean Liquibase-migrated DB). Verified: control starts only with
`--dev-insecure`.

### ISS-54 · No end-to-end test coverage for app primitives through the gateway
**Status:** open · **Effort:** M · **Tier:** T2 (test infra)

Nothing exercises `env.db/kv/storage/auth` over the real multi-node edge. `e2e_platform.sh`
deploys trivial inline JS (dispatch plumbing only); `examples/db-e2e` tests db richly but via
single-tenant `zeroship serve` (bypasses the gateway, SQLite not PG). The whole
"E2E-through-gateway" column is empty. Compounding coverage gaps: **`env.storage` has zero
example** (G2), **`env.auth` has zero example** so the gateway `ZeroShip-User`→worker
`AuthPlugin` identity chain is never tested (G3), and `kv-dashboard` has no test runner (G4).
**Fix:** a gateway-E2E harness (`e2e_app_primitives.sh`) deploying a real built example through
control→gateway→worker + asserting the primitives over the edge, plus `storage-gallery` /
`auth-notes` examples + a kv runner.

---

## Deferred — the app-builder will be re-implemented

**The `apps/zeroship-builder` is deferred for a from-scratch re-implementation** (operator
decision, 2026-06-11). Do not fix issues against the current builder — they are coupled to UI
that is going away. Re-triage all of these after the re-impl defines what it actually needs;
fold the builder-side surfaces of the platform gaps above (rollback UI, monetization UI,
observability/incidents views, data/migrations canvas) into that new scope. Full detail of the
items below is in git history (pre-`7ad090ff`).

- **ISS-14** · builder seeds 3 fabricated issues into the PM agent (`agents.ts seedIssues()`).
- **ISS-16** · builder reports an invented "B+" scorecard to the PM agent (`agents.ts defaultScores()`).
- **ISS-13** · skill registry catalogue-only; `/skills` ships a disabled "Add to project" CTA.
- **ISS-17** · no incidents timeline (HealthCanvas + SRE monitor; backing also needs ISS-18 + ISS-28).
- **ISS-24** · per-app migration journal exists (`__zeroship_migrations`) but no UI exposure (engine gap is ISS-44).
- **Monetization UI** · the Stripe/payments backend (ISS-29/30/31) needs a creator-facing surface — onboard Connect, set pricing, see payouts, end-user checkout. New work for the rewrite.
