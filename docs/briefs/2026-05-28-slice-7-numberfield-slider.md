# Slice 7 — NumberField + Slider (numeric inputs)

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `3e83462f`, post Slice-6-complete).

Two numeric input surfaces from Base UI: `number-field` (text + stepper buttons + scrub-area gesture) and `slider` (range with thumb + track). Both reuse Field cascade from Slice 4 and Input's height/focus ring.

## Goal

- **NumberField** — text input with `-` / `+` stepper buttons and an optional drag-to-scrub area. Settings-app pattern: precise numeric entry with quick increment/decrement.
- **Slider** — continuous range, single thumb (or two for ranges). Thumb fills with accent; track shows progress.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / `oklch()` in component CSS.
- Glass-surface invariant.
- HIG as principle but NOT in source.
- `prefers-reduced-motion`, `@media (forced-colors: active)` (with the specificity mirror lesson from Slice 5/6), RTL via logical properties.
- Real-path aria-wiring.
- Hit-target floor ≥ 1.75rem for stepper buttons and slider thumb.
- DO NOT commit, push, or merge.

## API shape

### NumberField

```tsx
// sdks/ui/src/components/NumberField/NumberField.tsx
export interface NumberFieldProps extends Omit<BaseNumberFieldRootProps, "render"> {
  /** Size — sm 32 / md 40 (default) / lg 48 — matches Input rhythm. */
  size?: "sm" | "md" | "lg";
  /** Visual variant — default filled / outline border-only. Mirrors Input. */
  variant?: "default" | "outline";
  /** Show the drag-to-scrub area on hover/focus. */
  showScrub?: boolean;
  /** Placeholder for empty state. */
  placeholder?: string;
  className?: string;
}
```

Subparts (namespace):
- `NumberField.Group` — wrapper for the input + buttons row (Base UI's NumberField.Group).
- (Increment/Decrement/Input/ScrubArea are rendered internally; consumers see one component.)

### Slider

```tsx
// sdks/ui/src/components/Slider/Slider.tsx
export interface SliderProps extends Omit<BaseSliderRootProps, "render"> {
  /** Size — sm 24 / md 32 (default) / lg 40 (track block-size). */
  size?: "sm" | "md" | "lg";
  /** Visual variant — default solid accent / outline rim. */
  variant?: "default" | "outline";
  /** Show numeric value next to the thumb. */
  showValue?: boolean;
  className?: string;
}
```

Subparts not exposed individually — Slider internally renders Root + Track + Indicator + Thumb (single or range).

## Files to create

```
sdks/ui/src/components/NumberField/
  NumberField.tsx     (~280 lines)
  NumberField.css
  index.ts

sdks/ui/src/components/Slider/
  Slider.tsx          (~220 lines)
  Slider.css
  index.ts

sdks/ui/src/stories/
  NumberField.stories.tsx  (10 stories)
  Slider.stories.tsx       (10 stories)

sdks/ui/scripts/
  capture-numberfield-evidence.mjs  (NEW)
  capture-slider-evidence.mjs       (NEW)
```

## Story matrix (20 total)

### NumberField (10)
1. Basic; 2. AllSizes; 3. AllVariants; 4. MinMaxStep (1-100 step 5); 5. Currency (snapToStep + formatOptions); 6. ScrubArea; 7. Disabled; 8. WithLabel + Field cascade; 9. Required + RequiredInvalid; 10. RTL.

### Slider (10)
11. Basic (single thumb); 12. Range (two thumbs); 13. AllSizes; 14. AllVariants; 15. Steps (discrete with tick marks); 16. WithValue (numeric label); 17. Disabled; 18. WithLabel; 19. Vertical orientation; 20. RTL.

## Aria-wiring (6 new)

1. NumberField stepper — click `+` 3 times; assert value = initial + 3 × step.
2. NumberField keyboard — focus input, ArrowUp twice; assert value flipped.
3. NumberField ScrubArea — drag scrub element 50px; assert value increased.
4. Slider keyboard — focus thumb, ArrowRight 5 times; assert value increased by 5 × step.
5. Slider Range — drag thumb-1 right; assert value[0] > original AND value[1] unchanged.
6. Slider forced-colors hover — thumb computed bg = system color (Highlight), not oklch.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm exec tsc -p sdks/ui/tsconfig.json --noEmit` — clean.
3. `pnpm --filter @zeroship/ui build-storybook` — green.
4. `pnpm --filter zeroship-builder build` — green.
5. Token purity + raw `oklch(` = 0.
6. A11y clean for 174 stories (154 + 20 new).
7. Aria-wiring 58 PASS + 2 SKIP + 0 FAIL (52 + 6 new).
8. 20 PNGs captured.

## Contingencies (decide inline)

- **NumberField formatOptions for currency**: Base UI uses Intl.NumberFormatOptions. Use `style: "currency", currency: "USD"` for the Currency story.
- **Slider Range thumb count**: pass `value={[20, 60]}` (array) for range mode. Base UI auto-detects.
- **Slider Vertical**: set `orientation="vertical"`. Track flips to block-axis.
- **Scrub cursor**: Base UI has `NumberField.ScrubAreaCursor` for the custom drag cursor. Wire it.
- **Stepper hit-target**: stepper buttons must be ≥ 1.75rem (sm) / 2.75rem (coarse). Apply the chip-overlay pattern from Slice 4 if intrinsic size is below.
- **Forced-colors specificity mirror**: apply Slice 5/6 lesson — every state selector inside the @media block at equal/higher specificity.
- **Required cascade**: Field context drives required + size + disabled. Mirror Slice 4 pattern.

## Report

End with files changed; per-component API (file:line); token purity; tsc + build status; a11y count; aria-wiring counts; contingencies fired; 20 screenshot paths; one taste note; "I did NOT commit, push, or merge."
