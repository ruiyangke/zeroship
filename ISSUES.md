# Known Issues

Open platform-level gaps with no full fix landed. Each entry is a self-contained note:
what's actually wrong today, the fix + rough effort, and dependencies. Closed and
removed-surface entries were dropped on 2026-06-11; they live in git history (this file
before commit `9fdf9904`).

> **Context (re-verified 2026-06-11).** Two architectural shifts shape how the entries
> below are scoped — and invalidated several older "control-plane table" fix-paths:
>
> 1. **The data/health/plan/media canvases were deleted** (`4110071f`, 2026-05-27 —
>    workspace re-cut to a tier-gated `preview/files/logs/env/settings` surface). Some
>    issues below are gated on such a surface returning.
> 2. **The console is a pure creator app** (2026-05-31 — zero control-plane calls,
>    enforced by `apps/zeroship-builder/src/server/no-control-import.test.ts`). So
>    "add a control-plane table/column" is the wrong shape; creator-scoped
>    `@zeroship/kv` is the sanctioned store, and it is durable now (redb dev / Redis prod).

## Legend

**Status** — `open` · `re-scoped` (open, but corrected 2026-06-11 — the title/severity/
fix-path were updated) · `partial` (substrate exists, exposure missing).

**Tiers** — `T1` pre-launch, small, actively harmful · `T2` GA-blocking (compliance) ·
`T3` post-launch power/security · `T4` dormant or gated on a product decision.

## Status at a glance

**11 open issues.**

| Tier | Issue | Status | Effort |
|---|---|---|---|
| **T1** | ISS-14 · de-fake seeded issues | re-scoped | S |
| **T1** | ISS-16 · kill fake `defaultScores()` "B+" | re-scoped | S |
| **T2** | ISS-12 · account deletion / GDPR | open (unblocked) | M |
| T3 | ISS-10 · session visibility on `/me` | re-scoped | S–M |
| T3 | ISS-11 · 2FA / TOTP | open | M–L |
| T4 | ISS-28 · cron / scheduled-worker harness | open | M |
| T4 | ISS-18 · perf metering / `env.meter` | open | L |
| T4 | ISS-17 · incidents (needs 18 + 28) | open | L |
| T4 | ISS-13 · skill registry (only launch-visible) | open | L |
| T4 | ISS-15 · deploy history (gated on deploy returning) | open | M |
| T4 | ISS-24 · migration-log exposure (journal exists) | partial | S–M |

---

## T1 — pre-launch · small · actively harmful

The two T1 items both feed **fabricated data to the PM/SRE agents today**, so the
platform reasons over fiction. Both are S-effort.

### ISS-14 · Builder seeds fabricated issues into the PM agent
**Status:** re-scoped (was "issues table missing") · **Effort:** S

`apps/zeroship-builder/src/server/agents.ts` `seedIssues()` lazily injects 3 invented
issues ("Welcome to your project plan", "Wire up password reset", "Initial deploy") on
first read of any project, and that fiction is consumed by the PM digest agent
(`pm-worker.ts:125`) and the @-mention list (`MentionDropdown.tsx`). PlanCanvas (the
composer) is deleted, so there is no create/update path and a real table is **not** the
fix.

**Fix:** drop the seeds — return an empty list for a new/ungraded project. A real write
path, if ever wanted, is creator-scoped KV (not a control-plane table — pure-creator-app).

### ISS-16 · Builder reports an invented "B+" scorecard to the PM agent
**Status:** re-scoped (was "Critic → scoreboard wiring") · **Effort:** S

The Critic writer is wired (`internal/middleware.ts:193` → `setQualityFromCritic`) and the
reader exists (`agents.ts:233 getQualityScores`, KV-backed). But `agents.ts defaultScores()`
returns a hardcoded **"B+"** overall with invented per-dimension grades for never-graded
apps, and that feeds the PM agent (`pm-worker.ts:126`). HealthCanvas (the grid) is deleted,
so per-deploy persistence is moot.

**Fix:** replace `defaultScores()` with an honest "not graded yet" shape. Per-deploy keying
depends on ISS-15 and is deferred.

## T2 — GA-blocking (compliance)

### ISS-12 · No account-deletion / GDPR-erase path
**Status:** open · **Effort:** M · **Unblocked** (mailer now exists)

No delete/erase path in `crates/auth` or `crates/control` (only a `disabled_at` soft-disable
column with no user-space setter). Schema readiness is partial: 10/15 FKs to `zeroship.users`
are `ON DELETE CASCADE`, but 5 are not (`0004_control.sql:93,224,256,270` creator/billing +
attribution rows; `0003_platform.sql:12`), so a hard `DELETE` is blocked for any creator with
a Stripe/billing row.

GA-blocking for EU sign-ups (Art. 17), not launch-blocking — early requests can be fulfilled
manually within the ~30-day window. The ToS/privacy pages already promise it.

