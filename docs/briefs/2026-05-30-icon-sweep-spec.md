# Icon sweep spec — replace inline-SVG glyphs with Icon (Lucide) everywhere

Shared map for the library-wide migration of hand-rolled inline `<svg>` glyphs to
the governed `Icon` (Lucide) primitive. lucide-react@1.14.0 (modern name set; all
names below CONFIRMED present). DataTable already done (commit 3470c511).

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> Per-agent rule for this sweep: **EDIT ONLY — do NOT run `pnpm build`,
> `build-storybook`, or `test-storybook`** (concurrent builds clobber the shared
> storybook-static). Make the source + CSS edits, re-read your own files to
> self-check, REPORT. The orchestrator runs one central build + suite + visual.
> DO NOT commit, push, or merge.

## House idiom (from components/Icon/Icon.tsx + FilterBar/Stepper/PricingTable)
- `import { Check } from "lucide-react";` then `<Icon as={Check} size="sm" />`.
- `size`: `sm`(1rem) / `md`(1.25rem, default) / `lg`(1.5rem) — CSS via `zs-icon--{size}`,
  NEVER a numeric prop.
- Color inherits via `currentColor` from the parent `color`. Drop per-svg stroke/fill.
- Decorative by default (no `label` → `aria-hidden`). **Every glyph below is
  decorative** (sits beside text or inside a button that already carries the
  `aria-label`). Do NOT pass `label`.
- To keep a component class (rotation/positioning), pass `className="…"` to `<Icon>`
  (composes onto `zs-icon`). `<Icon>` spreads `...rest`, so `data-*` pass through.

## KEEP — do NOT migrate (structural / animation)
- **Arrow pointers** (Base UI `Arrow` slot — positioning chrome, the triangle that
  points at the trigger): Popover.tsx:314 · Tooltip.tsx:342 · Menu.tsx:260 ·
  NavigationMenu.tsx:561 · PreviewCard.tsx:672. Leave inline.
- **Button loading spinner** (Button.tsx:101 `Spinner`) — animation-coupled CSS;
  SEPARATE task, not a glyph swap. Leave inline.
- **NumberField scrub-area cursor** (NumberField.tsx:284 — bespoke 26×14 double-headed
  resize arrows; no clean confirmed Lucide match). Leave inline.

## MIGRATE — decision table (file:line · helper · → Lucide · notes)

### Group A — close-X (lowest risk; identical glyph → `X`)
- Dialog.tsx:421 `CloseGlyph` → **X** (button has `aria-label={closeLabel}`)
- Drawer.tsx:263 `CloseGlyph` → **X**
- Toast.tsx:593 `CloseGlyph` → **X** (button `aria-label="Dismiss notification"`)
- Combobox.tsx:402 `ChipRemoveGlyph` → **X** (chip remove; button labels it)

### Group B — disclosure chevron (`ChevronDown`; PRESERVE rotation CSS)
- Accordion.tsx:486 `ChevronGlyph` → **ChevronDown**, pass
  `className="zs-accordion-trigger-chevron"` (the rotate rule keys off it).
- Collapsible.tsx:309 `CollapsibleChevron` → **ChevronDown**, pass
  `className="zs-collapsible-trigger-chevron"`.
- NavigationMenu.tsx:605 `ChevronGlyph` → **ChevronDown** (chevron only; LEAVE the
  Arrow at :561). Rotation CSS is on the parent `zs-navmenu-icon` span — keep it.
  ⚠️ NavigationMenu has a PRE-EXISTING red story baseline unrelated to this — diff
  against that baseline, don't blame the swap.

