# Fix brief: Content blocks (Wave 2c) — merged code-review fixes

Merges codex (CHANGES-NEEDED) + claude (APPROVE). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`. Two real
🔴 (one touches the shipped Card + the 2a blocks), one 🟡.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## 🔴 1. StatCard root `data-slot` is clobbered by Card

`sdks/ui/src/blocks/StatCard/StatCard.tsx` + `sdks/ui/src/components/Card/Card.tsx`.

StatCard renders a `Card` and sets `data-slot="stat-card"`, but `Card`'s own
`dataProps` (`data-slot: "card"`) is spread AFTER `...rest`, so the root always
ends up `data-slot="card"` — the stat-card slot never lands (consumers can't
target it via the documented data-slot vocabulary).

**Fix (general, in Card):** make `Card` honor a consumer-supplied `data-slot`.
In `Card.tsx`, destructure `"data-slot": dataSlot = "card"` from props and use
`dataSlot` in `dataProps` (so a composing block can override the root slot;
default unchanged = `"card"`). Then StatCard passes `data-slot="stat-card"` and
it lands. Verify Card's OWN stories still show `data-slot="card"` by default
(unchanged behavior).

**Regression test (MANDATORY — fail pre-fix):** in `StatCard.stories.tsx`, the
existing `Up` `play()` (keep it) asserts the root:
```ts
expect(canvas.getByTestId("statcard-up")).toHaveAttribute("data-slot", "stat-card");
```
Pre-fix this reads `"card"` (fails); post-fix `"stat-card"` (passes). Ensure the
StatCard root carries that `data-testid`.

## 🔴 2. Root `asChild` silently drops the composed body — remove it from composed-body blocks

`sdks/ui/src/blocks/Banner/Banner.tsx` (this slice) AND
`sdks/ui/src/blocks/EmptyState/EmptyState.tsx` +
`sdks/ui/src/blocks/ErrorState/ErrorState.tsx` (same latent bug, shipped in 2a).

`asChild` routes through `Slot`, which renders ONLY the consumer's child —
discarding the block's composed body (icon/title/description/actions/dismiss).
The file-header comment claiming "the ergonomic content still renders inside"
is FALSE. `asChild` is fundamentally wrong for a block whose purpose IS the
composed body.

**Fix:** REMOVE root `asChild` from all three (`EmptyState`, `ErrorState`,
`Banner`):
- Delete the `asChild` prop from `*Props`, the `Slot` branch, the
  `asChild`-invalid-child dev-warn, and the now-unused `Slot` import (only if
  unused after — Title parts may still use Slot).
- The root always renders its native element (`<div>` / region).
- Fix the file-header JSDoc: remove the false "renders inside" claim; add a line
  "This block has no root `asChild` — its value is the composed body; wrap the
  block in your own element for a custom/semantic root."
- **KEEP** the `.Title` part's `asChild` (releveling the heading is correct —
  Title wraps text, Slot is right there).
- Remove or rewrite any ROOT-`asChild` stories for these blocks (e.g. an
  EmptyState/ErrorState/Banner "asChild" story) — do NOT leave a story
  exercising a removed prop. (Layout primitives Stack/Grid/etc. and Badge KEEP
  their `asChild` — they have no composed body, so Slot is correct; do NOT touch
  them.)

## 🟡 3. Banner `dismissible` without `onDismiss` → no-op focusable button

`sdks/ui/src/blocks/Banner/Banner.tsx` (~L170).

Render the dismiss `<button>` ONLY when `onDismiss` is provided. Add a
`process.env.NODE_ENV!=="production"` dev-warn when `dismissible` is true but
`onDismiss` is missing ("Banner dismissible requires onDismiss; the dismiss
button is not rendered"). Update the `dismissible` JSDoc.

## Won't-fix (note)

- claude/codex 🟢 StatCard `play()` exists though brief said "no play": KEEP it
  — it now carries the data-slot regression assertion above and a useful
  delta-a11y check. Beneficial, not a defect.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' blocks/   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' blocks/                # 0
grep -rn 'asChild' blocks/EmptyState blocks/ErrorState blocks/Banner | grep -iv 'title'   # only Title-part asChild should remain
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6175 --silent &) ; sleep 3
# Re-run ALL touched suites (2a + 2c) since EmptyState/ErrorState changed:
for s in EmptyState ErrorState StatCard Banner DescriptionList Card; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6175 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: the Card data-slot-override diff, confirmation StatCard root now reads
`data-slot="stat-card"` (+ the regression play), the three blocks' asChild
removal (+ which stories were removed/updated), the Banner dismissible guard,
build results, grep counts, and ALL suite pass counts (EmptyState/ErrorState
must still pass after asChild removal; Card must still pass after the data-slot
change).
