# Slice brief: FilterBar block

A governed search/filter toolbar for collections (lists, card grids, etc.):
[search input] · [active filter chips] · [spacer] · [actions]. Composes the
real Input + Tag + Button + Cluster. Lives in `sdks/ui/src/blocks/FilterBar/`.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference / compose
`components/Input` (search), `components/Tag` (removable active-filter chips),
`components/Button` (clear-all + actions), `layouts/Cluster`/`Stack` (layout),
`components/Icon` + lucide-react (search/filter glyphs, decorative). House:
forwardRef, data-slot, per-prop JSDoc.

## API
```ts
interface FilterBarActiveFilter { id: string; label: ReactNode; onRemove?: () => void; }
interface FilterBarProps extends ComponentPropsWithoutRef<"div"> {
  search?: string;                       // controlled search value (optional)
  onSearchChange?: (value: string) => void;
  searchPlaceholder?: string;            // default "Search…"
  activeFilters?: FilterBarActiveFilter[]; // rendered as removable Tags
  onClearFilters?: () => void;           // when set + activeFilters present → a "Clear" button
  actions?: ReactNode;                   // trailing actions slot (e.g. a "Filters"/"New" Button)
  children?: ReactNode;                   // optional extra controls between search and actions
}
```
- Layout (Cluster/flex, wraps on narrow): a search `Input type="search"` with a
  decorative search `Icon` in its `startSlot` (lead) when `search`/`onSearchChange`
  given; then the active-filter `Tag`s (each `removable`, firing its `onRemove`);
  an optional "Clear" Button (`variant="plain"`, fires `onClearFilters`) shown
  only when `activeFilters?.length`; `children`; a flex spacer; the `actions`
  slot at the end.
- If neither `search` nor `onSearchChange` is given, omit the search input
  (FilterBar can be just chips + actions).
- `forwardRef`, `data-slot="filter-bar"` (+ `-search`/`-filters`/`-actions`),
  per-prop JSDoc.

## a11y
Root is a `<div role="search">` ONLY when it has the search input (else a plain
div / `role="toolbar"`? — use `role="search"` when searchable, else no role).
Search Input labelled (`aria-label` from placeholder or an explicit prop).
Active-filter Tags use the real Tag's remove button (`aria-label="Remove {label}"`).
Decorative icons `aria-hidden`. Keyboard: native (input + buttons).

## Constraints
`--zs-*` tokens only; no raw hex/px; logical properties; forced-colors +
reduced-motion where relevant; no "HIG"/"Apple"; pre-launch no-back-compat. Export
from `blocks/index.ts`; `@import` CSS into styles.css (Composed blocks); story
`layout: "fullscreen"`.

## Stories (`src/stories/FilterBar.stories.tsx`)
- Default (search + 2 active filter chips + Clear + an actions Button),
  SearchOnly, FiltersOnly (no search), Empty.
- **play()**: type in the search → `onSearchChange` fires (use a fn spy + a
  controlled useState wrapper); click a chip's remove → its `onRemove` fires;
  click Clear → `onClearFilters` fires. axe clean.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/FilterBar|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/FilterBar|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6263 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6263 --maxWorkers=1 FilterBar.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6263.) Report: files, composition, a11y (role=search
gating), build/grep/suite counts, decisions.
