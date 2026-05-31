# Fix brief: PricingTable — dual-review merge (codex + claude)

PricingTable (`sdks/ui/src/sections/PricingTable/`) is uncommitted. Two reviews
(codex xhigh + claude) plus an orchestrator DOM measurement. **The featured-tier
"taller" concern is DEBUNKED** — measured: all three tiers are identical boxes
(top 131 / bottom 570 / h 439, CTA 506–546, no transform); the larger look is the
box-shadow/ring halo, not layout. No layout fix needed. The real findings:

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Run ALL commands with cwd INSIDE this worktree (nested under main repo; wrong cwd
builds the wrong checkout — `cd` in first).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## F1 🔴 — `featured` without `badge` signals "recommended" by ring/color ALONE
PricingTable.tsx:~364. A consumer sets `featured: true` but omits `badge` → the
recommended tier is conveyed only by the accent ring/elevation, violating the
house "never color/icon alone" rule (color-blind + SR users get no "recommended"
signal). **Fix:** when `featured && badge == null`, default a real visible
`<Badge>Most popular</Badge>` (intent=info, solid — match the existing string-badge
path) so a featured tier ALWAYS carries a visible/AT-readable recommendation label.
(A consumer-supplied `badge` still overrides.) Document the default.
**Regression** (`FeaturedNoBadge` story): `<PricingTable tiers={[{…featured:true,
no badge}, …]}/>`; play(): assert the featured tier renders visible "Most popular"
text (`getByText`). **Pre-fix: no badge text → fails.**

## F2 🟡 — React keys not namespaced across the two additive surfaces
PricingTable.tsx:~483. Prop tiers map with `key={tier.id}`; compound tiers with
`key={child.key ?? idx}`. A prop tier `id="pro"` + a compound child `key="pro"`
(or no key → idx collision with a prop index) yields duplicate keys in the merged
`allTiers` map → React reconciliation hazard + console error. **Fix:** namespace by
source — `prop:${tier.id}` and `compound:${child.key ?? index}`.
**Regression** (`KeyCollision` story): one prop tier `id="pro"` AND one compound
`<PricingTable.Tier>` whose React key resolves to `"pro"`; play(): spy on
`console.error`, assert NO "Encountered two children with the same key" warning AND
both tiers render (heading count). **Pre-fix: duplicate-key console.error → fails.**

## F3 🟡 — `hasTitle = title != null` treats `false` / `""` as a rendered title
PricingTable.tsx:~511. Common conditional JSX `title={showTitle && "Pricing"}`
yields `title={false}` when `showTitle` is false; `false != null` is true → an
empty `<h2>` renders and the section gets a useless `aria-labelledby` pointing at an
empty heading. **Fix:** a renderability guard — treat `false`, `null`, `undefined`,
and `""` as "no title": `const titleRenders = title != null && title !== false &&
title !== "";` Gate BOTH the `<h2>` render and the `aria-labelledby` on
`titleRenders`.
**Regression** (`FalseTitleNoLabel` story): `<PricingTable title={false} tiers={…}/>`;
play(): assert `root.hasAttribute("aria-labelledby") === false` AND no empty
`[data-slot="pricing-table-title"]` heading. **Pre-fix: attr present + empty h2 →
fails.**

## F4 🟡 — CSS raw-rem literals where tokens / named knobs belong
PricingTable.css. The px-gate passes (these are rem, not px), but for house
consistency:
- `:283` `border: 0.0625rem solid CanvasText` → use `var(--zs-selection-hairline)`
  (the house 1px-hairline token — Radio/Toolbar use it; PricingTable should too).
- `:120` featured ring `0.125rem`, `:289` forced-colors `0.1875rem`, `:62` title
  lead `max-inline-size: 40rem`, `:222` badge nudge `0.125rem` → introduce local
  named custom properties at the `.zs-pricing` scope (the house "single-knob" idiom,
  cf. `--zs-auth-form-width`, `--zs-hero-measure`): e.g.
  `--zs-pricing-ring-width: 0.125rem; --zs-pricing-lead-measure: 40rem;` and the
  forced-colors featured border can be `calc(var(--zs-pricing-ring-width) * 1.5)` or
  its own token. Reference the vars instead of the literals so they're tunable + the
  intent is named. (Keep values identical — visual must not change; re-capture to
  confirm.)
- These are calibration, not bugs; no runtime regression test (CSS-only). Confirm
  via the hex/px grep staying 0/0 and the visual recapture being pixel-equivalent.

## F5 🟢 — empty-features fallback lacks its slot + dangling Separator
PricingTable.tsx:~399. A tier with zero features renders a `<Separator>` + an
aria-hidden grow `<div>` that lacks `data-slot="pricing-features"`. **Fix:** give the
fallback grow region `data-slot="pricing-features"` (consistent slot targeting) AND
suppress the `<Separator>` when there are no features (no hairline above an empty
region). No regression test required (untested path; verify build green).

## C1 🟡 (claude) — dead `cloneElement` import
PricingTable.tsx:~89. `cloneElement` is imported but never used (PricingTable
resolves tiers into plain objects, never clones — unlike Hero). **Fix:** drop it
from the react import list.

## C2 🟡 (claude) — redundant mobile-collapse rule
PricingTable.css:~92-98. `@media (max-width: 47.999rem){ .zs-pricing__grid{
grid-template-columns: 1fr } }` is a no-op: the composed `Grid` already collapses to
a single column below 48rem (its responsive base count is always 1; the
`columns={{md}}` only promotes at ≥48rem). The rule's comment claims it "overrides
the promoted column count" which is inaccurate at this width. **Fix:** DELETE the
redundant `@media` rule and update the JSDoc/comments in BOTH PricingTable.tsx
(:50, :504) and PricingTable.css (:41) that reference the 47.999rem collapse to
state that the Grid primitive owns the collapse (no local override). (This removes
the 47.999rem literal from this section — that's fine; the collapse still happens
via Grid.)

## Constraints (unchanged)
`--zs-*` only; no raw hex/px; logical properties; forced-colors + reduced-motion;
no "HIG"/"Apple"; pre-launch no-back-compat (no shims/aliases). forwardRef +
data-slot preserved. Keep comments/JSDoc HONEST about implemented behavior. Preserve
stable keys. Each 🔴/testable-🟡 ships a fail-pre-fix regression story (F1, F2, F3);
F4/F5/C1/C2 verified via build + grep + visual recapture.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
node -e "const x=require('./sdks/ui/dist/index.js'); console.log('PricingTable export:', !!x.PricingTable)"
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' sections/PricingTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' sections/PricingTable|wc -l)"
grep -n 'cloneElement' sections/PricingTable/PricingTable.tsx   # must be EMPTY (C1)
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6292 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6292 --maxWorkers=1 PricingTable.stories 2>&1 | grep -E 'Tests:|✕'
```
Report: the F1 default-badge contract, the per-test PRE-FIX failure output for
F1/F2/F3 (proof they bite), the CSS token/knob changes (F4) with confirmation the
visual is unchanged, F5/C1/C2, final build/grep/suite counts, decisions.
