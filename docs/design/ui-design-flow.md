# Enterprise UI Design Flow — the pattern zeroship-builder follows

**Status:** pattern doc (living). The reusable UI/product-design process for zeroship —
both for the builder's own surfaces and, more importantly, for the apps the agent fleet
generates for creators. We follow this flow whenever we design a UI.

**Date:** 2026-05-26

---

## 0 · Why this exists

Enterprises don't design by taste; they run a repeatable, gated, evidence-driven process
on top of a governed design system. zeroship's pitch is "AI builds *and operates* your
app" — so the builder must bake the **outcomes** of an enterprise design flow into
generation, not just emit code. This doc is the pattern: the stages, the gates, and how
each maps onto the zeroship agent fleet.

The shape is the classic **double diamond**: diverge to understand → converge to define →
diverge to explore → converge to ship.

```
   Discover ──▶ Define ──▶ Design ──▶ Validate ──▶ Build ──▶ Measure
  (research)  (IA/PRD)  (system+   (crit/test/   (tokens→   (analytics
                         mockups)   a11y/QA)      code)      → iterate)
   └─ diverge ─┘└ converge ┘└─ diverge ─┘└─ converge ─┘
```

---

## 1 · The flow (stages)

Each stage lists its **purpose**, **artifacts**, and the **zeroship owner** (which agent /
surface owns it).

### 1. Discover (research)
- **Purpose:** understand the problem, users, and success before drawing anything.
- **Artifacts:** problem statement, jobs-to-be-done, personas, journey map, competitive +
  heuristic analysis, **success metrics defined up front** (north-star + KPIs).
