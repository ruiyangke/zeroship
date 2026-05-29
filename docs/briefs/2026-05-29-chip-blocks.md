# Slice brief: Chip blocks (Wave 2b) — Badge, Tag

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`.
**Plan:** Tasks 13–14. **Spec:** §4c.

Two small token-chip blocks: `Badge` (promotes the placeholder to a real
component) and `Tag` (interactive removable / filter chip; generalizes the
builder's `Pill`/`FilterPill`). Live in `sdks/ui/src/blocks/<Name>/`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns

- `sdks/ui/src/components/Button/Button.{tsx,css}` — intent/variant token
  mapping, focus-visible ring, the glass opaque-base rule.
- `sdks/ui/src/components/Card/Card.tsx` — forwardRef/asChild/Slot/data-slot,
  dev-warn, JSDoc header.
- `sdks/ui/src/components/Toggle/Toggle.tsx` — `aria-pressed` toggle pattern
  (for Tag's filter mode).
- `_slot.ts`, `_classnames.ts`.

## Token facts (live crystal set)

Intent → color: `neutral` → label/fill family (`--zs-label`, `--zs-fill-*`);
`info` → `--zs-accent`; `success` → `--zs-system-green`; `warning` →
`--zs-system-orange`; `danger` → `--zs-system-red`. Pill radius
`--zs-radius-full`. Type: `--zs-text-caption-1-*` / `--zs-text-footnote-*` for
the small chip text. **Glass rule:** every variant paints an OPAQUE background
base (even "soft") so axe's contrast walk passes — never rely on alpha alone
over the page.

---

## 1. Badge (`src/blocks/Badge/`) — promote placeholder → real

```ts
export type BadgeIntent = "neutral" | "info" | "success" | "warning" | "danger";
export type BadgeVariant = "solid" | "soft" | "outline";
export type BadgeSize = "sm" | "md";
interface BadgeProps extends ComponentPropsWithoutRef<"span"> {
  intent?: BadgeIntent;   // default "neutral"
  variant?: BadgeVariant; // default "soft"
  size?: BadgeSize;       // default "md"
  asChild?: boolean;
}
```
- Variants per intent: `solid` = filled intent bg + contrasting ink (use the
  intent's on-color / `--zs-accent-ink` for info, white-equivalent label token
  for system colors — verify contrast); `soft` = opaque tinted bg (intent at
  low prominence over an opaque base) + intent-toned ink; `outline` =
  transparent-feel but OPAQUE base + intent border (`--zs-border-*` width) +
  intent ink. Pill radius. `sm`/`md` differ in padding + text token.
- `forwardRef`, `data-slot="badge"`, `asChild` via `Slot` + dev-warn, per-prop
  JSDoc.
- **Replace the placeholder:** delete `Badge`/`BadgeProps` from
  `src/placeholders.tsx` (Badge is its ONLY export → delete the whole file),
  remove the placeholder export block at the bottom of `src/index.ts`, and
  export the real `Badge` (+ `BadgeProps`/`BadgeIntent`/`BadgeVariant`/
  `BadgeSize`) from `src/blocks/index.ts`. No `@deprecated` alias (pre-launch).
- **Migrate the one consumer (no-back-compat = update callers in-patch):**
  `apps/zeroship-builder/src/client/workspace/canvases/SettingsCanvas.tsx:106`
  renders `<Badge tone="info">current</Badge>`. Change `tone="info"` →
  `intent="info"`. Verify the builder still typechecks/builds (see gates).
  (The `Badge` mention in `apps/zeroship-builder/src/server/internal/prompts.ts`
  is prose in a component-list string — leave it.)
- Stories: a matrix story (all 5 intents × 3 variants), a sizes story. Every
  combo axe-clean (contrast is the whole point). No `play()`.

## 2. Tag (`src/blocks/Tag/`) — interactive chip

```ts
export type TagSize = "sm" | "md";
interface TagProps extends ComponentPropsWithoutRef<"span"> {
  size?: TagSize;              // default "md"
  leadingIcon?: ReactNode;     // decorative, aria-hidden wrapper
  removable?: boolean;         // shows a remove (×) button
  onRemove?: () => void;
  selected?: boolean;          // filter-chip toggle state
  onSelectedChange?: (selected: boolean) => void;
}
```
**Two MUTUALLY EXCLUSIVE modes (avoid button-in-button):**
- **Removable/static** (default, or `removable`): root is a `<span>`. When
  `removable`, render a trailing real `<button type="button"
  aria-label="Remove {textLabel}">` (× glyph, `aria-hidden` on the glyph). The
  remove button fires `onRemove`. Also: when the tag (or its remove button) has
  focus, `Backspace`/`Delete` fires `onRemove`.
- **Filter toggle** (`selected` / `onSelectedChange` provided): the root itself
  is a `<button type="button" aria-pressed={selected}>` that toggles
  `onSelectedChange(!selected)`; the `selected` state paints with `--zs-accent`.
- If BOTH `removable` and a selectable prop are passed, dev-warn that they're
  mutually exclusive and prefer filter mode (or document the precedence) — do
  NOT nest a button inside a button.
- Derive the remove button's accessible label from the children text when it's
  a string; otherwise require/accept an explicit label (document).
- `forwardRef`, `data-slot="tag"` (+ `tag-remove`), per-prop JSDoc.
- Stories: default, with `leadingIcon`, **removable** (play: click × → `onRemove`
  spy fires; focus × and press `Delete` → `onRemove` fires), **filter** (play:
  click → `aria-pressed` flips + `onSelectedChange` fires). axe each.

---

## Wiring & constraints

- `@import` both CSS into `styles.css` (extend the "Composed blocks" group).
- Export both from `src/blocks/index.ts`.
- `--zs-*` only, no raw hex/px; logical properties; `prefers-reduced-motion` +
  `forced-colors` where relevant (chips have hover/selected transitions —
  honor reduced-motion; forced-colors must keep borders/selected state
  visible). No "HIG"/"Apple". Pre-launch no-back-compat.

## Verification (run and REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
pnpm --filter zeroship-builder build   # MUST stay green after the Badge API migration
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/Badge blocks/Tag   # 0
grep -rn '[0-9]\+px'             --include='*.css' --include='*.tsx' --include='*.ts' blocks/Badge blocks/Tag   # 0
# suites on a FREE port:
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6161 --silent &) ; sleep 3
for s in Badge Tag; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6161 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: per-block files, the intent×variant token mapping, confirmation the
placeholder is deleted + `src/index.ts` no longer exports it + SettingsCanvas
migrated + `zeroship-builder` build green, build results, grep counts, the 2
suites' pass counts, and any decision the brief didn't cover (esp. the
removable-vs-selectable resolution + remove-button labeling).
