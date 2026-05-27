# Slice 1 Button — review fixes (no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `f2728d3f`).
**Status when this brief is written:** Button + Crystal landed in slice 1.
This brief lands every issue surfaced by two independent review passes
(pilot self-review + codex second pass) plus a Storybook a11y-panel
contrast finding. Nothing is deferred per the user's directive.

## Goal
Land one focused commit that resolves all 26 items below. Builds + a11y
verifier remain green; Storybook a11y panel's "could not determine" /
"incomplete" entries on Button labels and story labels clear.

## Hard constraints (unchanged)
- Worktree single-writer.
- Pre-launch, no back-compat — rename `role` → `intent` cleanly, no aliases.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex / no raw px (blur radii too).
- HIG is the design principle; only palette/material varies per theme.
- `prefers-reduced-motion` and `forced-colors: active` are first-class.
- DO NOT commit, push, or merge. Pilot reviews and commits.

## Fix list — every item must land

### 🔴 Real bugs (9)

**1. Drop the dead `effectiveRole` computation.** `Button.tsx:90–94`.
The conditional returns `role` in both branches. Replace usage with `role`
directly; delete the variable; delete the misleading "We enforce by
ignoring primary if destructive is set" comment above it.

**2. Disabled buttons must not stay tappable on touch.** `Button.css:42–52`.
The hit-area `::before` re-enables `pointer-events: auto` even when the
button itself is `disabled`. Scope it:
```css
@media (pointer: coarse) {
  .zs-button:not(:disabled):not([aria-busy="true"])::before { ... }
}
```

**3. `{...rest}` spread must come FIRST.** `Button.tsx:107–118`.
Currently it's last and can clobber our internal `aria-busy`,
`data-variant`, `data-intent`, etc. Spread first; our controlled props
override anything in `rest`.

**4. Destructive-plain disabled stays red — cascade bug.** `Button.css`.
The disabled rules for `.zs-button--plain` run before the
`.zs-button--destructive.zs-button--plain` rules but the destructive
variant has no `:disabled` / `[aria-busy="true"]` override of its own, so
its red text persists even on a disabled state where contrast is
inappropriate. Add explicit disabled/busy rules for
`.zs-button--destructive.zs-button--plain:disabled` and
`.zs-button--destructive.zs-button--plain[aria-busy="true"]`.

**5. `gray + destructive` is invisibly neutral.** `Button.css:223–225`.
The JSDoc on `intent="destructive"` says it OVERRIDES the accent palette
with system-red regardless of variant — but gray+destructive falls
through to plain gray, with no destructive signal. Resolve by giving
gray+destructive a system-red LABEL while keeping the neutral gray fill
(communicates destructive intent without the loud red bg). Apply to all
gray+destructive states (default, hover, active, disabled, busy).

**6. Loading uses `color: transparent` only.** `Button.css:233–240`.
Slot images, SVGs, or children with explicit `color` styles bleed
through behind the spinner. Switch to `opacity: 0` on the label AND the
slot spans. Keep accessibility intact: the label remains in the DOM (so
screen readers still announce the name); `opacity: 0` is OK at the AT
tree level (still focusable / queryable). Pair with
`pointer-events: none` on the now-invisible label/slots so they don't
catch clicks during loading (the button is `aria-busy` + `disabled`
already, so this is belt-and-braces).

**7. Hit-area extends BOTH axes.** `Button.css:42–52`.
Small buttons (32px high) sometimes render <44pt wide for icon-only
content. The `::before` must extend in both directions:
```css
@media (pointer: coarse) {
  .zs-button:not(:disabled):not([aria-busy="true"])::before {
    content: "";
    position: absolute;
    top: 50%; left: 50%;
    transform: translate(-50%, -50%);
    inline-size: max(100%, var(--zs-hit-min));
    block-size: max(100%, var(--zs-hit-min));
    pointer-events: auto;
  }
}
```

**8. `prefers-reduced-motion` must STOP the spinner.** `Button.css:268–272`.
Setting `animation-duration: 0.01ms` still runs the keyframe rapidly.
Use `animation: none` and leave the partial-circle stroke static.

**9. Translucent surfaces need an opaque base so axe can walk the stack.**
This is the foundation pattern for every glass surface (slice 1: only
`.zs-story-row`; future Card/Dialog/Popover follow the same shape).

