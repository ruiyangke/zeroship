# zeroship-builder — canonical features & roadmap

**Status:** canonical (living). This document is the single source of truth for
*what the builder is, what's built, and the order we build the rest.*
**Date:** 2026-05-25
**Owner:** builder program

> **Supersedes the phasing in three older docs.** Their *feature definitions* and
> *product design* remain authoritative; their *ordering* is replaced by the
> tier ladder here.
>
> | Doc | Keep | Replaced by this doc |
> |---|---|---|
> | `docs/proposals/feature-roadmap.md` | strategic thesis (monetization moat) | platform phase ordering (predates the built builder) |
> | `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` | §0–§29 behavior/UX/architecture | §30 Release 0–3 phasing |
> | `docs/archive/superpowers/specs/2026-04-30-zeroship-builder-features.md` | the 580-feature inventory (A–JJ) | priority→release mapping |
> | `zeroship-builder-status.md` | the May-1 build-state snapshot | "recommended next focus" (folded into M1/M2/M4) |

---

## 1 · Goal

A creator describes an app in natural language; a fleet of AI agents
(**Builder · Critic · Reviewer · PM · SRE**) builds it, ships it on the zeroship
runtime, monitors it, fixes it, and helps the creator monetize it. The platform
takes 15%; the creator keeps the rest.

Not "AI builds your app" — **"AI builds *and operates* your app: a tiny product
team in software."**

- **Maker mode (default)** — Ali, the non-technical creator. Chat · preview · plan · health · settings. Never sees code.
- **+Data** — graduated middle tier (data + media canvases).
- **Dev mode** — Sam, the technical creator. Files · env · full IDE.

**GA = the full §0–§31 surface of the design spec.** This roadmap sequences the
road there as always-shippable tiers.

## 2 · Strategic thesis (why the order is what it is)

From `feature-roadmap.md`, and still true: **the moat is the monetization loop.**
Competitors (Lovable, v0, Bolt, Base44) let creators *ship*; none take a
Shopify-style cut of *end-user revenue*. zeroship wins by being the first place a
non-coder ships an AI-generated app that **earns money**, platform taking 15%.

Consequence for ordering: a beta where creators ship **and earn** beats a beta
where they only ship. Monetization is therefore pulled into the **first
externally-meaningful tier (M1)** — cheap to do now because the backend
(`stripe_handlers`, `stripe_store`, `metering`, `@zeroship/payments`) already
exists; it is mostly creator-facing UI wiring.

## 3 · Where we are today (2026-05-25)

The builder is **~80% built on the surface and agent-wire layers, frozen since
2026-05-01**, while the platform beneath it absorbed ~1,800 commits (db, kv,
sandbox/Nomad-CH, runtime). The result: a rich, tested frontend whose **loop to
the now-much-stronger backend has drifted out of sync.**

**Real:** the agent loop genuinely plans, writes files, and runs commands in a
real sandbox via a real LLM; control-plane deploy/env/secrets/routes and the
sandbox runtime are production-grade; 221 e2e tests pass against live OpenAI; a
clean `.zship` builds.

**Broken / missing (the gap):**

- **B1 — sandbox create is broken.** Builder sends `user_id="builder"`; control
  now requires typed-ids (`usr_…`/`prj_…`) → **400 on every `POST /sandboxes`.**
  The build loop dies at sandbox creation.
- **B2 — `GET /api/apps/{id}/logs` does not exist** → logs/SRE views 404.
- **B3 — deploy is not in the agent loop.** The agent's only custom tool is
  `ask_survey`; `deployApp` is a decoupled client action. "Ships it" is not
  agent-driven; the Reviewer "pre-deploy gate" is advisory prose with no hook.
- **B4 — the data canvases are stubs.** Issues/deploys/media/archive/incidents
  live in KV or hardcoded sample constants (ISSUES.md ISS-14/15/17/19/20–26).
- **B5 — conveniences:** no public preview DNS, GCS L2 snapshot stub, dead
  `/sessions/*` API in `sandbox.ts`, monetization UI absent.

---

## 4 · Canonical feature catalog

Feature areas A–JJ from the inventory, with **priority span**, **current build
status**, and **target tier**. Feature numbers are canonical IDs — full
per-feature behavior lives in
`docs/archive/superpowers/specs/2026-04-30-zeroship-builder-features.md`
and `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`.

Status legend: ✅ shipped · 🟡 partial · 🔶 stubbed (UI-honest placeholder) ·
⬜ missing.

