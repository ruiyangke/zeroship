# Slice brief: Content blocks (Wave 2c) — StatCard, Banner, DescriptionList

**Worktree:** `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
**Branch:** `builder/ui-layouts-blocks`. **Plan:** Tasks 15–17. **Spec:** §4c.

Three composed blocks: `StatCard` (KPI/metric card, built ON `Card`),
`Banner` (inline page-level message), `DescriptionList` (key→value rows).
Live in `sdks/ui/src/blocks/<Name>/`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference patterns

- `sdks/ui/src/components/Card/Card.tsx` — StatCard COMPOSES Card (import
  `Card` and its parts; don't re-roll the surface).
- `sdks/ui/src/blocks/ErrorState/ErrorState.tsx` — the `live`-gated role
  pattern + intent icon tinting + compound parts; mirror it for Banner.
- `sdks/ui/src/blocks/EmptyState/EmptyState.tsx` — dual ergonomic+compound
  surface, dev-warn, Center/Stack composition.
- `_slot.ts`, `_classnames.ts`. Layout primitives `Stack`/`Cluster` to compose.

## Token facts (live crystal set)

Intent → color: `info`→`--zs-accent`, `success`→`--zs-system-green`,
`warning`→`--zs-system-orange`, `danger`→`--zs-system-red`. Delta direction →
color: `up`→`--zs-system-green`, `down`→`--zs-system-red`, `flat`→
`--zs-label-secondary`. Value type: `--zs-text-title-1-*` / `--zs-text-title-2-*`.
Labels: `--zs-label` / `--zs-label-secondary` / `--zs-label-tertiary`. Divider:
`--zs-separator`. Glass rule: opaque background base on any tinted surface.

---

## 1. StatCard (`src/blocks/StatCard/`) — built on Card

```ts
export type StatCardDeltaDirection = "up" | "down" | "flat";
interface StatCardDelta { value: string; direction: StatCardDeltaDirection; }
interface StatCardProps extends ComponentPropsWithoutRef<"div"> {
  label: ReactNode;       // the metric name
  value: ReactNode;       // the metric value (large)
  delta?: StatCardDelta;  // optional change indicator
  icon?: ReactNode;       // optional decorative icon (aria-hidden)
  // pass-through to the underlying Card (variant/size) as sensible defaults
}
```
- Render a `Card` (surface/elevated as default) containing: label (small,
  `--zs-label-secondary`), value (large title token), and the delta row.
- **Delta a11y — direction MUST be conveyed by more than color:** render an
  `aria-hidden` directional glyph (▲/▼/—) tinted by direction AND make the
  delta text itself state the direction (e.g. visually-hidden "increased"/"
  decreased" prefix, or include it in the visible string). Color alone fails
  WCAG 1.4.1. `flat` is neutral.
- label+value read as a coherent unit (the value is not a heading unless the
  consumer wants — keep it a styled `<div>`/`<p>`; document).
- `forwardRef`, `data-slot="stat-card"`, per-prop JSDoc.
- Stories: up / down / flat, with icon, axe (contrast on delta colors). No play.

## 2. Banner (`src/blocks/Banner/`) — inline message (generalizes LiveBanner)

```ts
export type BannerIntent = "info" | "success" | "warning" | "danger";
interface BannerProps extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  intent?: BannerIntent;     // default "info"
  dismissible?: boolean;
  onDismiss?: () => void;
  live?: boolean;            // default false
  title?: ReactNode;
  description?: ReactNode;
  asChild?: boolean;
}
// Compound: Banner.Title / .Description / .Actions
```
- Horizontal layout: leading intent icon (aria-hidden) + a text column
  (title/description) + optional `.Actions` + optional dismiss button. Opaque
  tinted background per intent (glass rule). Use `Stack`/`Cluster` to compose.
- **a11y (mirror ErrorState):** `role` ONLY when `live` — `status` for
  info/success, `alert` for warning/danger; default a plain region (a
  statically-rendered banner must not assert a live region). Dismiss is a real
  `<button aria-label="Dismiss">` (× glyph aria-hidden). Two-mode title docs
  like EmptyState/ErrorState (ergonomic props OR compound parts; no false
  "child suppresses prop" claim).
- `forwardRef`, `data-slot="banner"` (+ `banner-dismiss`), per-prop JSDoc,
  asChild via Slot + dev-warn.
- Stories: each intent, dismissible (**play:** click dismiss → `onDismiss` spy
  fires), with actions, a `live` story. axe each.

## 3. DescriptionList (`src/blocks/DescriptionList/`) — key→value

```ts
interface DescriptionListProps extends ComponentPropsWithoutRef<"dl"> {
  orientation?: "horizontal" | "vertical";  // default "horizontal"
  divider?: boolean;                          // hairline between items
}
// Compound: DescriptionList.Item / .Term / .Detail
```
- Semantic `<dl>` root; `.Item` is a `<div>` (valid grouping inside `<dl>`)
  wrapping a `.Term` (`<dt>`) + `.Detail` (`<dd>`). `horizontal` lays term/detail
  in a row (term fixed-ish width, detail fills); `vertical` stacks them.
  `divider` draws a `--zs-separator` hairline between items (logical border).
- `forwardRef` on root + parts, `data-slot="description-list"` (+ `-item`/
  `-term`/`-detail`), per-prop JSDoc.
- Stories: horizontal, vertical, with divider; assert `dl`/`dt`/`dd` structure
  via a `play()` (getByRole or tagName checks). axe.

---

## Wiring & constraints

- `@import` the three CSS into `styles.css` ("Composed blocks" group); export
  all three (+ types/parts) from `src/blocks/index.ts`.
- `--zs-*` only; no raw hex/px; logical properties; `prefers-reduced-motion` +
  `forced-colors` where relevant (Banner dismiss hover, StatCard). No
  "HIG"/"Apple". Pre-launch no-back-compat. asChild via Slot + dev-warn.

## Verification (run, REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/StatCard blocks/Banner blocks/DescriptionList   # 0
grep -rn '[0-9]\+px'             --include='*.css' --include='*.tsx' --include='*.ts' blocks/StatCard blocks/Banner blocks/DescriptionList   # 0
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6171 --silent &) ; sleep 3
for s in StatCard Banner DescriptionList; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6171 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: per-block files, intent/delta→token mapping, build results, grep
counts, the 3 suites' pass counts, and any decision the brief didn't cover
(esp. StatCard delta a11y phrasing + DescriptionList horizontal layout method).