### Group C — select / list family
- Select.tsx:388 `ChevronDownGlyph` (trigger) → **ChevronDown**
- Select.tsx:440 `CheckGlyph` (selected) → **Check**
- Autocomplete.tsx:256 `ChevronDownGlyph` → **ChevronDown**
- Combobox.tsx:370 `ChevronDownGlyph` (trigger) → **ChevronDown**
- Combobox.tsx:385 `CheckGlyph` (selected) → **Check**
- Menu.tsx:448 `CheckGlyph` (checkbox item) → **Check**
- Menu.tsx:534 `RadioDotGlyph` (radio dot) → **Circle** ⚠️ Lucide `Circle` is a
  HOLLOW stroked ring vs the current FILLED dot. If a filled dot is required for the
  radio semantics, KEEP this one inline (flag it) rather than ship a hollow ring.
  Default: keep inline unless a filled treatment via CSS (`fill: currentColor` on the
  Icon svg) reproduces the dot — migrator's judgment; flag for visual review.
- Menu.tsx:737 `ChevronRightGlyph` (submenu) → **ChevronRight**
- (LEAVE Menu Arrow at :260.)

### Group D — misc (+ heaviest CSS rework)
- Checkbox.tsx:123 `IndicatorCheck` → **Check** · Checkbox.tsx:139 `IndicatorMinus`
  → **Minus**. ⚠️ HIGHEST-RISK: both glyphs render simultaneously; CSS swaps
  visibility via `data-checked`/`data-indeterminate` on the chip targeting
  `[data-glyph="check"]`/`[data-glyph="minus"]` and the inner
  `.zs-checkbox__indicator-check`/`-minus`. Lucide glyphs won't carry those inner
  classes — re-point Checkbox.css at the Icon wrappers; pass `data-glyph="check"`/
  `"minus"` through `<Icon>` (it spreads rest). Verify the check/indeterminate
  visibility toggle + any draw-on animation survive.
- NumberField.tsx:305 (Decrement) → **Minus** (button `aria-label="Decrement"`) ·
  NumberField.tsx:373 (Increment) → **Plus** (button `aria-label="Increment"`).
  (KEEP the scrub-cursor at :284.) `zs-icon--sm` (1rem) matches the current 16×16.
- Banner.tsx:228 `BannerIcon` → intent-keyed: success → **CircleCheck**, info/
  warning/danger → **CircleAlert** (or per-severity warning → **TriangleAlert**,
  danger → **CircleX** if preferred — migrator picks one cohesive mapping + notes
  it). Wrapper `zs-banner__icon` stays aria-hidden; cross-check size vs `zs-icon`.
- ErrorState.tsx:195 `ErrorStateIcon` → **CircleAlert** (universal alert glyph; it
  IS an icon). Wrapper `zs-error-state__icon` stays aria-hidden; size to match
  current footprint (likely `<Icon size="lg">` or a wrapper-sized fill).

## CSS caveats (recap)
1. Checkbox: re-point `data-glyph`/inner-class selectors at Icon wrappers (above).
2. Accordion/Collapsible/NavigationMenu: rotation keys off the existing class/parent
   span + `data-open`/`data-panel-open` — pass the same className; confirm
   transform-origin/sizing against the `zs-icon` box.
3. Banner/ErrorState: wrapper `svg{…}` descendant rules still match (Icon renders an
   svg) but cross-check the size token vs the current intent-badge size.
4. NumberField: 16×16 → `zs-icon--sm` natural match.

## Per-group constraints
`--zs-*` only; no raw hex/px (rem/%/em/oklch); logical properties; forced-colors +
reduced-motion preserved; no "HIG"/"Apple"; pre-launch no-back-compat; NO public API
change; forwardRef + data-slot unchanged. Stories are the regression net — do NOT
rewrite them; if a `play()` reached into the OLD svg internals (path/circle/rect or
`getByRole('img')`), update ONLY that query to the new Icon DOM and NOTE it (most
queries target data-slot/role and need no change). Report: each swap done, CSS
rules deleted/kept, any glyph you decided to KEEP inline (+ why), any story query
touched, and a self-read confirmation (no build — orchestrator verifies centrally).
