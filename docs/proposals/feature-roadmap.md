# zeroship feature roadmap — April 2026

> Built from the competitive research in `docs/competitors/` + the current
> state of the runtime, SDKs, and dev experience.
>
> Organizing principle: **dependency order**, not impressive-demo order.
> Each phase unlocks the next. AI builder is deferred, not cancelled — it
> only ships when creators can monetize what it generates.

---

## Strategic frame

**Where we are**: Runtime and dev-loop are top-tier. 512K/1.17M req/s WinterCG/
fetchFast, zero-config PGlite dev, typed RPC via `"use server"` transform,
`@zeroship/db` + `@zeroship/auth` end-to-end verified. Four infrastructure
bugs fixed in the todo-demo wiring session unlock every app from here.

**Where the market is**: Lovable at $6.6B / $400M ARR hunting for acquisitions.
Anthropic has a leaked full-stack builder. Wix owns Base44 and could connect
it to their marketplace. **None of them take a cut of end-user revenue**. That
unclaimed monetization loop is the only strategic opening large enough to
justify building a new platform in this market.

**The singular bet**: zeroship wins by being the first and only place where
non-technical creators can ship an AI-generated app that earns money from end
users, with the platform taking a Shopify-style 15%. Everything in this plan
either makes that loop possible, makes it work well, or defends it from the
obvious threats.

**The discipline**: runtime perf is done. Don't ship more of it. Every
engineering hour from here is spent on **creator-visible value** or
**monetization-loop durability**.

---

## Estimation model

These estimates are in **AI-assisted days of focused work**, not wall-clock
weeks. Calibration points from recent sessions:

- PGlite integration end-to-end + 4 real bugs fixed: ~1 day
- Runtime WinterCG refactor with 7 perf rounds: ~2 days
- C++ tier-1 experiment (3 iterations + revert): ~half day
- Original zeroship platform (CLAUDE.md note): ~2 weeks

**Fast-compression work** (AI writes, human reviews): CRUD SDKs, native plugins,
dashboard UI, tests, migrations — expect 3-10× speedup vs pre-AI estimates.

