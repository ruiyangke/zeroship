# Platform Gap Analysis — "What's Missing" — 2026-06-11

> Five parallel read-only capability audits (creator journey · platform primitives/SDKs ·
> billing/monetization · production readiness · multi-tenant scale) against `main` HEAD.
> Question: not "is it correct/secure" (covered in the 2026-06-{03,09,11} security reviews)
> but **what capability is absent or stubbed** for zeroship to be a working product per its
> stated vision (no-code AI app platform: hosting · DB · auth · payments · scaling, 15% cut).

## Framing — the core is strong; the gaps are the business + ops + scale layer

What's genuinely built and solid: the V8/compio runtime + sandbox boundary, the four app
primitives (`env.{db,kv,storage,auth}`) wired in the worker, the `.zship` deploy pipeline +
content-addressed blob ingest, gateway manifest dispatch + CHWBL, the full auth IdP
(password/OAuth/magic-link/sessions/GDPR-erase), structured logging, the Liquibase migration
mechanism, and the deploy contract (`default = {fetch?, rpc?}`). The runtime can run
a real app.

What's missing clusters into four areas, in priority order: **(0) the monetization engine
doesn't function end-to-end** — the literal reason the platform exists; **(1) production data
durability** — it can't safely hold what creators ship; **(2) the production/ops layer** —
it can't go on the public internet as-is; **(3) scale infrastructure** — it breaks past a
few dozen tenants; and **(4) a set of product capabilities** real apps need.

---

## Tier 0 — The monetization engine is non-functional end-to-end

This is the single biggest gap: "the platform takes 15%" cannot operate today. Four audits
converged here. A creator **can** get paid (Stripe Connect direct charges settle to their
account automatically), but **the platform cannot collect its cut, and there is no usage
billing at all.**

- **No `env.meter` primitive** — apps cannot record billable units. Documented as "planned"; no `MeterPlugin` in the worker. (`crates/worker/src/cache.rs`)
- **No usage producer** — the control plane has a `POST /internal/usage` ingest + `UsageReport` type, but **nothing on the worker/gateway ever emits one**. The billing pipe is open on the receiving end with nothing feeding it. (`crates/control/src/internal.rs:150`)
- **`metering.rs` is a 1-line stub.** No usage → pricing → invoice/charge pipeline exists.
- **The 15% fee is not server-enforced** — it's a client-side default in creator-controlled code (`sdks/payments/checkout.ts`, `applicationFeePercent ?? 15`), overridable to 0 or bypassable. The webhook only records what Stripe reports; no floor, no creator↔account binding. (filed CT-B1)
- **Spending-limit enforcement is dead code** — a complete-looking metering/billing/enforcement engine (~2000 LOC) lives in `crates/platform/`, which is **workspace-excluded and tokio-based**, so it violates the zero-tokio invariant and can never run. It needs a from-scratch compio reimplementation, not a port.
- **Stripe Connect onboarding is a placeholder** — `stripe_handlers.rs::onboard` returns a hardcoded URL; no real `/v1/account_links` call, no `acct_` ownership verification.
- **No monetization UI** — the Stripe backend + SDK are real but there's no creator surface to onboard, set pricing, or see payouts, and no end-user checkout wiring.
- **Plan→pricing is disconnected** — `plan_id` is free-text, self-escalatable to `unlimited` (CT-A1), gates only runtime limits, maps to no pricing.

**To make the business model real:** server-side checkout that stamps the fee from a
platform-held rate (CT-B1) → real Stripe onboarding → an `env.meter` primitive + worker
usage producer → a compio reimplementation of metering→pricing→spending-limits → a creator
monetization UI.

---

## Tier 1 — Launch-blocking infrastructure (can't durably hold data / serve the internet)

- **No production object storage.** `crates/plugin-storage` and `crates/bundle` ship **only `LocalFs`/`LocalDiskBlobStore`**; `S3BlobStore` and the `s3` plugin backend are comment-only (`blob.rs:48`, no `s3.rs`). Every creator's app bundles + all `env.storage` user data sit on one node's disk, no replication. **A node/disk loss is irrecoverable.**
- **No platform backups / DR.** The only backup doc is auth-scoped `pg_dump` (`auth-deploy.md §7`); no backup/PITR/WAL-archiving for control, per-app tenant schemas, or the blob store. Postgres is a single instance — its death is a total outage.
- **No production TLS / edge / cert automation.** Services bind plain HTTP; `ops/Caddyfile` is dev-only (`auto_https off`, `*.zeroship.localhost`). No wildcard or custom-domain ACME. The platform can't go on the public internet as-is.
- **DB connection exhaustion at modest scale.** The plugin-db pool is **8 connections per app**, one per isolate context; `8 × hundreds-of-isolates × threads × nodes` blows past Postgres's ~1k ceiling almost immediately. No PgBouncer/PgCat. **Caps real concurrent app count to a few dozen** until a transaction-mode proxy lands.

---

## Tier 2 — GA-blocking (before public GA)

