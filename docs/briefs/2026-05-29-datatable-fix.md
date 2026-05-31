# Fix brief: DataTable (Wave 3) — merged code-review fixes

Merges codex (CHANGES-NEEDED, 2🔴) + claude (APPROVE). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`. One fix is
cross-cutting (EmptyState/ErrorState description contrast — a latent a11y
regression from the 2a visual polish).

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

Severity: 🔴 real bug · 🟡 calibration · 🟢 nit.

## 🔴 1. Opaque DataTable surface + fix the EmptyState/ErrorState description contrast (the real bug the transparent root was hiding)

The DataTable root was made TRANSPARENT to dodge an axe-serious contrast fail:
the default EmptyState description (`--zs-label-tertiary`, footnote size) is
~4.46:1 on `--zs-surface` (below AA 4.5:1). The transparent root masks this; a
data table is a content surface and SHOULD be opaque. Fix both:

a) **`blocks/DataTable/DataTable.css`** — paint an **opaque** surface on the
   table/body (`background: var(--zs-surface)` on the table or tbody region) per
   the package's opaque-surface/glass rule. Keep the sticky header + selected
   row opaque too (see #4).
b) **`blocks/EmptyState/EmptyState.css` + `blocks/ErrorState/ErrorState.css`** —
   change the `__description` color from `--zs-label-tertiary` back to
   **`--zs-label-secondary`** (which clears AA on `--zs-surface`). KEEP the
   footnote SIZE from the 2a polish — the description stays visually softer than
   the original via size, but regains AA via the secondary color. Also check
   `blocks/Banner/Banner.css` description: if it uses `--zs-label-tertiary` on an
   opaque/tinted bg, bump it to `--zs-label-secondary` too.

**Regression (MANDATORY):** the DataTable **Empty** story must render on the now
**opaque** table surface and pass axe (it would FAIL pre-fix once the surface is
opaque, unless the EmptyState description is fixed — proving the fix). Re-run the
EmptyState + ErrorState suites (must still pass; secondary is darker → safe).

## 🔴 2. Select-all must preserve off-data (other-page/filter) selections

`blocks/DataTable/DataTable.tsx` (~L295, the select-all clear/select logic).

When all visible rows are selected, clearing emits `[]`, wiping keys for rows
NOT in the current `data` (other pages/filters). The contract is visible-row
selection that does not touch off-data keys.
- **Select-all:** append only the MISSING visible keys to the existing
  `selectedKeys` (preserve existing order + off-data keys).
- **Clear-all:** remove only the VISIBLE keys from `selectedKeys` (keep off-data
  keys).

**Regression (MANDATORY):** in the SelectableMultiple story, seed
`selectedKeys` with an off-data key (one not in `data`); after select-all then
clear-all, assert that off-data key SURVIVES (fails pre-fix where clear emits
`[]`).

## 🟡 3. Header `<th>` must use an opaque background-color, not a gradient

`blocks/DataTable/DataTable.css` (~L61, the sticky `<th>` background).

The header uses `background-image: linear-gradient(...)`, which breaks axe's
contrast walk (the documented glass-surface invariant: opaque
`background-color`, never a gradient/filter alone, behind text). Replace the
header background with an opaque `background-color` (a computed `color-mix`
into `--zs-surface` is fine) so `background-image` is `none` behind the header
text. Sticky header stays opaque so rows scroll under it.

## 🟢 4. `renderError` truthiness

`blocks/DataTable/DataTable.tsx` (~L321).

`renderError` is documented "when provided + truthy". Compute the node once and
gate on a real truthy check so `""`/`0`/`false` don't render a blank error row:
`const errorNode = renderError?.(); const showError = errorNode != null && errorNode !== false;` render `errorNode`.

## Won't-fix (deliberate, with rationale)

- **codex 🟡 loading skeleton per-cell vs full-colSpan.** KEEP the per-cell
  skeletons. "Skeleton rows" (the brief) is satisfied — a skeleton bar per
  column is valid table structure AND the premium loading pattern (mirrors the
  populated table; Linear/Vercel do this) vs a single full-width bar which reads
  cheaper. Ensure the loading row's cells structurally match the columns
  (including the selection column). Add a one-line comment noting the choice.
- claude 🟢 `aria-selected` on a `role=table` row: brief-mandated, axe-clean,
  AT-tolerant; promoting to `role=grid` pulls in keyboard-nav expectations.
  Leave.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' blocks/   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' blocks/                # 0
grep -n 'background-image' blocks/DataTable/DataTable.css   # header should NOT use a gradient behind text
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6195 --silent &) ; sleep 3
# DataTable + the cross-cutting blocks must all stay green:
for s in DataTable EmptyState ErrorState Banner; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6195 --maxWorkers=1 "$s.stories" 2>&1 | grep -E 'Tests:|✕'; done
```

Report: the opaque-surface change + the EmptyState/ErrorState (+Banner?)
description tertiary→secondary diff, the select-all off-data-preservation diff +
its regression assertion, the header opaque-bg change (background-image gone),
the renderError truthiness fix, the loading per-cell rationale, build results,
grep counts, and the 4 suite pass counts (DataTable Empty axe-clean on the
opaque surface).
