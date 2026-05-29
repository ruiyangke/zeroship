# Fix brief: Layout primitives (Wave 1) — merged code-review fixes

Merges the codex (CHANGES-NEEDED) and claude code-reviewer (APPROVE-WITH-NITS)
reviews. Worktree: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report. The
> orchestrator verifies gates and commits.

Severity legend: 🔴 real bug · 🟡 calibration · 🟢 nit.

---

## 🔴 1. Grid responsive `columns` object — wrong mobile base

`sdks/ui/src/layouts/Grid/Grid.tsx` (`baseColumns` logic, ~line 95–104).

Today an object like `{ lg: 4 }` sets the base `--grid-cols` to the smallest
*provided* value (4), so the grid shows 4 columns at ALL widths, including
below `sm`. The mobile-first contract (and the Tailwind mental model) is:
**an object's base is always 1; provided breakpoints promote upward.** So
`{ lg: 4 }` = 1 column until `lg`, then 4; `{ sm: 2, lg: 4 }` = 1 below sm,
2 from sm, 4 from lg.

**Fix:** for object `columns`, set `baseColumns = 1` unconditionally (drop the
`columns.sm ?? columns.md ?? columns.lg ?? 1` expression — it becomes just
`1`). A numeric `columns` is unchanged (base = the number). The existing
sm/md/lg media fallback chain in `Grid.css` already promotes correctly once
the base is 1 — no CSS change needed; verify.

**Regression test (MANDATORY — must fail on pre-fix code):** add a `play()` to
the responsive Grid story that renders `<Grid columns={{ lg: 4 }} data-testid="grid-resp">`
and asserts the computed base custom property is `"1"`:
```ts
const grid = canvas.getByTestId("grid-resp");
expect(getComputedStyle(grid).getPropertyValue("--grid-cols").trim()).toBe("1");
```
Pre-fix this reads `"4"` (fails); post-fix `"1"` (passes). This is viewport-
independent (asserts the base token, not the media-query result), so it is
stable under the Test Runner's default viewport.

Also update the `columns` JSDoc on `GridProps` and the responsive story's
description text (`sdks/ui/src/stories/Grid.stories.tsx` ~line 91) to state the
base-1 mobile-first rule.

## 🟡 2. Remove `"baseline"` from the `Align` union (spec fidelity)

`sdks/ui/src/layouts/_layout-primitives.ts:8` and the `ALIGN` map.

The approved spec (§4) defines `Align = "start" | "center" | "end" | "stretch"`.
The current union adds `"baseline"`. Remove `"baseline"` from the `Align` type
AND its entry in the `ALIGN` record. Grep the layouts + stories for any use of
`align="baseline"` and remove (there should be none). Do not amend the spec —
the spec is the contract.

## 🟡 3. `asChild` invalid-child dev-warn (parity with Card)

All six roots (`Stack`, `Grid`, `Cluster`, `Container`, `Center`, `Split`) plus
`Split.Side` and `Split.Main`.

Card dev-warns when `asChild` is set but the child isn't a valid single element
(`Card.tsx` dev-mode `isValidElement` block). The layout primitives currently
rely on `Slot` rendering nothing for an invalid child — a silent disappearance.
Mirror the Card pattern: destructure `children`, and in a
`process.env.NODE_ENV !== "production"` block, when `asChild && !isValidElement(children)`,
`console.warn` a short message naming the component (e.g.
`"Stack asChild requires a single React element child; rendering nothing."`).
Keep the warn message style consistent across the eight surfaces. The branch
must DCE in production (gated on `process.env.NODE_ENV`), matching Card.

## 🟡 4. Add missing root `asChild` stories

- `sdks/ui/src/stories/Grid.stories.tsx` — add a `Grid asChild` smoke story
  rendering `<Grid asChild><section …/></Grid>` with a `play()` asserting the
  rendered tag is `SECTION` and carries the `zs-grid` class (mirror the
  Stack/Cluster/Container/Center asChild stories).
- `sdks/ui/src/stories/Split.stories.tsx` — add a `Split asChild (root)` story
  rendering `<Split asChild><section>…<Split.Side/><Split.Main/>…</section></Split>`
  asserting the root tag swap + `zs-split` class. (The existing story only
  covers `Split.Side`/`Split.Main` asChild.)

## 🟢 5. Per-prop JSDoc

- `sdks/ui/src/layouts/Split/Split.tsx:58–63` — add `/** Render-as the single
  child element rather than a `<div>`. */` to `asChild` on `SplitSideProps` and
  `SplitMainProps`.
- `sdks/ui/src/layouts/Grid/Grid.tsx:36` (`GridColumns`) — add a one-line JSDoc
  to each of `sm` / `md` / `lg` (e.g. `/** Columns at the --zs-bp-sm breakpoint
  and up. */`).

---

## Verification (run and report; DO NOT commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' layouts/   # expect 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' layouts/                # expect 0
```

Report: the exact Grid.tsx diff, confirmation `baseline` is gone from the union
+ map, the dev-warn added to all 8 surfaces, the two new asChild stories, build
results, the two grep counts, and confirm the new Grid regression `play()`
asserts `--grid-cols === "1"`.