- **No deploy history / rollback** — control stores a single overwrite-only `deploy_hash`; no `deploys` table, no `rollback`. A bad AI-generated deploy is unrecoverable — uniquely dangerous when creators can't hand-fix. (ISS-15)
- **No custom domains** — gateway routes by subdomain label only; no domain→app table, DNS verification, or per-domain TLS. Every serious creator needs their own domain. (ISS, roadmap 1.7)
- **WebSocket subscriptions are `501` in the multi-node gateway** — only single-tenant `zeroship serve` supports WS. Realtime apps don't work in prod. (`gateway/router/dispatch.rs`)
- **Edge observability blackout** — no `/metrics` on gateway/control/auth (only worker + sandbox), no latency histograms, no distributed tracing, no dashboards/alerting. Operators are blind on the edge hot path.
- **No CD / prod orchestration** — CI builds+tests but never publishes an image or deploys; no k8s/Nomad/Terraform for the platform itself; gateway/control/auth lack graceful shutdown (in-flight requests dropped on redeploy). "Multi-node" is `docker compose --scale` on one host.
- **No prod secrets backend** — Vault / AWS-SM `SecretRef`s are stubs (`BackendUnavailable`); secrets live in env/files with no rotation runbook for the core keys.
- **Static worker fleet** — the CHWBL ring is built once at gateway boot from `--workers`; no dynamic membership, no health-based ejection, no autoscaler. A dead worker stays in the ring; scaling needs a restart.
- **2FA / TOTP** — no MFA; creators control money via Stripe Connect. (ISS-11 — in progress in the fix loop.)

---

## Tier 3 — Capability completeness (real apps need these)

- **No scheduled/cron primitive** — background/periodic work (digests, reminders, polling) is impossible; "scheduled workers" only fire when manually curl'd. (ISS-28)
- **End-user authorization (P12) unbuilt** — no `env.authz`, no `@zeroship/permissions`, no bundle `policies.cedar`. The Cedar engine exists but is wired only into the control plane (platform RBAC), never the worker/runtime. Apps needing per-user access control must hand-roll it in `env.db`.
- **Missing ecosystem SDKs** — `@zeroship/email` and `@zeroship/ai` are absent; `@zeroship/payments` is thin. Common generated-app needs (send email, call an LLM) have no first-party wrapper.
- **No non-additive migrations** — only `CREATE TABLE`/`ADD COLUMN IF NOT EXISTS`; no rename/type-change/drop/backfill. Limits real schema iteration. (roadmap 1.5)
- **node-compat build↔runtime mismatch** — the vite-plugin shims `node:{fs,http,child_process,module,timers,url}` that the runtime does **not** register; such imports build but fail/no-op at runtime.
- **No `env.assets`** runtime static-asset writes (build-time assets only).
- **Per-tenant fairness + quotas** — ~200 isolates share a thread's executor with only an after-the-fact CPU-kill watchdog; one app can starve co-tenants. No concurrency cap, no fair queue, load-blind LRU. No per-app KV/storage aggregate quotas (KV unbounded, storage unbounded).

---

## Tier 4 — Post-launch / scale-time

- **No teams / collaborators / organizations** — single-owner apps only.
- **No V8 snapshots** — cold start pays full compile+eval; the <10ms snapshot path (the economic unlock for stateless workers) is unbuilt.
- **In-flight work dropped on evict/redeploy** — no drain timer (W-2).
- **Multi-region / HA** — single region, no data replication, control plane + Postgres + Redis + blob all SPOFs (honestly acknowledged in `distributed.md`).
- **Sandbox at scale** — no warm pool (every build cold-starts), fixed per-node capacity, no cross-node placement; egress unfiltered (SB-A1).
- **Stateless-worker migration** + storage-S3 + PgBouncer + sharding + config-bus reconfigure — the `plugins-workers-distributed` proposal legs beyond the shipped KV→Redis.

---

## The five gaps to fix first (cross-domain)

1. **Stand up the metering→billing→fee-enforcement spine** (Tier 0) — `env.meter` + worker producer + server-enforced 15% checkout + a compio metering/spend engine. Without it the platform earns nothing and has no usage data. This is the product's reason to exist.
2. **Production object storage + DB/object backups** (Tier 1) — implement `S3BlobStore` + the plugin-storage `s3` backend; add platform-wide backup/PITR. Until then a disk loss is fatal.
3. **Production TLS + custom-domain cert automation** (Tier 1/2) — the per-app/custom-domain promise has no implementation and the stack serves plain HTTP.
4. **DB connection proxy (PgBouncer/PgCat)** (Tier 1) — the 8-conn-per-app pool caps the platform at ~dozens of apps; a transaction-mode proxy is the smallest fix with the largest scale unlock.
5. **Deploy history + rollback** (Tier 2) — a one-way `deploy_hash` with no revert is uniquely dangerous for AI-generated deploys; pair with edge observability so operators can see and recover from a bad rollout.

> Note: several Tier 2/3 product gaps (deploy-in-AI-loop, monetization UI, observability surfaces,
> rollback UI) are coupled to the **builder rewrite** already planned — fold them into that scope
> rather than bolting onto the current builder.