**Fix:** request → grace-period → hard-delete job (host in the `crates/auth` `cron/` module);
a SET-NULL/anonymize decision for the 5 non-cascade FKs; Stripe-Connect retention handling;
blob/bundle cleanup for owned apps. The confirm/undo email is free now that the mailer exists.

## T3 — post-launch · power / security

### ISS-10 · No active-session list / single-session revoke
**Status:** re-scoped (was "/auth/sessions endpoints") · **Effort:** S–M

The `auth_sessions` table the old title assumed is gone; the model is `idp_sessions` +
per-app `gateway_sessions`. Security-critical revocation **already exists**: password-reset
cascade (`ui/reset.rs`), RP-/backchannel-logout (`crates/gateway/src/backchannel_logout.rs`),
`credential_version` bumps. Only the **visibility** layer is missing — list a user's active
sessions (device/IP/UA) and revoke one without a password change.

**Fix:** a `list_by_user` union over the two session tables + two handlers on the existing
auth-service `/me` page. Target `crates/auth`, not control.

### ISS-11 · No two-factor (TOTP) enrollment
**Status:** open · **Effort:** M–L

Zero MFA code (grep finds only a `token_handlers.rs:512` placeholder and an unrelated
"two factors" comment in `ui/link.rs`). Not a sign-up blocker — password + Google/GitHub
OAuth + magic-link all work. Good GA hardening since creators control money via Stripe Connect.

**Fix:** challenge in the auth-service `/login` flow before `accept_login`, + encrypted-at-rest
secret storage + backup codes + enrollment on `/me` (benefits from ISS-10's `/me` security
card landing first).

## T4 — dormant / gated (defer)

### ISS-28 · No cron / scheduled-worker harness
**Status:** open · **Effort:** M

`pmDigest`/`sreMonitor` procs work standalone but **never fire** — shipped-but-inert (no fake
data shown). `crates/control/src/cron` and `crates/auth/src/cron` are process-lifetime platform
sweeps (audit-retention, token-sweep), not an app-job scheduler; no app-level scheduled-worker
primitive exists in runtime/bundle. The scheduler loop is small (copy the `cron::spawn_all`
pattern); the new work is the out-of-band chat-thread write path (`data-pm-recommendation`
chunks without a `useChat` round-trip). Platform-primitive pair with ISS-18.

### ISS-18 · No performance-metering pipeline (`env.meter`)
**Status:** open · **Effort:** L

`sre-worker.ts:134` emits an honest placeholder ("Performance metrics: <pipeline not wired
yet>"). `crates/control/src/metering.rs` is a 1-line stub; `crates/platform`'s metering is
workspace-excluded dead code with no live consumer; no `MeterPlugin`/`env.meter.*` is
registered (worker registers only db/kv/storage/auth). **Fix:** runtime instrumentation +
time-series store + aggregation API + UI. Feeds ISS-17; pairs with ISS-28.

### ISS-17 · No incidents timeline
**Status:** open · **Effort:** L

Fully dormant — no incidents RPC (only a comment at `internal/sre.ts:17`), no `incidents`
table, nothing fires the SRE monitor, and HealthCanvas is deleted so even the empty state is
gone. Nothing fake is shown. Gated: needs ISS-18 (detection signal) + ISS-28 (scheduler) + a
resurrected Health surface.

### ISS-13 · Skill registry is catalogue-only
**Status:** open · **Effort:** L (or S to defer cleanly)

The only gap an unauthenticated prospect actually sees: `client/pages/Skills.tsx:169` ships a
"Coming soon" badge + disabled "Add to project →" on the public `/skills` funnel page; static
catalogue at `client/lib/skills.ts`; no registry table, no install RPC. Full fix is L (control
registry + install endpoint + a sanctioned builder→control path that doesn't break the
pure-creator-app invariant + manifest wiring + agent prompts). **Cheap pre-launch option (S):
hide the disabled CTA** and defer the real registry.

### ISS-15 · No deploy history (overwrite-only `deploy_hash`)
**Status:** open · **Effort:** M · gated on console deploy returning

`0004_control.sql:28` apps has a single `deploy_hash TEXT`; `registry.rs:296` is overwrite-only;
no `deploys` table, no rollback handler. Deploy was removed from the console (pure-creator-app)
and PlanCanvas is deleted, so nothing renders history today — low pre-launch, becomes medium
when console deploy returns. ISS-16's per-deploy scorecard depends on this.

### ISS-24 · Migration log not exposed (journal already persists)
**Status:** partial · **Effort:** S–M · gated on a data surface returning

Substrate now exists: plugin-db persists a per-app `__zeroship_migrations` journal in each app
schema (`register_model/bootstrap.rs`, read/updated by `migration_sweeper.rs:267,322`). Only an
RPC to read it + a surface are missing — the cheapest of the old data-canvas family. Needs the
same per-app-schema access-path decision the (removed) DataCanvas introspection family did.
