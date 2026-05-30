# Slice brief: DataTable — migrate inline SVGs to Icon (Lucide)

Replace DataTable's three hand-rolled inline `<svg>` glyphs with the governed
`Icon` (Lucide) primitive — consistent with the new sections/blocks (Hero,
PricingTable, FilterBar, Stepper all use `Icon`+lucide-react). Glyph RENDERING
ONLY: do NOT touch the sort logic, `aria-sort` wiring, the Menu/RowActions, the
TanStack engine, or the public API. Lives in `sdks/ui/src/blocks/DataTable/`.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Run ALL commands with cwd INSIDE this worktree (nested under main repo; wrong cwd
builds the wrong checkout — `cd` in first).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## The three glyphs (DataTable.tsx)
1. **`SortGlyph({state})`** (lines ~485-498) — currently one 12×12 svg with two
   paths (`.zs-data-table__sort-up` / `-down`) that CSS dims/highlights by
   `data-state` (ascending → up caret accent, descending → down caret accent, none
   → both muted). aria-hidden (the `<th aria-sort>` carries the meaning).
2. **`BoolGlyph({value})`** (lines ~501-530) — check (true) / horizontal dash
   (false), `currentColor`, colored by `.zs-data-table__bool-glyph[data-value]`.
3. **`KebabGlyph()`** (lines ~532-540) — three vertical dots; the RowActions
   trigger `Button` carries `aria-label="Row actions"`, glyph is aria-hidden.

## Migration (map each to a Lucide glyph via Icon)
Import from `lucide-react` and render through `components/Icon` (`<Icon as={…}
…/>`). Mirror how `blocks/FilterBar`, `blocks/Stepper`, `sections/PricingTable`
already use Icon+Lucide (read one for the exact pattern + sizing idiom).

- **SortGlyph** → swap the icon BY STATE (idiomatic, clearer than the dimmed pair):
  - `none` → `ChevronsUpDown` (the up+down chevron pair), muted (`--zs-label-tertiary`)
  - `ascending` → `ChevronUp`, accent (`--zs-accent`)
  - `descending` → `ChevronDown`, accent
  Keep the `aria-hidden` wrapper + `data-state` on it (drives the color). The glyph
  is decorative — `<Icon>` with NO `label` (so it's aria-hidden internally too).
- **BoolGlyph** → `value ? Check : Minus` (same Lucide glyphs PricingTable uses —
  already confirmed present in the installed lucide-react). Keep `data-value` +
  `--zs-data-table__bool-glyph` color rules (true → success token, false → muted).
  aria-hidden.
- **KebabGlyph** → `MoreVertical` (VERIFY this export exists in the installed
  `lucide-react`; if not, use `EllipsisVertical`). aria-hidden.

**VERIFY the Lucide export names against the installed package** before using them
(`node -e "const l=require('lucide-react'); console.log(['ChevronsUpDown','ChevronUp',
'ChevronDown','Check','Minus','MoreVertical','EllipsisVertical'].map(n=>n+':'+!!l[n]))"`).
Use whichever vertical-ellipsis name resolves true.

## CSS rework (DataTable.css)
- **Sort** (lines ~187-220): keep `.zs-data-table__sort-glyph` as the SIZED wrapper
  (`inline-size/block-size: 0.75em` so it tracks the header text; default color
  `--zs-label-tertiary`; `data-state="ascending"/"descending"` → `color:
  var(--zs-accent)`). DELETE the now-obsolete `.zs-data-table__sort-glyph svg`,
  `.zs-data-table__sort-up`, `.zs-data-table__sort-down`, and the per-path opacity/
  data-state rules. The `Icon`'s lucide svg inherits `currentColor` and should fill
  the wrapper (`inline-size/block-size: 100%` on the rendered svg if Icon doesn't
  already) — match the sibling Icon sizing approach. Net: the whole glyph is muted
  when unsorted, full-accent when active (single-color, cleaner).
- **Bool** (lines ~382-392): the `.zs-data-table__bool-glyph` color rules stay; just
  ensure they target the Icon's svg / the element you put the class on. Size to the
  existing footprint.
- **forced-colors** (lines ~330-336, ~461): the sort + bool forced-colors rules must
  keep working. Since Icon uses `currentColor`, setting the wrapper `color:
  CanvasText`/`Highlight` flows through. Update the selectors that referenced the
  deleted `.sort-up/.sort-down` paths — assert the active sort + bool glyphs stay
  distinguishable in forced-colors (CanvasText / Highlight), conveyed by SHAPE
  (different glyph per state) not color alone.

## Constraints
`--zs-*` only; no raw hex/px (rem/%/em/oklch ok); logical properties; forced-colors
+ reduced-motion preserved; no "HIG"/"Apple"; pre-launch no-back-compat. NO public
API change. NO change to sort logic / aria-sort / Menu / TanStack / managed-vs-
controlled behavior. forwardRef + data-slot unchanged.

## Regression net = the 12 EXISTING DataTable stories (FROZEN)
This is a mechanical glyph swap with NO API/logic change, so the existing 12 stories
ARE the regression net — they MUST stay green + axe-clean, and the sort/boolean/
kebab interactions must still work. Do NOT rewrite them. If a story's `play()`
queried the OLD svg path/structure, update ONLY that query to the new Icon DOM
(note any such change). No NEW regression test is required (no bug being fixed —
this is a refactor under a frozen behavioral net); but if the migration surfaces a
genuine defect, that fix ships its own fail-pre-fix test.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
node -e "const l=require('./node_modules/lucide-react'); console.log(['ChevronsUpDown','ChevronUp','ChevronDown','Check','Minus','MoreVertical','EllipsisVertical'].map(n=>n+':'+!!l[n]).join(' '))"
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "inline-svg left in DataTable.tsx (target 0): $(grep -c '<svg' blocks/DataTable/DataTable.tsx)"
echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/DataTable|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6293 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6293 --maxWorkers=1 DataTable.stories 2>&1 | grep -E 'Tests:|✕'
```
Report: the Lucide-name verification output, the three glyph swaps + the CSS rules
deleted/kept, any story `play()` query updated (and why), inline-svg count (→0),
build/hex-px/suite counts (expect all 12 green + axe clean), decisions.