**Slow / wall-clock work** (AI can't skip reality): Stripe KYC cycles, DNS
propagation, SSL cert provisioning, real-payment testing, model eval quality.
These don't compress much.

**Day** below = one focused AI-assisted working day (not a calendar day with
meetings and context switches).

## Phase table

| Phase | Goal | Duration | Unlocks |
|---|---|---|---|
| **0. DONE** | Runtime + SDK foundation | — | dev loop works |
| **1. Creator MVP** | One non-technical creator ships a monetized app | **10-14 days** | real revenue flows, case study |
| **2. AI POC** | Validate that AI can generate zeroship apps that work | **3-5 days** | go/no-go on AI builder investment |
| **3. AI Builder v1** | Non-coders go prompt→shipped | **8-12 days** | real TAM expansion |
| **4. Differentiators** | What nobody else has | **5-7 days** (parallel) | defensibility |
| **5. Ecosystem** | SDKs + marketplace | ongoing | moat compounds |

Total to "first real creator making money": **2-3 weeks**.
Total to "AI builder in beta": **5-7 weeks**.

---

## Phase 1: Creator MVP (10-14 days)

**Definition of done**: a creator who can write some TypeScript but isn't a
DevOps person can ship a real consumer app, onboard their Stripe account, take
money from end users, and see their payout in the dashboard. Platform takes
its 15%.

### 1.1 Scaffold (0.5 day) — `npm create zeroship-app`

Creator's first five minutes shouldn't involve reading five READMEs.

- Minimal template: `package.json`, `vite.config.ts`, `src/index.ts` (server),
  `src/App.tsx` (client), `tsconfig.json`, `.gitignore` (including `.zeroship/`)
- Three variants at create time: `--template=basic`, `--template=saas`,
  `--template=marketplace`
- `npm install && npm run dev` → PGlite boots, app runs, creator sees a
  working counter with DB persistence in < 60 seconds

**Why first**: every subsequent feature is blocked by "but how do creators
start?" Sets onboarding floor to something we control.

### 1.2 `@zeroship/storage` (1 day)

File uploads are table stakes. Every profile, avatar, product image, or
document requires this. 60%+ of real apps need it.

- Native plugin `zeroship.storage.*` — put/get/delete/list/presignUrl
- Backend: S3 in production (MinIO for self-hosted), local filesystem in dev
- SDK: `storage.bucket("uploads").put(key, file)` with image resize/transform
  via `imgproxy` (not Cloudinary — self-hosted)
- Upload UI component (React): drag-drop, progress, error handling
- Size + MIME type limits enforced in the native plugin

**Why here**: feature parity with every competitor; AI builder can't generate
apps with uploads without this.

### 1.3 `@zeroship/kv` (0.5 day)

Sessions, caches, feature flags, rate-limit counters. Less critical than
storage (creators can fake it with DB) but cheap to ship.

- Native plugin `zeroship.kv.*` — get/set/delete/incr/expire/list
- Backend: Redis in prod, in-memory Map in dev
- SDK: typed accessors `kv.get<string>("session:xyz")`
- TTL support first-class

### 1.4 Stripe Connect (3-4 days) — **the defining feature**

*Wall-clock bottleneck: Stripe KYC sandbox cycles + webhook reliability
testing. Code itself is ~1 day with Claude.*

This is the thing that makes zeroship a platform vs another PaaS.

- Creator onboarding: Express Connect flow (zeroship is platform, creator
  is Express account)
- KYC + identity verification handled by Stripe
- Platform fee: 15% via `application_fee_amount` on every charge
- App-facing SDK: `@zeroship/payments` — `createCheckout(priceId, {successUrl})`,
  subscription management, webhook relay
- End user pays → Stripe → creator gets 85% → zeroship gets 15% → split goes
  to each party's Stripe balance, payouts on standard schedule
- Creator dashboard: pending payouts, payout history, failed charges, dispute
  surface

**Technical notes**:
- Need an idempotency layer for webhook delivery (Stripe retries)
- Tax is Stripe's problem (Stripe Tax), not ours in v1
- Creators in countries without Connect Express → manual platform sign-off
  required, queue for Phase 4

**Why this phase**: without this, nothing else matters commercially. Every
day we don't have it is a day a creator can't choose us over Lovable+DIY
Stripe.

### 1.5 Schema migration beyond additive (1-2 days)

Today `registerModel` runs `CREATE TABLE IF NOT EXISTS` + `ADD COLUMN IF NOT
EXISTS`. Real iteration needs rename, type change, drop with backfill.

- Migration history table per app (`_zs_migrations`)
- `zeroship migrate preview` — shows SQL before running
- `zeroship migrate up/down` via CLI
- AI-generated apps need migration flow auto-triggered when creator edits
  their schema
- Rollback: each migration tracks its inverse

**Why**: competitors are weak here (Lovable/Bolt also only handle additive).
If we do this well, it's a durable differentiator for AI-iterated apps.

### 1.6 Deploy pipeline polish (1 day)

Today's `zeroship deploy` works but is rough. Productize.

- `zeroship deploy` auto-builds `.appbundle`, uploads to control plane,
  triggers rolling deploy
- Health check before cutover; auto-rollback on failure
- `zeroship rollback <version>` one-command revert
- `zeroship logs --tail` for realtime debugging
- SSL auto-provisioning on `{app}.zeroship.ai`

### 1.7 Custom domains (1-2 days)

*Wall-clock: DNS propagation + Let's Encrypt rate limits. Code is ~half day.*

Every real creator needs their own domain.

- Dashboard: add custom domain → DNS instructions → validate via TXT
- Cert auto-provisioning via Let's Encrypt (or Cloudflare SSL if we front
  with CF)