| Area | Features | Pri | Status today | Target tier |
|---|---|---|---|---|
| **A** Pre-auth / public | 1–11 | [2–3] | ✅ marketing/pricing/templates/legal; ⬜ showcase | shipped · M4 (showcase) |
| **B** Auth (creator) | 12–23 | [0–2] | ✅ signup/login/verify; 🟡 OAuth; 🔶 forgot-pw (ISS-09), sessions/2FA (ISS-10/11), delete (ISS-12) | M0 core · M1 forgot-pw+delete · M2 2FA |
| **C** Onboarding | 24–30 | [0–2] | ✅ welcome, first-prompt, tour, intent | shipped |
| **D** Project lifecycle | 31–48 | [0–2] | ✅ create/list/rename/delete; 🟡 search/sort/filter; 🔶 archive (ISS-19) | shipped · M1 (archive) |
| **E** Workspace shell | 49–60 | [0–1] | ✅ topbar/pills/rail/tier-toggle; 🟡 cmd-palette, notifications | shipped · M2 |
| **F** Chat surface | 61–84.14 | [0–2] | ✅ streaming/receipts/diffs/stop/regen/edit/slash/@/survey | shipped |
| **G** Preview canvas | 85–97 | [0–2] | 🔶 PreviewCanvasStub; real preview needs sandbox-preview wiring + DNS | **M0** (basic) · M1 (DNS, device frames) |
| **H** Files / editor | 98–117 | [1–2] | 🟡 read-only manuscript view; ⬜ Monaco editor | M2 (+Code) |
| **I** Logs canvas | 118–129 | [1–2] | 🟡 UI shipped, **backend missing (B2)** | **M0** (endpoint) · M1 (filters) |
| **J** Data canvas (DB) | 130–145 | [1–2] | 🔶 hardcoded sample data (ISS-20–25) | M2 |
| **K** Media canvas | 146–153 | [1] | 🔶 KV/base64 (ISS-26) | M1 |
| **L** Env canvas | 154–161 | [1–2] | ✅ vars/secrets (real backend); ⬜ per-branch | shipped · M3 (per-branch) |
| **M** Settings canvas | 162–178 | [0–2] | ✅ name/url/plan; ⬜ **custom domain (167)**; 🔶 archive | shipped · M1 (domains) |
| **N** Deploy / versions | 179–187 | [0–2] | 🟡 deploy exists but **not in agent loop (B3)**; ⬜ history (ISS-15)/rollback | **M0** (deploy-in-loop) · M1 (history/rollback) |
| **O** End-user auth (built app) | 188–192 | [1–2] | 🔶 config UI stub | M2 |
| **P** Templates | 193–199 | [2–3] | ✅ gallery/use; ⬜ detail page, submit | shipped · M4 (marketplace) |
| **Q** Account / profile | 200–207 | [0–2] | ✅ name/email/password; ⬜ portfolio | shipped · M4 |
| **R** Billing — creator pays | 208–216 | [2] | ⬜ **UI missing** (backend real: stripe/metering) | **M1** |
| **S** Payouts — creator earns | 217–224 | [2] | ⬜ **UI missing** (backend real: Stripe Connect) | **M1** (core) · M3 (tax/schedule) |
| **T** Built-app monetization | 225–229 | [2–3] | ⬜ UI/templates (SDK `@zeroship/payments` exists) | M1 (checkout) · M3 (coupons/trials) |
| **U** Sharing / social | 230–236 | [2–3] | 🟡 OG/meta; ⬜ showcase/stars/follows | M2 · M4 |
| **V** Collaboration | 237–242 | [4] | ⬜ | M4+ |
| **W** Notifications | 243–246 | [1] | 🟡 in-app; ⬜ email/prefs | M2 |
| **X** Search | 247–248 | [1–2] | 🟡 | M2 |
| **Y** Admin (role-gated) | 249–260 | [1–3] | ⬜ **not built** (status-doc Plan 03) | M4 |
| **Z** Help & support | 261–266 | [2–3] | 🔶 editorial stubs | M4 |
| **BB** Builder skill catalog | 276–320 | [0–3] | 🔶 catalogue-only (ISS-13); ⬜ skill auto-load/packs | M2 (skills real) · M4 (marketplace) |
| **CC** PM agent + plan canvas | 321–356 | [0–2] | ✅ chat-mode + plan UI; ⬜ issues table (ISS-14), cron (ISS-28) | M1 (tables+cron) · M2 (proactive) |
| **DD** Themes & feature-sets | 357–375 | [1–3] | 🔶 catalogue | M3 |
| **EE** SRE agent + health | 376–407 | [1–2] | ✅ chat-mode; 🔶 perf from log-regex (ISS-18); ⬜ incidents (ISS-17), cron, auto-fix | M1 (cron) · M2 (real telemetry) |
| **FF** Reviewer agent | 408–412 | [1–2] | 🟡 defined, **advisory only — no hard-gate hook** | M2 |
| **GG** Quality (Critic + scorecard + gates) | 413–474 | [0–3] | ✅ Critic loop; 🟡 scorecard (KV), gates; ⬜ per-deploy persistence, post-deploy verify | M2 |
| **HH** Data management (deep) | 475–518 | [1–3] | ⬜ schema/bulk/backups/migrations-as-objects | M3 |
| **II** Branching (dev/prod/preview) | 519–556 | [1–3] | ⬜ not built | M3 (prod/dev + migration gate) · M4 (preview/CoW) |
| **JJ** Cross-platform | 557–580 | [0–2] | ✅ responsive builder; 🟡 built-app responsive defaults + Critic dim | shipped · M2 |
| **AA** A11y / polish | 267–275 | [1–5] | ✅ responsive/a11y/focus; ⬜ dark mode/i18n/PWA | shipped · M4+ |

