# Codex brief — Phase 3 (the moat): generated apps compose from @zeroship/ui; Critic/Reviewer enforce it

## Goal
Make generated apps **inherit the design system**: the Builder composes UIs from
`@zeroship/ui` (wrapped in `ThemeProvider`, deps declared so Phase 2's registry install
works), and the Critic/Reviewer enforce the `docs/design/ui-design-flow.md` §4 ship gates.
This is the platform moat: every creator's app is polished + accessible by default.

Worktree: `-C .worktrees/ui-design` (branch `builder/ui-design`). **DO NOT commit / merge /
push** — the pilot reviews and commits.

## Context
- `@zeroship/ui` is published to the private Verdaccio registry (Phase 1) and installable in
  the sandbox via the scoped `.npmrc` the Builder writes (Phase 2,
  `ZEROSHIP_SDK_REGISTRY`). Generated `package.json` may list `@zeroship/*` (no whitelist).
- Read `docs/design/ui-design-flow.md` (esp. §3 table + §4 gates) and
  `docs/reference/api-design-guidelines.md`.
- Agent code: Builder/scaffold prompts in `apps/zeroship-builder/src/server/internal/
  prompts.ts` (+ `agents.ts`, `agent-writes.ts`, `tools.ts`); Critic in
  `apps/zeroship-builder/src/server/internal/critic.ts`; Reviewer in
  `apps/zeroship-builder/src/server/internal/reviewer.ts`. INVESTIGATE these first to learn
  the existing dimension / blocker-kind structures, then extend them idiomatically.

## Do
1. **Builder composes from `@zeroship/ui`** (prompts.ts / scaffold). Instruct generated apps
   to: add `@zeroship/ui` (+ `react`/`react-dom`) to `package.json`; import
   `"@zeroship/ui/styles.css"` once; wrap the app root in `<ThemeProvider>`; build UI by
   COMPOSING `@zeroship/ui` components (Button, Card, Dialog, Select, Tabs, Input/Textarea,
   Table, Badge, Toast, EmptyState, …) instead of hand-rolling buttons/inputs/dialogs/
   layout; use the semantic tokens, not raw hex/px. Keep it concise + token-budget-aware
   (link the component list, don't inline everything). Don't break existing non-UI prompt
   behavior.
2. **Critic dimensions** (critic.ts) — add the §4 validation dimensions (idiomatic to the
   existing structure): composed-from-system (ad-hoc primitive where a system component
   exists), states (empty/loading/error/success/partial), responsive (mobile/tablet/
   desktop), accessibility (WCAG AA: semantics/focus/contrast/keyboard/labels; no
   `dangerouslySetInnerHTML` on user content), content (clear copy, validation messages,
   empty-state guidance). Reuse existing correctness/ship-safety dimensions.
3. **Reviewer blocker kinds** (reviewer.ts) — add hard-gate blocker kinds for the
   non-negotiable §4 violations (e.g. secrets in client bundle, injection/XSS,
   `dangerouslySetInnerHTML` on user content; missing critical states; serious a11y).
   Keep medium issues as warnings per the existing severity model.
4. **Update `docs/design/ui-design-flow.md` §3** table + §5 with the concrete names: the
   `@zeroship/ui` package + component list, the new Critic dimension keys, the Reviewer
   blocker kinds.

## Verify (NO OpenAI — wiring + structure only; FLAG that behavior needs a quota'd run)
- `pnpm --filter zeroship-builder build` (and typecheck) green.
- Any existing critic/reviewer unit tests pass; ADD unit tests for the new dimensions /
  blocker kinds (assert a sample violation is flagged/blocked, a clean sample passes) — per
  the repo's regression-test discipline.
- Structural assertions: the Builder prompt references `@zeroship/ui` + `ThemeProvider`; the
  new Critic dimensions + Reviewer blocker kinds are present and wired into the run.
- Do NOT run a full generation (needs `OPENAI_API_KEY` quota) — state clearly in the report
  what a live generation run would validate and that it's pending quota.

## Report (stdout)
- The prompt changes (how apps are told to compose from `@zeroship/ui`); the new Critic
  dimensions + Reviewer blocker kinds (names + what each checks/blocks); the §3 table update;
  unit tests added; all verification output; and exactly what remains for a quota'd
  generation e2e to confirm.

## Constraints
- Pre-launch, no back-compat. No network needed (no installs). NO OpenAI usage.
- Add regression tests for new gate logic (assert flag/block on a bad sample).
- **DO NOT commit / merge / push.**
