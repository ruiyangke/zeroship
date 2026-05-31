# Fix brief: Chip blocks (Wave 2b) — merged code-review fixes

Merges codex (CHANGES-NEEDED) + claude (APPROVE-WITH-NITS). Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

Severity: 🔴 real bug · 🟡 calibration · 🟢 nit.

## 🔴 1. Tag filter hover outranks the forced-colors rule

`sdks/ui/src/blocks/Tag/Tag.css` (~L158 + the `@media (forced-colors: active)`
block).

The normal-mode filter hover selectors (`.zs-tag--filter:hover:not([data-selected])`
and `.zs-tag--filter[data-selected]:hover`) have higher specificity than the
forced-colors selected/idle rules, so under forced-colors a hovered tag paints
`--zs-accent-hover` / the OKLCH mix instead of `Highlight` / `Canvas`.

**Fix:** in the `@media (forced-colors: active)` block, add equal-or-higher-
specificity mirrors for the hover states — `.zs-tag--filter:hover:not([data-selected])`
→ `Canvas`/`CanvasText` border, and `.zs-tag--filter[data-selected]:hover` →
`Highlight`/`HighlightText` — with `forced-color-adjust: none`. Mirror the
Button.css wave-8 forced-colors specificity-mirror pattern (see Button.css
"Specificity mirror" comment). **Regression guard:** forced-colors isn't
emulated in the test-storybook gate (same as reduced-motion), so the
orchestrator will static-verify these hover mirror selectors exist in the
forced-colors block; the existing Tag suite must stay green.

## 🟡 2. Badge success/warning deepening rotates hue (oklch → oklab)

`sdks/ui/src/blocks/Badge/Badge.css` (~L95, the success/warning solid-fill
deepening mixes).

The deepening uses polar `oklch` `color-mix`, which interpolates hue and
rotates success toward teal and warning toward magenta/red against the neutral
token. Contrast stays AA but the semantic color drifts off green/orange.

**Fix:** switch those intent→neutral deepening mixes to
`color-mix(in oklab, …)` (rectangular — no hue rotation), keeping the same
percentages. Re-run the Badge matrix suite (axe) afterward to confirm contrast
still passes. Apply to success AND warning solid fills (and any soft/outline ink
that used the same polar mix). Leave info/danger as-is if already non-drifting.

## 🟡 3. Tag filter mode is controlled-only — guard the uncontrolled mistake

`sdks/ui/src/blocks/Tag/Tag.tsx` (filter-mode detection + `onSelectedChange`
JSDoc).

A consumer wiring only `onSelectedChange` (no `selected`) gets a chip that
fires `onSelectedChange(true)` forever and never visually presses
(`aria-pressed` stuck false). Keep filter mode CONTROLLED-ONLY (don't add
internal state), but: (a) add a `process.env.NODE_ENV!=="production"` dev-warn
when `onSelectedChange` is provided without a boolean `selected`; and (b) state
"filter mode is controlled — pass `selected` and update it in `onSelectedChange`"
in the `onSelectedChange` / `selected` JSDoc.

## 🟢 4. Remove the dead Backspace/Delete span handler

`sdks/ui/src/blocks/Tag/Tag.tsx` (~L253-262 + the keydown JSDoc).

The root `<span>` has no `tabIndex` (correct) so it can't focus, and the remove
`<button>`'s own keydown `stopPropagation()`s — the span-level Backspace/Delete
branch is unreachable. Remove the dead span `onKeyDown` branch and trim the
JSDoc claim to "while the remove button has focus" (the live path).

## Won't-fix (note only)

- claude 🟢 `data-testid` vs CONVENTIONS: the slice briefs mandate `data-testid`
  on every variant; other landed slices follow it; the `play()`s use `getByRole`.
  Harmless, consistent with the rest of the package — leave.
- claude 🟢 `TagProps extends span` while filter mode renders `<button>`:
  accepted per the brief's single-interface design (shared attrs overlap). Leave.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' blocks/Badge blocks/Tag   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' blocks/Badge blocks/Tag                # 0
grep -n 'forced-colors' blocks/Tag/Tag.css   # confirm hover mirrors added
grep -n 'oklab' blocks/Badge/Badge.css       # confirm rectangular mix
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6167 --silent &) ; sleep 3
for s in Badge Tag; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6167 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: the Tag forced-colors hover-mirror diff, the Badge oklab change, the Tag
filter dev-warn + JSDoc, the removed dead branch, build results, grep counts,
and the 2 suites' pass counts (both must stay green; Badge matrix axe must still
pass after the oklab change).
