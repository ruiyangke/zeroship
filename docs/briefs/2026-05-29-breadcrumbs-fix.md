# Fix brief: Breadcrumbs — merged code-review fixes

Merges codex (CHANGES-NEEDED, 2🔴) + claude (APPROVE-WITH-NITS). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
File: `sdks/ui/src/blocks/Breadcrumbs/Breadcrumbs.tsx` (+ `.css`), the story, and
`layouts/PageHeader/PageHeader.tsx`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

Severity: 🔴 real bug · 🟡 calibration · 🟢 nit.

## 🔴 1. Auto-separators skip React.Fragment children

`withAutoSeparators` walks `Children.toArray(children)`, but `Children.toArray`
flattens arrays NOT Fragments — so `<Breadcrumbs><><Breadcrumbs.Item/>…</></Breadcrumbs>`
(the common map/conditional shape) yields ONE fragment child and gets no
separators.

**Fix:** before the `child.type === BreadcrumbsItem` walk, recursively flatten
Fragment children — for each node, if `isValidElement(node) && node.type ===
React.Fragment`, splice in `Children.toArray(node.props.children)` (recurse for
nested fragments); else keep the node. Run the existing separator-insertion on
the flattened list.
**Regression (MANDATORY):** add a Breadcrumbs story rendering the compound parts
wrapped in a `<>…</>` Fragment; `play()` asserts the rendered `<ol>` contains the
expected separators (count = items-1). Fails pre-fix (0 separators).

## 🔴 2. Collapse renders an ellipsis that hides nothing

With `maxItems={1}` (head=1, tail clamped to 1) and 2 crumbs, `hiddenCount = 2 -
1 - 1 = 0`, yet `shouldCollapse` (`length > maxItems`) is true → an ellipsis
button with `aria-label="Show 0 hidden breadcrumbs"`.

**Fix:** compute `hiddenCount` first and only collapse when `hiddenCount > 0`
(i.e. `shouldCollapse = list.length > maxItems && hiddenCount > 0`); otherwise
render the full trail. Also drop the redundant/dead second predicate
(`maxItems < list.length`, claude 🟡 #2) — fold into the single guard.
**Regression (MANDATORY):** a story/assertion that a `maxItems` value which
would hide nothing renders NO ellipsis (all crumbs present). Fails pre-fix
("Show 0 hidden").

## 🟡 3. Current page must stay visible when collapsed

If a crumb is explicitly flagged `current: true` and the resolved `currentIndex`
falls in the collapsed (hidden) middle, no rendered crumb gets
`aria-current="page"` while collapsed.

**Fix:** when collapsing, ensure the current crumb is visible — extend the tail
to start no later than `currentIndex` (`tailStart = Math.min(tailStart,
currentIndex)`), so the current crumb (and the trail after it) stays rendered.
Update the `aria-current` count/`hiddenCount` accordingly. Document that the
current crumb is always kept visible.

## 🟢 4. PageHeaderBreadcrumbsProps should omit `data-slot`

`layouts/PageHeader/PageHeader.tsx` — `PageHeaderBreadcrumbsProps = BreadcrumbsProps`
exposes `data-slot`, but the wrapper always forces `"page-header-breadcrumbs"`.
Change the type to `Omit<BreadcrumbsProps, "data-slot">` so the public API
matches runtime behavior.

## 🟢 5. Current-page font-weight token

`Breadcrumbs.css` — the current-page rule uses a raw `font-weight: 600`. Use the
type-scale weight token instead (e.g. `var(--zs-text-headline-weight)` /
`--zs-text-footnote-weight` — whichever matches the intended emphasis), for
token-purity parity with `.zs-breadcrumbs__list`.

## Optional (🟢, only if trivial)

- claude 🟢 expand-remount: the collapsed/expanded key prefixes
  (`head-`/`tail-`/`crumb-`) change on toggle, remounting all crumbs. If cheap,
  key crumbs by a stable identity (e.g. the item index in the full list) so
  toggling reconciles instead of remounts. Skip if it complicates the collapse
  logic — it's purely an efficiency nit.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' blocks/Breadcrumbs   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' blocks/Breadcrumbs                # 0
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6215 --silent &) ; sleep 3
for s in Breadcrumbs PageHeader; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6215 --maxWorkers=1 "$s.stories" 2>&1 | grep -E 'Tests:|✕'; done
```
NOTE: a Storybook DEV server runs on 6006 — use 6215 for the static runner.

Report: the Fragment-flatten fix + the new Fragment story/assertion, the
hiddenCount>0 collapse guard + its regression + the dropped dead clause, the
current-stays-visible fix, the Omit type, the font-weight token, build results,
grep counts, and the 2 suite pass counts.