- **zeroship owner:** the **Wizard** (clarifies the creator's idea via survey) + **PM**
  agent (frames the problem, success criteria).

### 2. Define
- **Purpose:** converge on what to build.
- **Artifacts:** PRD / user stories with acceptance criteria, **information architecture**
  (sitemap, navigation, content model), scope cut.
- **zeroship owner:** **PM** agent (plan canvas) + the data-brief the Wizard emits.

### 3. Design
- **Purpose:** turn requirements into interfaces composed through an app-owned
  component layer built on accessible headless parts, not one-off controls.
- **Artifacts:** user flows → wireframes → hi-fi mockups → prototypes. Every screen
  specifies all **states** (empty / loading / error / success / partial), **responsive**
  breakpoints, **content / UX writing**, motion, **accessibility**, theming, i18n.
- **zeroship owner:** **Builder** agent, composing Base UI parts through local
  components and applying Tailwind where the markup is rendered (see §3). The
  agent reuses the app's component layer instead of re-rolling controls.

### 4. Validate
- **Purpose:** prove it's good before it ships — gated.
- **Artifacts:** design critique, usability tests, prototype / A-B tests, **accessibility
  audit (WCAG AA)**, design QA.
- **zeroship owner:** **Critic** (heuristic + quality + a11y + states + responsive
  dimensions, iterating with the Builder) → **Reviewer** (the non-overridable hard gate).

### 5. Build / handoff
- **Purpose:** implement with parity to the design.
- **Artifacts:** design tokens, app-local components, interaction examples, and
  redlines; component-driven implementation; design QA on the built artifact.
- **zeroship owner:** **Builder** (writes files in the sandbox) → **deploy tool**
  (Reviewer-gated build → `.zship` → deploy).

### 6. Measure / iterate
- **Purpose:** close the loop against the success metrics from stage 1.
- **Artifacts:** instrumentation, session replay, metrics vs. KPIs, design-debt log.
- **zeroship owner:** **SRE** agent (health) + per-deploy **scorecard** deltas.

---

## 2 · What makes it *enterprise* (the five pillars)

The stages above are universal. These pillars are what make the flow enterprise-grade —
and each is a design lever for zeroship:

1. **UI conventions = single source of truth.** Carbon (IBM), Polaris (Shopify), Fluent
   (MS), Lightning (Salesforce): tokens → primitives → patterns → page templates.
   In zeroship apps that source lives with the app: compose, don't redraw.
2. **Multi-role and gated.** PM, research, product design, content design, a11y, design
   ops, eng — with **hard gates** (design crit, a11y audit, brand/legal, security) before
   ship.
3. **DesignOps & governance.** The system is a *product*: contribution model, versioning,
   deprecation, audit, maintainers.
4. **Research- and data-driven.** Decisions trace to evidence and move metrics — not vibes.
5. **Accessibility & compliance are non-negotiable.** WCAG AA / Section 508, consent &
   privacy UIs, brand integrity across products.

---

## 3 · How zeroship-builder applies the pattern

**Key insight:** the agent fleet is a *compressed enterprise design org* — discovery →
define → design → validate → ship → measure, collapsed to minutes and one creator.

| Enterprise pillar | zeroship-builder mechanism |
|---|---|
| UI system = source of truth | Apps compose Base UI parts through local components and apply Tailwind where rendered. `examples/issue-tracker/src/ui/` is the current example; zeroship injects no UI SDK or external theme stylesheet. |
| Gated, multi-role | Critic dimension keys (`composed-from-system`, `states`, `responsive`, `accessibility`, `content`, `correctness`, `security`, `performance`, `code_health`) → Reviewer hard gate |
| Research / definition | Wizard (discovery) + PM (IA / plan) |
| States + a11y + responsive by default | Scaffold templates ship empty/loading/error states, WCAG defaults, breakpoints — the agent *inherits* them |
| Measure / iterate | SRE + per-deploy scorecard deltas |

**The pattern, per generated app:**
1. **Wizard** clarifies the brief + success intent (Discover).
2. **PM** sets scope + IA (Define).
3. **Builder** composes Base UI through the app's local component layer,
   applies Tailwind at render sites, and fills in all states, responsive,
   a11y, and content requirements (Design + Build).
4. **Critic** runs the validation dimensions in a loop with the Builder; **Reviewer**
   hard-gates before deploy with blocker kinds `secrets_in_client`, `injection`, `xss`,
   `dangerous_html_user_content`, `missing_critical_states`, `serious_accessibility`,
   `auth_bypass`, `build_or_typecheck`, `migration_safety`, `destructive_op`, `security`,
   and `correctness` (Validate).
5. **Deploy tool** ships the built artifact; **SRE** + scorecard measure (Measure).

**The highest-leverage gap to close (today):** generated apps are still written
largely from scratch each prompt, so quality is LLM-variable. The Builder needs
coherent app-local tokens, components, and page patterns. Base UI supplies
accessible parts and state attributes; Tailwind keeps visual rules at the
render site. Templates can seed that structure without making it a platform SDK.

---

## 4 · The gates (must pass before ship)

A generated app should not deploy until it clears these — the Critic/Reviewer enforce them:

- [ ] **Composed through the app's UI layer** (reuse local components and
      accessible headless parts instead of duplicating controls).
- [ ] **All states present** — empty, loading, error, success, partial.
- [ ] **Responsive** — works at mobile / tablet / desktop breakpoints.
- [ ] **Accessible** — WCAG AA: semantics, focus order, contrast, keyboard, labels; no
      `dangerouslySetInnerHTML` on user content.
- [ ] **Content** — clear copy, validation messages, empty-state guidance.
- [ ] **Correctness** — no stale-state bugs; functional state updates.
- [ ] **Ship-safety** — no secrets in client bundle, no injection/XSS, safe migrations.
- [ ] **Builds + deploys + the live URL renders the real app** (not the starter screen).

---

## 5 · Maintenance

Living doc. zeroship publishes no UI SDK or external theme contract. Generated
React apps own their UI dependencies, local components, tokens, and styles. The
issue tracker is the current reference: it uses Base UI 1.5.0 in `src/ui/` and
styles the app with Tailwind.

Current Critic dimension keys: `composed-from-system`, `states`, `responsive`,
`accessibility`, `content`, `correctness`, `security`, `performance`, `code_health`.

Current Reviewer blocker kinds: `secrets_in_client`, `injection`, `xss`,
`dangerous_html_user_content`, `missing_critical_states`, `serious_accessibility`,
`auth_bypass`, `build_or_typecheck`, `migration_safety`, `destructive_op`, `security`,
`correctness`. `high` and `critical` block deploy; `medium` remains a warning.

The stages (§1) and pillars (§2) are stable; the zeroship *mechanisms* (§3–§4) evolve with
the build.