**Locked foundation decisions** (inventory decisions 1–27) stand unchanged:
two-modes persona, Refined-Atelier brand, single-shell IA, living-document chat,
three tiers, five-agent fleet, Critic⇄Builder loop with Critic-blocks/Builder-style
tiebreaker, non-overridable hard gates, opt-in public scorecard. See the design
spec §1.

---

## 5 · The roadmap — five shippable tiers

Each tier is a **runnable, demoable product**. Two lanes per tier: **builder-app
work** and **platform prerequisites**. A builder chunk never starts before its
platform prerequisite lands.

### M0 — Close the loop *(the magic works, once, for one creator)*

> **Outcome demo:** type a prompt → fleet plans → writes files into a real
> sandbox → runs/builds → live preview → deploy to a running URL. Single node,
> one creator. Stubs everywhere else are fine.

- **Builder lane:** thread a real `usr_…`/`prj_…` typed-id through
  `_sandbox_backend.ts` (fix **B1**); add a **deploy tool** to the agent toolset,
  gated behind a real Reviewer pass (fix **B3**); wire the Preview canvas to the
  live sandbox preview proxy (replace `PreviewCanvasStub`).
- **Platform lane:** add `GET /api/apps/{id}/logs` (fix **B2**); confirm sandbox
  preview proxy path is reachable from the builder; retire dead `/sessions/*`.
