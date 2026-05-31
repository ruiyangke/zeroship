# Fix brief: catalog cohesion polish (final whole-surface review)

From the final whole-surface cohesion review of the 18 layouts+blocks pieces.
Cross-piece consistency only — no new components, no behavior change beyond the
intent rename. Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

Severity: 🔴 must-fix · 🟡 should-fix · 🟢 nit.

## 🔴 R1. Unify the intent vocabulary across blocks

Today: `BadgeIntent = neutral|info|success|warning|danger`,
`BannerIntent = info|success|warning|danger`,
`ErrorStateIntent = error|warning` (uses `error` where the others use `danger`).
Token mapping already agrees; only the NAMES diverge.

Fix:
- Create a shared base type `sdks/ui/src/blocks/_intent.ts`:
  ```ts
  /** Canonical status-intent vocabulary shared across status-bearing blocks. */
  export type Intent = "neutral" | "info" | "success" | "warning" | "danger";
  ```
  Re-export `Intent` from `blocks/index.ts` (and thus `src/index.ts`).
- `Badge`: `BadgeIntent = Intent` (unchanged set).
- `Banner`: `BannerIntent = Exclude<Intent, "neutral">` (neutral banner is
  meaningless — keep the subset, but DERIVE it; document the exclusion).
- `ErrorState`: `ErrorStateIntent = Extract<Intent, "warning" | "danger">`.
  **Rename `"error"` → `"danger"` everywhere in ErrorState**: the prop default
  (`intent = "danger"`), the `data-intent` value, the CSS selectors
  (`.zs-error-state--error` → `.zs-error-state--danger`), any
  `--zs-error-state-*` token names keyed on it, the JSDoc, AND the
  ErrorState stories (`intent="error"` → `intent="danger"`). Keep the same
  red token mapping. Re-run the ErrorState suite.

## 🔴 R2. Composed blocks must relabel their inner layout primitives' data-slot

EmptyState/ErrorState wrap their content in `<Center asChild><Stack
className="…__column">`; Banner uses an inner row `Stack` + text `Stack`. These
emit generic `data-slot="center"/"stack"` instead of the documented
`<name>-<part>` vocabulary, unlike AppShell/PageHeader which relabel.

Fix — first make ALL layout primitives honor a consumer `data-slot` uniformly
(Stack/Cluster/Split already do from the 2d fix; ADD the same to **Grid,
Container, Center** — `"data-slot"?: string` prop, destructure with the current
literal as default, emit it). This also resolves nit G3. THEN:
- `EmptyState`: pass `data-slot="empty-state-column"` to the element that owns
  the column (the `Center` if it's `asChild`-merged onto the column, else the
  `Stack`). Verify the rendered column node actually carries
  `empty-state-column` (Center asChild merges its data-slot onto the child, so
  set it on the Center).
- `ErrorState`: same → `data-slot="error-state-column"`.
- `Banner`: `data-slot="banner-row"` on the row Stack, `data-slot="banner-text"`
  on the text-column Stack.
**Regression:** an EmptyState (or ErrorState) story asserts its column node has
`data-slot="empty-state-column"` (fails pre-fix → reads `center`/`stack`).

## 🟡 Y1. Standardize the asChild-invalid-child dev guard on `console.warn`

The three `.Title` parts (`EmptyState.tsx`, `ErrorState.tsx`, `PageHeader.tsx`)
use `console.error`; every other surface (Badge + all layout roots, incl.
PageHeader's own root) uses `console.warn` for the identical "asChild given a
non-element child → render nothing" condition. Change the three `.Title`
`console.error` → `console.warn` (keep the `process.env.NODE_ENV` gate + the
`eslint-disable-next-line no-console`).

## 🟡 Y2. Tag focus-ring width token

`Tag.css` uses `--zs-button-focus-ring-width`; Banner/DataTable use
`--zs-focus-ring-width` for the same `outline`. Tag is a chip, not a button —
switch Tag's two occurrences to `--zs-focus-ring-width` (keep the shared
`--zs-focus-ring-color`/`-offset`).

## 🟡 Y3. AppShell hairline token

`AppShell.css` hardcodes `0.0625rem` for hairline borders (~2 places);
DescriptionList/DataTable use `var(--zs-selection-hairline)` for the same role.
Use `var(--zs-selection-hairline)` in AppShell so a global hairline change
propagates. (rem is allowed, but the token is the cohesion win.)

## 🟡 Y4. Dedupe the start/end side union

`AppShellSidebarSide` and `SplitSide` are byte-identical (`"start" | "end"`).
Add `export type Side = "start" | "end";` to
`sdks/ui/src/layouts/_layout-primitives.ts`; have `Split` (`SplitSide = Side`)
and `AppShell` (`AppShellSidebarSide = Side`) derive from it. KEEP the prop
names (`Split.side`, `AppShell.sidebarSide` — both clearer in context); only the
type is shared. Re-export `Side` from `layouts/index.ts`.

## 🟢 G1 + G2. DataTable type parity

- Promote `DataTable`'s inline `density: "comfortable" | "compact"` to a named
  exported `DataTableDensity` type (parity with every other tunable union).
- Add a one-line comment on `DataTableAlign` noting it intentionally narrows the
  shared layout `Align` (a cell can't "stretch").

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' layouts/ blocks/   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' layouts/ blocks/                # 0
grep -rn 'error' blocks/ErrorState/ErrorState.css   # no --error selectors remain
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6207 --silent &) ; sleep 3
# Re-run every touched suite:
for s in EmptyState ErrorState Banner Badge Tag StatCard DescriptionList DataTable Stack Grid Cluster Container Center Split AppShell PageHeader; do printf "%-16s " "$s"; npx test-storybook --config-dir .storybook --url http://127.0.0.1:6207 --maxWorkers=1 "$s.stories" 2>&1 | grep -oE 'Tests:.*total'; done
```

Report: the shared Intent/Side types + which pieces derive from them, the
ErrorState error→danger rename (files touched), the data-slot relabels (+ the
column regression assertion), Y1/Y2/Y3 fixes, the DataTableDensity export, build
results, grep counts (0/0, no --error selectors), and ALL touched suite pass
counts. Factual handoff.
