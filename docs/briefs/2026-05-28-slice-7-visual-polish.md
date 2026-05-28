# Slice 7 visual polish — NumberField + Slider

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `cfc0b4bf`).

Closes codex visual review: 3🔴 + 2🟡 + 1🟢. Codex transcript at `/tmp/.../bpxbt7d85.output:5380-5475`.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only.
- Glass-surface invariant.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bugs (3)

**1. Slider value label detached from thumb.**
`Slider.css:51`, `Slider.tsx:281`.

WithValue's "60%" sits top-right instead of near the handle. Move into the control overlay, position from the same thumb position var:
```css
.zs-slider__value {
  position: absolute;
  inset-block-end: calc(100% + var(--zs-space-2));
  inset-inline-start: var(--zs-slider-value-position, 50%);
  transform: translateX(-50%);
  color: var(--zs-label-secondary);
}
```

Slider.tsx must compute `--zs-slider-value-position` from the current value's percentage. Use Base UI's emitted `data-value` or a render-callback state read to inline-style the position. If that's too invasive for a polish pass, fall back to `inset-inline-start: 50%` (centered above the track) and document inline.

**2. Field-wrapped horizontal Slider visually collapses.**
`Slider.css:35`, `stories/Slider.stories.tsx:231` (WithLabel + Disabled stories).

Disabled inherits Field's tight intrinsic width → Slider shrinks to a short dash. Add a horizontal floor:
```css
.zs-slider--horizontal {
  min-inline-size: var(--zs-slider-inline-min, calc(var(--zs-control-h-lg) * 4));
}
.zs-field > .zs-slider--horizontal {
  inline-size: 100%;
}
```

**3. NumberField scrub target reads as blank chip.**
`NumberField.css:281`, `NumberField.tsx:253`.

Scrub target is too pale and has no resting glyph. Add a visible rest treatment + bidirectional-arrow ::before glyph:
```css
.zs-number-field__scrub-area {
  background-color: var(--zs-input-bg-hover);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-input-border);
  color: var(--zs-label-secondary);
}
.zs-number-field__scrub-area::before {
  content: "";
  display: block;
  inline-size: 0.625rem;
  block-size: 0.625rem;
  background-color: currentColor;
  -webkit-mask: url("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'><path d='M1 5 L3 3 L3 7 Z M9 5 L7 3 L7 7 Z' fill='black'/></svg>") center/contain no-repeat;
  mask: url("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'><path d='M1 5 L3 3 L3 7 Z M9 5 L7 3 L7 7 Z' fill='black'/></svg>") center/contain no-repeat;
}
.zs-number-field__scrub-area:hover {
  color: var(--zs-accent-active);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-input-border-hover);
}
```

The SVG mask renders two opposing triangles (left + right arrows). Uses `mask` instead of `background-image` so `currentColor` works for forced-colors mappings.

### 🟡 Calibration (2)

**4. Slider thumb rim too close to indicator → reads as a "blue knot".**
`Slider.css:117, 138, 195`.

Reduce rest rim mass to a hairline-derived size; lighten the shadow:
```css
.zs-slider__thumb {
  box-shadow:
    0 0 0 calc(var(--zs-focus-ring-width) - var(--zs-selection-hairline)) var(--zs-accent),
    0 var(--zs-space-half) var(--zs-space-2) color-mix(in oklch, var(--zs-label) 18%, transparent);
}
.zs-slider--outline .zs-slider__thumb {
  box-shadow: 0 0 0 var(--zs-selection-hairline) var(--zs-accent);
}
.zs-slider--outline .zs-slider__indicator {
  background-color: color-mix(in oklch, var(--zs-accent) 45%, transparent);
}
```

**5. Disabled Slider value (track/thumb) still feels active.**
`Slider.css:237, 240`.

Disabled fill/rim use text-strength gray. Shift to non-text disabled tokens:
```css
.zs-slider[data-disabled] .zs-slider__indicator {
  background-color: var(--zs-fill);
}
.zs-slider[data-disabled] .zs-slider__thumb {
  background-color: var(--zs-surface-raised);
  box-shadow:
    0 0 0 var(--zs-selection-hairline) var(--zs-label-quaternary),
    0 0 0 0 transparent;
}
```

### 🟢 Nit (1)

**6. NumberField default and outline variants nearly indistinguishable.**
`NumberField.css:73`, `styles.css:393` (Crystal's `--zs-input-bg`).

Crystal's `--zs-input-bg` is too close to transparent on the panel. DECISION: make `default` the more-material variant. Set:
```css
.zs-number-field--default .zs-number-field__group {
  background-color: var(--zs-input-bg-filled);
}
.zs-number-field--default .zs-number-field__group:hover {
  background-color: var(--zs-input-bg-filled-hover);
}
```

If `--zs-input-bg-filled-hover` doesn't exist, fall back to `color-mix(in oklch, var(--zs-input-bg-filled), var(--zs-label) 4%)`.

## Files to modify

- `sdks/ui/src/components/Slider/Slider.css` — items 1, 2, 4, 5.
- `sdks/ui/src/components/Slider/Slider.tsx` — item 1 (inline-style the `--zs-slider-value-position` custom property from current value percentage).
- `sdks/ui/src/components/NumberField/NumberField.css` — items 3, 6.

(No story changes; no aria-wiring changes — visual-only polish.)

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5 + raw `oklch(` in component CSS = 0.
5. A11y clean for 181 stories.
6. Aria-wiring 65 PASS + 2 SKIP + 0 FAIL (unchanged).
7. Re-capture 27 Slice 7 PNGs. Eyeball:
   - WithValue: "60%" sits ABOVE the thumb, not top-right.
   - WithLabel / Disabled Slider: minimum width visible.
   - ScrubArea: visible chip with arrow glyph at rest.
   - AllVariants: thumb rim feels lighter; outline variant indicator 45% mix not solid.
   - Disabled Slider: track/thumb read inactive.
   - AllVariants NumberField: default vs outline now legibly different.

## Contingencies

- **Item 1 value-position computation**: Slider.tsx needs to read the current `value` and emit `style={{ "--zs-slider-value-position": `${percent}%` }}` on the Root. If `value` is a controlled prop, compute `(value - min) / (max - min) * 100`. For range mode, multiple thumbs → pick the FIRST thumb for the value-label position (single-thumb is the common case). Document inline.
- **Item 3 SVG data-URI mask**: ensure escaped quotes. Test in dev — if Chrome/Firefox both render the mask correctly, ship. Otherwise fall back to a simpler ::before character (e.g., "⇔" U+21D4) and document.
- **Item 6 token existence**: grep `styles.css` for `--zs-input-bg-filled-hover`. If absent, use the inline `color-mix` fallback.

## Report

End with files changed; per-item confirmation (1–6); token purity; build status; a11y count; aria-wiring counts; contingencies fired; 27 PNG paths CONFIRMING the WithValue label is above the thumb, ScrubArea has arrow glyph, AllVariants legibly distinct; one taste note; "I did NOT commit, push, or merge."