- **Exit gate (measurable — repurposed from the old roadmap's POC gate):**
  a fixed harness of **10 diverse prompts**; **≥ 7/10 produce an app that
  compiles, runs in the sandbox, and deploys to a reachable URL**, proven by a
  faithful e2e that drives the real runtime + dispatcher (no shims).

### M1 — Monetizable closed beta *(real creators ship AND earn)*

> **Outcome demo:** a hand-picked creator signs up, builds an app, attaches
> Stripe, an end user pays, the platform takes 15%, the creator sees earnings —
> all state survives restart and is multi-node-honest.

- **Builder lane:** creator **billing** UI (R: plan/usage/invoices) + **payouts**
  UI (S: Stripe Connect onboarding, earnings dashboard, 15% breakdown) +
  **built-app checkout** (T: `@zeroship/payments` wiring); un-stub the 7
  canvases (issues, deploys/rollback, media→real storage, archive, forgot-pw,
  account-delete); custom-domain UI (M-167).
- **Platform lane:** `issues`/`issue_comments`/`milestones`, `deploys`,
  `media`, `archived_at` tables + REST; **cron harness** (`control/scheduler.rs`)
  firing `pm.digest`/`sre.monitor` (ISS-28); transactional email (forgot-pw,
  digests); custom-domain add + DNS-verify + TLS; multi-node KV backing.
- **Exit gate:** one real external creator completes signup → build → deploy →
  attach Stripe → receive a test end-user payment; all canvases read real tables;
  nothing in the 7 ISSUES remains `open`.

### M2 — Quality + agents honest *(the product team is real, not theatre)*

> **Outcome demo:** Critic⇄Builder loop visibly raises quality; Reviewer hard-gate
> blocks a bad deploy; PM/SRE act on real signals on a schedule, not on log-regex.

- **Builder lane:** Reviewer hard-gate enforcement on deploy (FF-409/412);
  per-deploy scorecard persistence + history (GG.5); Data-canvas real
  introspection (J + HH.1 via `pg_catalog`); incidents timeline (EE/ISS-17);
  end-user auth config (O); skills become real (BB auto-load).
- **Platform lane:** **metering→health pipeline** (real p50/p95/error-rate, not
  log parsing — ISS-18); `pg_catalog` introspection RPC; SRE telemetry store
  (control-plane Postgres + materialized views); pre/post-deploy gate hooks.
- **Exit gate:** a deliberately-broken change is blocked by a hard gate; the
  scorecard shows real per-deploy deltas; PM digest + SRE scan fire on cron and
  post real findings to the plan/health canvases.

### M3 — Differentiators + branching *(what nobody else has)*

> **Outcome demo:** prod/dev branches with safe migrations; multi-tenant SaaS
> primitives, rate-limiting, plain-English observability.

- **Builder lane:** branch switcher + per-branch env (II.1/II.4); themes &
  feature-sets real (DD); data management deep ops + backups/restore + migrations
  -as-objects (HH); built-app monetization extras (T: coupons/trials).
- **Platform lane:** `compio-postgres` schema-namespaced branching
  (`project_<id>__<branch>`); migration safety hard-gate on `prod`; gateway
  `{branch}--{slug}` host parsing; rate-limit/abuse protection; SaaS primitives.
- **Exit gate:** a creator forks dev from prod, runs a migration blocked on prod
  by the Critic hard-gate until acknowledged, and promotes dev→prod atomically.

### M4 — GA *(full surface, defensible, growth-ready)*

> **Outcome demo:** the complete §0–§31 product, public sign-up open.

- **Builder lane:** admin console (Y, role-gated: apps/users/revenue/health/audit);
  public showcase + creator portfolios (U/Q); templates marketplace +
  creator-publishing (P/BB); collaboration (V); notifications center + email (W);
  global search (X); help hub (Z).
- **Platform lane:** preview branches auto-created on PR-style deploys + CoW
  Postgres backend (II.5/II.6 V2); multi-domain; SRE full-autonomy mode;
  GG.8 continuous self-evaluation; dark mode / i18n (AA [5]).
- **Exit gate:** public sign-up opens; admin can answer every operational
  question from the console without a DB shell.

### Tier dependency map

```
M0 close-the-loop ──► M1 monetizable beta ──► M2 quality+agents ──► M3 branching+diff ──► M4 GA
   (B1·B2·B3)            (money UI + 7 stubs)     (real telemetry)      (branch DB)         (admin/growth)
        │                      │                       │
   measurable            real creator            hard-gate blocks
   10-prompt gate        ships & earns           a bad deploy
```

---

## 6 · Execution model — how we loop to implement

Each tier decomposes into **loop-sized chunks**: one chunk = one branch, one
focused diff, independently verifiable. The loop drains chunks tier by tier.

**Discipline (non-negotiable, per established working agreements):**

1. **Pilot, don't author.** Substantive chunks are dispatched to subagents
   (`model: opus`, `run_in_background: true`); the main loop orchestrates and
   judges. Trivial chunks may be done directly.
2. **Faithful e2e, never shims.** Every chunk's acceptance test drives the real
   path (live runtime + dispatcher + real backend), not unit stubs. A green
   suite over a shimmed path does not count as done.
3. **A regression test per fix.** Every bug/gap fix adds a test that would fail
   pre-fix. "Done" requires the test to *exist*, not just a green suite.
4. **Two-agent review.** Critic + reviser, not self-review, on each chunk before
   it's accepted. Verify the diff, run the tests, check the decisions each round.
5. **Measure, don't estimate.** No ns/%/LOC/speedup claims without a measurement.
   Tier exit gates are measurable (e.g. "≥7/10 prompts deploy"), not vibes.
6. **Commit-only, never push.** Commit per accepted chunk; `git push` only on
   explicit request.
7. **Pre-launch, no back-compat.** Rename/break/restructure freely; no
   `@deprecated` aliases, no migration shims, no detect-and-warn paths.

**Per-tier loop shape:**

```
deep-review the tier's chunks ─► draft chunk brief ─► dispatch builder subagent
   ─► critic subagent ─► reviser subagent ─► judge (diff+tests+decisions)
   ─► commit ─► next chunk … ─► tier exit gate met? ─► advance tier
```

---

## 7 · Open decisions to confirm

1. **Monetization in M1** (this doc's recommendation) vs deferred to a later tier.
   Rationale for M1: the moat thesis + the backend already exists.
2. **Branching deferred to M3/M4** despite the design spec placing two-branch in
   Release 0. Rationale: too platform-heavy to gate the core loop; only the
   migration hard-gate is pulled earlier (M2).
3. **Monaco editor (H)**: the spec wants it; it's currently absent and the Files
   canvas is read-only. Confirm M2 (+Code) placement and the ≤200 KB bundle /
   lazy-load constraint (JJ-561/562).
4. **SRE telemetry backend (M2)**: control-plane Postgres + materialized views
   (spec default) vs external (ClickHouse). Lean: Postgres for V1.

## 8 · Maintenance

This doc is living. When a tier's exit gate is met, mark its rows ✅ in §4 and
record the date. When ordering changes, update §5 and note why. The design spec
(§0–§31) and the feature inventory (A–JJ) stay frozen as the *definition* source;
this doc owns *status and order*.
