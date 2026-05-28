# Button visual polish (Phase 2.A of the missing-reviews plan)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `ee923f08`).

Closes the codex visual review of Button — `/tmp/codex-button-visual-review.md`. 3 🔴 + 6 selected 🟡 = 9 fix items. Pure story/foundation complaints (glass panel reads opaque, all-caps labels overpower, accent globally too saturated) deferred — those are theme-level / story-level concerns, not Button-component bugs.

## Goal

One commit closing the 3 visual bugs + 6 calibrated taste calls. Button is the foundation component every other one composes against; if it has visible drift, every other component inherits it.

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`.
- No raw hex, no raw px (incl. comments).
- HIG anchor as principle but **not in source** (per the HIG-scrub commit).
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bugs (3)

**1. Disabled destructive doesn't look disabled enough.**
`Button.css` — `.zs-button--destructive.zs-button--filled:disabled`. Codex: "Disabled destructive still communicates danger and availability instead of disabled state; the filled sample especially looks tappable." Current treatment uses `color-mix(in oklch, var(--zs-system-red) 42%, transparent)` for the disabled fill — that's still saturated red at 42%. Combined with white-on-faded-red text, it reads as "active destructive that's pending" rather than "disabled."

Two-part fix:
- Drop the fill's saturation further AND mix toward `--zs-fill-secondary` (a neutral) instead of toward transparent. Result: disabled destructive shifts from red-but-faded → gray-with-a-red-hint.
- Apply `filter: grayscale(0.4)` (or equivalent via color-mix in CSS) on the disabled state so the whole control reads less alive.

Concrete CSS:
```css
.zs-button--destructive.zs-button--filled:disabled,
.zs-button--destructive.zs-button--filled[aria-busy="true"] {
  background: color-mix(in oklch, var(--zs-system-red) 22%, var(--zs-fill-secondary));
  color: color-mix(in oklch, var(--zs-accent-ink) 65%, var(--zs-label-tertiary));
}
```
Same pattern for `--tinted` (lower the alpha further) and `--gray` and `--plain` destructive-disabled.

Visual proof: capture `destructive-disabled` and verify the buttons no longer read as "click me, I'm warning."

**2. Loading destructive buttons change width/weight vs idle.**
`Button.css` — `.zs-button[aria-busy="true"]`. Codex: "Loading destructive buttons appear wider and heavier than their matching idle destructive buttons." Likely cause: when the label opacity drops to 0 but the spinner is inserted as an absolutely-positioned element, the button's min-width relies on the label's measured width; but the label is being measured at zero alpha — possible browser quirk where `visibility: hidden`-like states drop the layout box.

Investigate + fix:
- Compare `destructive` and `loading-destructive` story screenshots side-by-side. Measure pixel widths. If they differ, the layout is shifting.
- Likely fix: ensure the label container preserves its width via `min-width` set on the inner `.zs-button__inner` OR explicitly `width: max-content` when `aria-busy`.

Concrete CSS to investigate:
```css
.zs-button[aria-busy="true"] .zs-button__inner {
  min-width: 0; /* allow shrink */
  visibility: hidden; /* preserves layout box but invisible — already used */
  /* ensure the parent button keeps its natural width */
}
.zs-button[aria-busy="true"] {
  inline-size: max-content; /* or pin via min-inline-size */
}
```
Verify by capturing both stories at same scale; widths must match.

**3. Focus-visible ring too chunky.**
`Button.css` — `.zs-button:focus-visible`. Codex: "Focus ring is highly visible and correctly blue, but the double edge/halo is a little heavy compared with Apple's quieter field focus treatment." Currently the ring is `0.1875rem` (3 device units) at `--zs-focus-ring-color` alpha 0.35.

Already done for Input (alpha 0.25). Apply matching restraint to Button:
- Drop alpha to 0.28 (between Input's 0.25 and the original 0.35 — Button's primary-action role earns a slightly louder ring than form-field focus, but not the current chunkiness).
- Keep width at 3 device units but consider 2 (0.125rem) if 3 still feels overbuilt. Codex's visual concern suggests trying 0.125rem.

Concrete:
```css
:root { --zs-button-focus-ring-color: color-mix(in oklch, var(--zs-accent) 28%, transparent); }
.zs-button:focus-visible {
  outline: 0.125rem solid var(--zs-button-focus-ring-color);
  outline-offset: 0.0625rem;
}
```

### 🟡 Selected taste calls (6)

**4. Tinted variants too candy / dusty.**
`styles.css` crystal palette — `--zs-input-bg-filled` analog for Button tinted. The tinted color-mix recipe uses `color-mix(in oklch, var(--zs-accent) 18%, transparent)`. 18% over white reads candy-bright on normal accent; over red on destructive reads dusty. Subtle calibration:
- Drop the tinted alpha to 14% so the wash is gentler.
- For destructive-tinted: keep alpha at 18% but mix with `--zs-system-red-active` (darker) instead of `--zs-system-red` so the result is muted-red, not dusty-pink.

**5. Gray variant unrefined.**
`Button.css` — `.zs-button--gray`. The gray uses `--zs-fill-secondary` (≈ 13% label-alpha) as bg. Codex: "Gray variant lacks refinement; it reads as disabled-adjacent in some stories." Fix: use a slightly different fill that's more clearly "neutral action," not "disabled state":
```css
.zs-button--gray {
  background: color-mix(in oklch, var(--zs-label) 8%, transparent);
  color: var(--zs-label);
}
```
Lower opacity + label-color base reads as "neutral grey button" instead of "disabled state."

**6. Plain in groups needs stronger anchoring.**
`Button.css` — `.zs-button--plain`. Codex: "Plain buttons need stronger optical anchoring in grouped layouts." When a plain Button sits next to a filled or tinted Button in a Footer or row, it floats. Fix via hover-only baseline: add a subtle `text-decoration: underline; text-decoration-color: color-mix(in oklch, var(--zs-accent) 20%, transparent); text-decoration-thickness: 0.0625rem; text-underline-offset: 0.125rem;` on hover only — anchors the action without making the rest state busy.

```css
@media (hover: hover) {
  .zs-button--plain:not(:disabled):not([aria-busy="true"]):hover {
    text-decoration: underline;
    text-decoration-color: color-mix(in oklch, currentColor 35%, transparent);
    text-decoration-thickness: 0.0625rem;
    text-underline-offset: 0.125rem;
  }
}
```

**7. Destructive red lacks subtlety.**
`styles.css` crystal palette — `--zs-system-red` is at oklch L=0.52. Codex: "filled destructive has adequate punch, but the red is blunt and web-like." Slightly bump chroma down (currently 0.2) to feel more refined:
```css
--zs-system-red: oklch(0.52 0.17 25);  /* was 0.2 */
--zs-system-red-hover: oklch(0.47 0.18 25);
--zs-system-red-active: oklch(0.43 0.19 25);
```
Re-run a11y; if filled destructive contrast drops below AA on white text, deepen L slightly (0.48-0.50).

**8. Spinner color preserved on tinted/gray destructive.**
`Button.css` — `.zs-button__spinner`. Codex: "Spinner color drifts toward purple/mauve in tinted and gray, which muddies destructive intent." Currently the spinner uses `stroke: currentColor` which follows the button's text color — destructive-tinted has accent-tinted-red-ish text, and gray-destructive has gray label color (no red signal). Fix:
```css
.zs-button--destructive .zs-button__spinner svg {
  stroke: var(--zs-system-red);
}
.zs-button--destructive.zs-button--filled .zs-button__spinner svg {
  stroke: var(--zs-accent-ink); /* white-on-red still white */
}
```

**9. Slot icons mechanically spaced + slightly large.**
`Button.css` — `.zs-button__slot--start` / `--end`. Codex: "Icons feel optically large and a little mechanically spaced." Fix: tighten the gap from `--zs-space-2` to `--zs-space-1` between slot and label; cap icon size to 0.875rem (the smaller of body or subheadline line-height):
```css
.zs-button__slot { font-size: 0.875rem; line-height: 1; }
.zs-button__slot--start { margin-inline-end: var(--zs-space-1); }
.zs-button__slot--end { margin-inline-start: var(--zs-space-1); }
.zs-button__slot svg { width: 1em; height: 1em; }
```

## Files to modify

- `sdks/ui/src/components/Button/Button.css` — items 1, 2, 3, 5, 6, 8, 9
- `sdks/ui/src/styles.css` — items 3 (token), 4 (palette adj), 7 (system-red chroma)
- (No story changes — fixes are component-level)

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity x5: 0 hex, 0 px, 0 zs-blur, 0 Card.Body, 0 CardBody.
4. `pnpm --filter zeroship-builder build` → green.
5. **A11y 56/1 clean** — items 4 (lower tinted alpha) and 7 (lower red chroma) are the highest contrast risk. If anything drops below AA 4.5:1, document the fallback chosen.
6. **A11y incomplete sweep**: 0 background-gradient + 0 pseudo-element across all 56 stories.
7. **Aria-wiring**: 13 PASS + 1 SKIP + 0 FAIL.
8. Re-capture Button PNGs via `capture-button-evidence.mjs` (10 PNGs). Eyeball:
   - `all-states` — disabled cell should now look genuinely disabled (not pending-active).
   - `destructive-disabled` — buttons should read as gray-with-red-hint, not tappable.
   - `loading-destructive` — buttons must be SAME WIDTH as `destructive` cell counterparts.
   - `focus-visible` — ring quieter than before, still keyboard-visible.

## Report

End with:
- Files changed.
- Per-item confirmation (1–9) with file:line refs.
- Token purity grep results (5 greps).
- Build status.
- A11y violations + incomplete sweep.
- Aria-wiring result.
- Any contingency that fired (item 7 contrast fallback).
- 10 Button screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