The bug today: `.zs-story-row` has `background: var(--zs-surface-raised)`
(translucent) over `.zs-story-main` (gradient mesh). Axe walks up
looking for a computable background, hits the gradient, gives up,
reports "Element's background color could not be determined due to a
background gradient" (impact: serious). This affects `.zs-story-label`
in every cell and `.zs-button--tinted/gray/plain` labels.

**Fix pattern** (use everywhere translucency is needed):
```css
.zs-story-row {
  position: relative;
  background-color: var(--zs-surface);    /* opaque — axe stops here */
  border-radius: var(--zs-radius-6);
  /* glass visual via overlay so axe doesn't see translucency/gradient on
     the element itself */
}
.zs-story-row::before {
  content: "";
  position: absolute;
  inset: 0;
  background: var(--zs-surface-raised);
  backdrop-filter: var(--zs-blur);
  -webkit-backdrop-filter: var(--zs-blur);
  border-radius: inherit;
  pointer-events: none;
  z-index: 0;
}
.zs-story-row > * { position: relative; z-index: 1; }
```

Apply this to `.zs-story-row` and `.zs-story-grid` in story.css. Document
in a comment block at the top of story.css ("Glass-surface pattern: opaque
base + ::before overlay; required for axe contrast walk").

After this fix, re-run a temp axe scan with both `violations` AND
`incomplete` lists from `@axe-core/playwright` and confirm no
color-contrast incomplete entries on Button labels or story labels.
(Write a `scripts/axe-incomplete-scan.mjs` if helpful — delete before
returning if so.)

### 🟡 Surface / API (9)

**10. Rename `role` prop → `intent`.** The DS prop shadows the standard
ARIA `role` attribute on every focusable element; this is a real
footgun. Rename in `Button.tsx`, the exported type
(`ButtonRole` → `ButtonIntent`), JSDoc, `data-role` →
`data-intent`. Update consumers:
- `Button.stories.tsx` — all `role="..."` examples become `intent="..."`.
- `apps/zeroship-builder/src/client/workspace/canvases/SettingsCanvas.tsx`
  — two call sites use `role="destructive"`; rename to
  `intent="destructive"`. The `role="cancel"` site loses `role` (see #11).
- `sdks/ui/src/index.ts` — re-export the renamed type.

**11. Narrow `ButtonIntent` to `"normal" | "destructive"`.** `primary` and
`cancel` had no behavior beyond emitting `data-role=`. Drop them. When
Dialog/Form land in a later slice with real semantics for these intents,
they can be re-introduced with real behavior (e.g., Enter-key binding
for primary, Escape for cancel). Update the SettingsCanvas
`role="cancel"` site to drop the prop entirely.

**12. Gate `:hover` rules with `@media (hover: hover)`.** `Button.css`,
every `:hover` selector. iOS Safari's sticky-hover-after-tap makes
tinted/gray/plain hover backgrounds persist visually after touch.
Wrap each hover block:
```css
@media (hover: hover) {
  .zs-button--filled:hover:not(:disabled):not([aria-busy="true"]) { ... }
  ...
}
```

**13. Dev-mode accessible-name warning.** `Button.tsx`.
In `process.env.NODE_ENV !== "production"`, warn once per mount if a
Button has no visible label AND no `aria-label` AND no `aria-labelledby`.
Use `useEffect` with the ref to inspect `element.textContent` and
attributes; `console.warn(...)` with a clear message + component
displayName. Don't throw.

**14. Add `@media (forced-colors: active)` rules.** `Button.css`.
Windows High Contrast Mode (and the macOS forced-colors media feature)
overrides colors with system tokens. We must defer to those tokens
explicitly so our buttons remain identifiable:
```css
@media (forced-colors: active) {
  .zs-button {
    border: 0.0625rem solid CanvasText;
    forced-color-adjust: none;
  }
  .zs-button--filled { background: Highlight; color: HighlightText; }
  .zs-button--tinted,
  .zs-button--gray   { background: Canvas; color: CanvasText; }
  .zs-button--plain  { background: transparent; color: LinkText; }
  .zs-button:disabled,
  .zs-button[aria-busy="true"] { color: GrayText; border-color: GrayText; }
  .zs-button:focus-visible { outline: 0.125rem solid Highlight; outline-offset: 0.125rem; }
}
```
Note: `border-width` here is 1px equivalent expressed in rem (`0.0625rem`)
so the no-raw-px rule is honored.

**15. Resolve `cursor: not-allowed` + `pointer-events: none` paradox.**
`Button.css:75–79`. Drop `pointer-events: none` — the HTML `disabled`
attribute on `<button>` already prevents activation; `pointer-events:
none` only hides the cursor (defeating `cursor: not-allowed`) and lets
clicks fall through to elements behind. Keep `cursor: not-allowed`.

**16. Add overflow policy for long labels.** `Button.css`.
`white-space: nowrap` with no max + no truncation = labels can overflow
their flex parent. Add:
```css
.zs-button__label {
  min-width: 0;                  /* allow flex shrink */
  overflow: hidden;
  text-overflow: ellipsis;
}
```
The `.zs-button` itself should NOT have `max-width` — consumers decide
constraints. Add a story with a long label to verify ellipsis behaves.

**17. Reword `data-role` JSDoc.** Original claimed AT support;
assistive tech ignores custom data attributes. After the rename it's
`data-intent`. Replace with: "Emitted as a styling/testing hook; not
consumed by assistive tech."

**18. Add `asChild` polymorphism.** `Button.tsx`.
HIG buttons sometimes render as anchors ("Learn more", "Open in
browser"). Add `asChild?: boolean`. When `asChild={true}`, render
React.cloneElement on the single child element, merging our props/className
onto it instead of wrapping in `<button>`.

Implement an INLINE `Slot` (no `@radix-ui/react-slot` dep — keep
@zeroship/ui dep surface minimal). ~40 LOC:
```tsx
function Slot({ children, ...props }: SlotProps) {
  if (!isValidElement(children)) return null;
  return cloneElement(children, mergeProps(props, children.props));
}
```
With `mergeProps` composing className, style, ref, and event handlers
(call children's handler first; ours after).

When `asChild={true}` is set:
- The Button wrapper becomes a Slot.
- `type="button"` default is dropped (anchors don't take `type`).
- Click activation rules differ for anchors (Enter/click only, no Space)
  — note in JSDoc that consumers using asChild with non-button targets
  inherit native semantics.
- Loading + disabled state: still set `aria-disabled` / `aria-busy` on
  the slotted element; do NOT set HTML `disabled` (anchors don't have
  it). Buttons still receive `disabled`.

Add a story `AsChild` showing `<Button asChild><a href="#">Open</a></Button>`
rendering as an anchor with Button styling.

### 🟢 Nits / dead code (8)

**19. Drop `text-shadow: none`.** `Button.css:235`. No upstream rule applies
text-shadow.

**20. Drop letter-spacing micro values.** `Button.css:92, 102, 112`.
`-0.002em` / `-0.004em` are subliminal at body sizes; remove.

**21. Update JSDocs.** Button.tsx `loading` JSDoc (line 40) currently says
"hidden via `visibility` to preserve width" — update to "hidden via
`opacity: 0` to preserve width and the accessible name". Button.css
anatomy header (line ~6) same fix.

**22. `startSlot ?` / `endSlot ?` should use `!= null`.** `Button.tsx:119, 121`.
Falsy values like `0` or `false` are valid React nodes that the current
ternary drops. Use:
```tsx
{startSlot != null ? <span className="zs-button__start">{startSlot}</span> : null}
```

**23. Drop `role="presentation"` from spinner SVG.** `Button.tsx:66`.
Redundant inside `aria-hidden="true"`. Remove. Optionally add
`focusable="false"` (defensive against IE-era SVG focus bugs).

**24. Stories matrix expansion.** `Button.stories.tsx`. Add stories that
exercise the cells current stories miss:
- `DestructiveDisabled` (4-col: filled/tinted/gray/plain × destructive × disabled)
- `LoadingDestructive` (loading + destructive)
- `WithSlots` (startSlot + endSlot using simple SVG icons)
- `LongLabel` (very long label that triggers the ellipsis policy)
- `FocusVisible` (autoFocus on first button; story description tells reviewer to tab)
- `AsChild` (anchor variant — see #18)

The existing 4 stories stay; this adds ~6 more. Update
`scripts/check-storybook-a11y.mjs` `stories` array to include the new IDs.

**25. Move active `transform: scale(0.97)` OFF the button onto an inner wrapper.**
`Button.tsx` + `Button.css`. Currently the scale also scales the
`::before` hit area (cosmetic but real). Structure:
```tsx
<button ...>
  <span className="zs-button__inner">
    {startSlot ...}
    {label ...}
    {endSlot ...}
  </span>
  {isBusy ? <Spinner /> : null}
</button>
```
And:
```css
.zs-button__inner {
  display: inline-flex;
  align-items: center;
  gap: var(--zs-space-2);
  transition: transform var(--zs-motion-fast) var(--zs-motion-spring);
}
.zs-button:active:not(:disabled):not([aria-busy="true"]) .zs-button__inner {
  transform: scale(0.97);
}
```
The `::before` lives on the button, not the inner — it doesn't scale.
The spinner sits on the button (not inner) so it also doesn't scale.

**26. Keep HTML `disabled` during loading; document the choice.**
Button.tsx + Button.css comment.
Trade-off: using HTML `disabled` is correct because the button cannot
be activated. Some screen readers stop announcing the button's state
when `disabled`, but `aria-busy="true"` is announced separately and is
the appropriate signal. Using `aria-disabled` + activation guards would
let focus persist on the button during loading — desirable for some
flows. We chose `disabled` for simplicity; revisit when a real use case
forces it. Add a short comment in Button.tsx near the
`disabled={disabled || isBusy}` line citing this decision.

## Files to modify

- `sdks/ui/src/components/Button/Button.tsx` — items 1, 3, 10, 13, 17, 18, 22, 23, 25, 26
- `sdks/ui/src/components/Button/Button.css` — items 2, 4, 5, 6, 7, 8, 12, 14, 15, 16, 19, 20, 21, 25
- `sdks/ui/src/components/Button/index.ts` — re-export rename for #10
- `sdks/ui/src/components/index.ts` — re-export rename for #10
- `sdks/ui/src/index.ts` — re-export rename for #10
- `sdks/ui/src/stories/Button.stories.tsx` — items 10 (rename props), 11 (drop cancel example), 24 (add stories)
- `sdks/ui/src/stories/story.css` — item 9 (glass-surface pattern on row/grid)
- `sdks/ui/scripts/check-storybook-a11y.mjs` — add the new story IDs from #24
- `apps/zeroship-builder/src/client/workspace/canvases/SettingsCanvas.tsx` — items 10 + 11 (rename + drop cancel role)

## Verification (run all, paste results in your report)

1. `pnpm --filter @zeroship/ui build` → green (ESM + DTS).
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity:
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty
     (note: `oklch(0 0 0)`, `0.0625rem`, etc. are fine — only the `#xxxxxx`
     literal hex pattern is the bar)
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty
4. `pnpm --filter zeroship-builder build` → green (intent rename
   propagated to SettingsCanvas).
5. A11y (violations): serve `storybook-static` on a free port, run
   `STORYBOOK_URL=... node scripts/check-storybook-a11y.mjs`. Must report
   `A11y clean for N stories across 1 themes (no serious/critical violations)`
   where N = 4 existing + 6 new = 10.
6. **A11y (incomplete)**: write a temp scanner `scripts/axe-incomplete-scan.mjs`
   that prints both `violations` AND `incomplete` arrays. Run it; the
   "Element's background color could not be determined due to a background
   gradient" entries on `.zs-story-label` / `.zs-button--*` labels MUST be
   gone. Delete the temp scanner before returning.
7. Capture screenshots — run the existing
   `scripts/capture-button-evidence.mjs`. PNGs land at
   `storybook-static/theme-evidence/`. Verify all 4 (or 10 if you extend
   the harness) appear.

## Report (stdout)

- Files changed/created — full paths.
- For each of items 1–26, one line confirming done with file:line ref.
- Token-purity grep results.
- Build status (both packages).
- A11y violations result line.
- A11y incomplete result — confirm the background-gradient entries are gone.
- Screenshot paths.
- One taste note (if any).
- Explicitly: "I did NOT commit, push, or merge."