- Gateway routes custom domain → correct app via domain-to-app-id table
- Wildcard + apex domain support

### 1.8 Creator dashboard MVP (2-3 days)

The UI where creators live.

- Apps list: name, status, domain, last deploy, revenue (MTD)
- Per-app view: revenue graph, error rate, request count, active users
- Deploy history with diff viewer + one-click rollback
- Logs per app (last 24h, keyword search)
- Settings: custom domain, env vars, Stripe Connect status, API keys
- Billing view: pending payout, historical payouts, disputes

**Minimum tech**: Next.js + shadcn on Vercel (or self-hosted on zeroship —
eating our own dog food is worth 2x the PR value)

### Phase 1 exit criteria

Pick one real non-technical creator. They scaffold an app, deploy it, wire
up their Stripe Connect, publish at a custom domain, take money from 5 real
end users, and see their first payout in their dashboard. Document every
point of friction.

**If we can't get one creator through this loop, don't ship AI builder.**
Phase 2 is blocked.

---

## Phase 2: AI builder proof (3-5 days)

**Goal**: determine whether Claude Opus 4.7 (or equivalent) can generate
zeroship apps that work, with our current SDKs as the target surface. This
is cheap validation before committing months to Phase 3.

### 2.1 Test harness (1 day)

- System prompt that defines zeroship's SDK surface (`@zeroship/db`, `/auth`,
  `/storage`, `/kv`, `/payments`) with example code snippets per primitive
- Tool calls: `write_file`, `read_file`, `run_migration`, `run_test`
- Test set: 10 standardized prompts of increasing complexity
  1. Todo list (our reference)
  2. Note-taking app with tags
  3. Recipe sharing with photo uploads
  4. Subscription newsletter
  5. Invoice generator with PDF export
  6. Marketplace (buyers, sellers, items, checkout)
  7. Multi-user chat
  8. Job board with payments
  9. SaaS dashboard with per-user tenants
  10. Full e-commerce store

For each: does it compile? Does it run? Does CRUD work? Does auth work? Does
payments work? Score 0-5 on each dimension.

### 2.2 Close SDK gaps (1-2 days)

Iterate. Common failure patterns typically:
- Schema shape mismatches (fix in `@zeroship/db` API or prompt)
- Missing primitives (fix by adding to SDK)
- Ambiguous error messages (fix error surfaces)
- Prompt over-specificity (fix system prompt to be more schematic)

### 2.3 Decision gate (1 day)

- **If 7+ of 10 apps compile and run first-shot**: commit to Phase 3. Ship
  AI builder in 6-8 weeks.
- **If 4-6 of 10**: another round of SDK hardening before Phase 3. Likely
  add month+ to timeline.
- **If <4**: the SDK or the model isn't ready. Defer AI builder by a quarter,
  keep investing in differentiators.

**Written outcome**: a 5-page report on what generation quality actually looks
like, with specific failure modes. This is the input to Phase 3's design.

---

## Phase 3: AI builder v1 (8-12 days, contingent on Phase 2)

**Goal**: non-technical creator types a description of their app, gets a
working app deployed to their subdomain, can iterate via chat.

### Scope for v1

- Chat interface (web-based, not CLI)
- Preview URL per generation
- Tool loop: generate → run → observe errors → fix
- Git-backed (every generation is a commit)
- Handoff to Phase 1 deploy pipeline when creator says "publish"
- One-shot apps only in v1 (no multi-turn migrations of complex apps)

### Explicit non-goals for v1

