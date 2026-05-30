# Refactor brief: promote 5 primitives blocks/ → components/

Move the atomic primitives `Spinner`, `Skeleton`, `Badge`, `Tag`, `Breadcrumbs`
(+ the shared `_intent.ts`) from `sdks/ui/src/blocks/` to
`sdks/ui/src/components/`. Pure internal taxonomy refactor — the public API is
flat (root `index.ts` re-exports all layers), so `import { Spinner } from
"@zeroship/ui"` is UNCHANGED. No behavior/markup/CSS-content change; only file
location, import paths, barrels, `@import` paths, and Storybook `title:` group.

> **DO NOT commit, push, or merge.** Apply + self-verify + report.
> Pre-launch, no back-compat. Use `git mv` so history follows.

## Moves (use `git mv`)
- `src/blocks/Spinner/`      → `src/components/Spinner/`
- `src/blocks/Skeleton/`     → `src/components/Skeleton/`
- `src/blocks/Badge/`        → `src/components/Badge/`
- `src/blocks/Tag/`          → `src/components/Tag/`
- `src/blocks/Breadcrumbs/`  → `src/components/Breadcrumbs/`
- `src/blocks/_intent.ts`    → `src/components/_intent.ts`  (Badge depends on it;
  blocks→components is the correct dependency direction)

## Import-path fixes (exact)
Inside each MOVED component file (`<Name>.tsx`): the depth to the shared
internals changes (now siblings):
- `from "../../components/_classnames"` → `from "../_classnames"`
- `from "../../components/_slot"`       → `from "../_slot"`
- Badge `from "../_intent"` → STAYS `from "../_intent"` (because `_intent.ts`
  also moves into `components/`, so it's still a sibling).

Consumers that import the moved pieces (repath these):
- `src/blocks/DataTable/DataTable.tsx`: `import { Skeleton } from "../Skeleton"`
  → `from "../../components/Skeleton"`. (Its `../EmptyState` import STAYS —
  EmptyState remains a block; its `../../components/Checkbox` STAYS.)
- `src/layouts/PageHeader/PageHeader.tsx`: `from "../../blocks/Breadcrumbs"`
  → `from "../../components/Breadcrumbs"`.
- `src/blocks/Banner/Banner.tsx`: `import type { Intent } from "../_intent"`
  → `from "../../components/_intent"`.
- `src/blocks/ErrorState/ErrorState.tsx`: `import type { Intent } from "../_intent"`
  → `from "../../components/_intent"`.

(Spinner/Skeleton/Tag/Breadcrumbs import nothing else internal beyond
_classnames/_slot. Verify by grep after the move — see verification.)

## Barrels
- `src/blocks/index.ts`: REMOVE the export blocks for Spinner, Skeleton, Badge,
  Tag, Breadcrumbs, AND the `export type { Intent } from "./_intent";` line.
- `src/components/index.ts`: ADD exports for the 5 (component + all their
  public types/parts — copy the exact export shapes that were in blocks/index.ts)
  AND `export type { Intent } from "./_intent";`. Place them sensibly (e.g. near
  Progress/Meter for Spinner/Skeleton; the others alphabetically/where they fit).
- Root `src/index.ts` is unchanged (it already `export * from "./components"`
  and `"./blocks"`). Confirm `Intent` + all 5 still resolve from the root.

## styles.css
`src/styles.css` — change the 5 `@import` paths from `./blocks/<Name>/<Name>.css`
→ `./components/<Name>/<Name>.css` (Skeleton, Spinner, Badge, Tag, Breadcrumbs).
Move them out of the "Composed blocks" group into the component-import group
(keep the file's existing grouping comments coherent).

## Stories (5 files in src/stories/)
For `Spinner`/`Skeleton`/`Badge`/`Tag`/`Breadcrumbs`.stories.tsx:
- import: `from "../blocks"` → `from "../components"`.
- `title: "Blocks/<Name>"` → `title: "Components/<Name>"`.
(Story files stay in src/stories/ — only the import + title change. PageHeader,
DataTable, EmptyState, ErrorState, Banner, StatCard, DescriptionList stories +
titles are UNCHANGED — those stay blocks.)

## Verification (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build                 # ESM + DTS clean (catches any bad import path)
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
# No stale blocks/ paths to the moved pieces remain, and no components→blocks _intent dep:
grep -rn 'blocks/\(Spinner\|Skeleton\|Badge\|Tag\|Breadcrumbs\)' --include='*.ts' --include='*.tsx' --include='*.css' . ; echo "^expect none"
grep -rn 'from "\.\./\.\./blocks/_intent"\|components/_intent' . ; echo "^components import _intent via ../ only"
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' components/Spinner components/Skeleton components/Badge components/Tag components/Breadcrumbs ; echo "^expect 0 hex"
# Root exports intact:
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
node -e "const x=require('./sdks/ui/dist/index.js'); ['Spinner','Skeleton','Badge','Tag','Breadcrumbs'].forEach(n=>console.log(n, !!x[n]))"
# Suites — the 5 moved + the consumers (must all still pass):
cd sdks/ui && (npx http-server storybook-static -p 6239 --silent &) ; sleep 3
for s in Spinner Skeleton Badge Tag Breadcrumbs DataTable PageHeader Banner ErrorState; do printf "%-13s " "$s"; npx test-storybook --config-dir .storybook --url http://127.0.0.1:6239 --maxWorkers=1 "$s.stories" 2>&1 | grep -oE 'Tests:.*total'; done
```
(Dev server on :6006 — use 6239.)

Report: confirm the 6 `git mv`s, the exact import-path edits made, barrels
updated, styles.css @imports moved, the 5 story title+import changes, build
results, the grep results (no stale blocks/ paths), root-export check for all 5,
and ALL 9 suite pass counts. Factual handoff.