- No visual edit mode (Lovable has it; we'll add in v1.5)
- No Figma import (Lovable's lead is large; add later)
- No mobile (web-only; Rork's territory)
- No multi-agent architecture (single Claude Opus agent with tool use is
  enough for v1)
- No model menu (just Claude Opus 4.7, pick right)

### Architecture sketch

- Session: creator prompt → agent
- Agent: Claude Opus with system prompt + tool definitions
- Tool definitions: write_file, run_migration, run_test, get_preview_url,
  describe_error
- Sandbox per session: ephemeral container running `zeroship serve` against
  a PGlite
- On "publish": deploy pipeline from Phase 1 takes over
- Context management: session state is the file tree + chat history +
  summary of intent

### Error recovery loop

Critical — this is where Lovable/Bolt eat credits and lose user trust. Our
version should:
- NOT charge creator per retry (our revenue is end-user cut, not token usage)
- Cap retries per bug (3 attempts, then ask creator)
- Surface what it tried in plain English
- Commit each attempt so creator can rewind

This is a concrete differentiator: "the AI is free to try, because we earn
when your app earns."

---

## Phase 4: Differentiators (5-7 days, overlaps Phase 3)

What zeroship has that nobody else does.

### 4.1 Multi-tenant SaaS primitives

No competitor handles "I'm building a SaaS where each of my 1000 customers
gets their own isolated data" as a first-class concept. AI-generated apps
need this desperately.

- `@zeroship/tenants` SDK: `defineTenant("org")`, `tenant(orgId).db.todos.find(...)`
- Native plugin does RLS policy generation automatically
- Tenant-scoped storage, KV, payments (per-tenant Stripe subscription)
- Admin dashboard auto-generated per app

### 4.2 Plain-English observability

Non-technical creators need "your app had 3 errors, 2 were user typos, 1 was
a real bug" — not distributed traces.

- Error aggregation with AI-summarized root cause
- "What changed today" — diff between yesterday and today's error rate
- Proactive alerts: "5 users hit the same bug in the last hour"
- Integration with deploy history: "this bug started after your Tuesday deploy"

### 4.3 Built-in rate limiting + abuse protection

Every app ships with default limits. Creators don't have to think about it
until they hit scale.

- Per-IP rate limit by default (60 req/min/IP on gateway)
- Per-user rate limit via auth (configurable per endpoint)
- Anomaly detection: sudden traffic spike → auto-enable strict mode
- Fail-open vs fail-closed is a per-app setting

### 4.4 Typed RPC hardening

We already have this (via `"use server"` + vite-plugin transform). Make it
a marquee feature.

- Compile-time type check across client-server boundary
- Auto-generated OpenAPI from server exports
- "I changed this server function" → TypeScript errors in client file
- Published as a comparison point: "the only AI app builder with full-stack
  type safety"

---

## Phase 5: Ecosystem (ongoing)

Once Phases 1-4 ship, the platform is defensible. Phase 5 is about
accumulation.

- `@zeroship/email` — transactional email via Resend/Sendgrid
- `@zeroship/ai` — LLM inference wrapper; creator brings no API keys
- `@zeroship/permissions` — RBAC/ABAC primitives
- `@zeroship/analytics` — product analytics for end users
- App marketplace (long-term, 6+ months out): creators publish templates;
  other creators remix; platform takes a smaller cut on template sales

Third-party SDKs (written by the community) on npm.

---

## Explicit non-goals

Things we will NOT ship in 2026, even though they're tempting:

1. **Runtime perf optimizations.** 512K req/s is already table-topping. Any
   further work returns zero strategic value.
2. **C++ native integration.** Explored, regressed, reverted. Don't revisit
   until a real creator hits a ceiling 512K can't handle (unlikely).
3. **WebContainer-style in-browser iteration.** Bolt's moat. Takes years to
   build. Server-side iteration is fine for our economics.
4. **Custom LLM / fine-tuning.** Use Claude Opus off-the-shelf. Lovable does.
5. **Swift native codegen.** Rork's niche. Mobile native is not the
   near-term TAM.
6. **Visual edit (click-to-change) in AI builder v1.** Lovable's lead is
   large; add in v1.5 when we understand the iteration patterns better.
7. **Multiplayer editing.** Replit's lead is years. Add only if creator
   demand is explicit.
8. **Figma import.** Nice-to-have; not differentiating. Defer.
9. **Web dashboard written in non-zeroship stack.** Eat our own dog food —
   the creator dashboard runs on zeroship itself.
10. **Support for non-Postgres databases.** Our whole stack assumes Postgres.
    Adding MySQL/MongoDB/etc. doubles our matrix for marginal market gain.

---

## Risks and mitigations

**Risk 1: Lovable ships end-user monetization before us.**
- Probability: meaningful (they're hunting for acquisitions, have $500M+ cash)
- Impact: existential (our only claimed territory gets contested)
- Mitigation: finish Phase 1 in ≤10 weeks. Don't pad.
- Trigger to re-plan: Lovable announces Stripe Connect marketplace layer

**Risk 2: Anthropic ships their leaked full-stack builder with payments.**
- Probability: medium, timeline unknown
- Impact: severe (model quality + distribution + now platform)
- Mitigation: integrate deeply with Claude — become their reference platform.
  If they add payments, we're their end-user monetization partner.
- Trigger to re-plan: Anthropic announces the full-stack builder publicly

**Risk 3: Security incident on a deployed zeroship app.**
- Probability: high if we're careless, medium if we invest
- Impact: brand-damaging (see Lovable RLS incident, 170 apps exposed)
- Mitigation: Phase 4 rate limiting + RLS-aware DB plugin + sandbox
  isolation. Security is a differentiator, not a cost.

**Risk 4: AI builder generates low-quality apps, creators churn.**
- Probability: real; addressed by Phase 2 gate
- Impact: category reputation damage
- Mitigation: Phase 2 exists explicitly to NOT ship a bad AI builder

**Risk 5: Stripe Connect onboarding friction blocks creators.**
- Probability: medium (Stripe Connect KYC is non-trivial for non-US creators)
- Impact: reduces addressable creator base
- Mitigation: US-first in Phase 1. Expand in Phase 4.

---

## TL;DR — the one-page version

| Week | Ship | Days | Why |
|---|---|---|---|
| **W1** | Scaffold + storage + KV + schema migration + deploy polish | 4 | Onboarding floor + capability completeness + production readiness |
| **W1-W2** | **Stripe Connect + custom domains** | 4-6 | Monetization loop (the moat) + creator can ship at own domain |
| **W2-W3** | Creator dashboard MVP | 2-3 | Where creators see revenue |
| **end of W3** | **First real creator ships a monetized app** | — | Phase 1 exit gate |
| **W3-W4** | AI POC harness + 10-app test + decision | 3-5 | Go/no-go on AI builder |
| **W4-W6** | AI builder v1 (if POC clears) | 8-12 | TAM expansion |
| **W4-W6** | Multi-tenant SaaS + observability + rate limiting (parallel) | 5-7 | Defensibility |
| **W7+** | Ecosystem SDKs, marketplace | ongoing | Moat compounds |

**One-sentence strategy**: ship the monetization loop in ~2 weeks, validate
AI codegen quality in ~4 days, commit AI builder only if the SDK surface
holds under real AI load, and use the 5-7 days of differentiator work in
parallel to make sure whatever Lovable ships later this year doesn't erase us.

## Wall-clock reality check

Day estimates are for **focused AI-assisted work**. The real wall-clock
calendar gets bent by:

- **Stripe KYC cycles** (~1-2 days per test iteration while waiting on
  Stripe's side — parallelizable with other work)
- **DNS propagation** (hours per test, not meaningful blocker once
  automated)
- **Let's Encrypt rate limits** (matter only at scale)
- **Real-payment testing** (can't compress "spend $1 in test mode, wait
  for settlement")
- **Beta creator feedback loop** (depends entirely on your creator pool)
- **Human decisions** (scoping, positioning, UX copy) — always wall-clock

Realistic calendar from today: **first monetized creator in 3 weeks**, **AI
builder beta in 6 weeks** if you don't stop to do other things.
