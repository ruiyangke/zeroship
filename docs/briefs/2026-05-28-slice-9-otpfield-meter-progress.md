# Slice 9 — OtpField + Meter + Progress (small surfaces)

**Worktree** `.worktrees/wave1-slice9` on branch `wave1-slice9` off `builder/ui-design@e17f6655`.

Three small surfaces: `otp-field` (one-time-password code input), `meter` (gauge), `progress` (task bar).

## Goal

- **OtpField** — N-digit code entry with one focusable input per cell. Used for 2FA / email verification.
- **Meter** — static gauge: battery level, disk full, signal strength. Different ranges paint different intent colors.
- **Progress** — task bar: indeterminate spinner OR determinate fill (0-100%).

## Hard constraints

Standard set (see Slice 8 brief). DO NOT commit; orchestrator merges.

## API shape

### OtpField
```tsx
export interface OtpFieldProps extends Omit<BaseOtpFieldRootProps, "render"> {
  /** Number of digits — default 6. */
  length?: number;
  size?: "sm" | "md" | "lg";
  variant?: "default" | "outline";
  className?: string;
}
```

### Meter
```tsx
export interface MeterProps extends Omit<BaseMeterRootProps, "render"> {
  size?: "sm" | "md" | "lg";
  /** Intent — `neutral` accent, `success` green, `warning` amber, `danger` red. */
  intent?: "neutral" | "success" | "warning" | "danger";
  /** Show the numeric value next to the gauge. */
  showValue?: boolean;
  /** Optional label. */
  label?: string;
  className?: string;
}
```

### Progress
```tsx
export interface ProgressProps extends Omit<BaseProgressRootProps, "render"> {
  size?: "sm" | "md" | "lg";
  /** Show percentage label next to the bar. */
  showValue?: boolean;
  label?: string;
  className?: string;
}
```

For both Meter + Progress, `value === null` (or undefined) → indeterminate mode (Progress only — Meter requires a value).

## Files to create

```
sdks/ui/src/components/OtpField/OtpField.{tsx,css}, index.ts (~150 lines)
sdks/ui/src/components/Meter/Meter.{tsx,css}, index.ts (~140 lines)
sdks/ui/src/components/Progress/Progress.{tsx,css}, index.ts (~140 lines)
sdks/ui/src/stories/OtpField.stories.tsx (8)
sdks/ui/src/stories/Meter.stories.tsx (7)
sdks/ui/src/stories/Progress.stories.tsx (7)
sdks/ui/scripts/capture-{otpfield,meter,progress}-evidence.mjs (NEW)
```

## Files to modify

- Barrel exports (components/index.ts, src/index.ts).
- styles.css (3 @imports).
- check-storybook-a11y.mjs (22 new story IDs).
- check-aria-wiring.mjs (6 new assertions).

## Story matrix (22 total)

### OtpField (8)
1. Basic 6-digit · 2. CustomLength (4-digit) · 3. AllSizes · 4. AllVariants · 5. WithLabel (Field) · 6. Required + RequiredInvalid · 7. Disabled · 8. RTL.

### Meter (7)
9. Basic (60%) · 10. AllIntents (4 colors at same value) · 11. AllSizes · 12. WithValue (label + percent) · 13. Ranges (0–25 danger, 25–75 warning, 75–100 success — `getStatus(value)` heuristic) · 14. Disabled · 15. RTL.

### Progress (7)
16. Determinate · 17. Indeterminate · 18. AllSizes · 19. WithValue · 20. CompletionCelebrate (100% state styling) · 21. WithLabel · 22. RTL.

## Aria-wiring (6 new)

1. OtpField typing first cell auto-advances focus to next cell.
2. OtpField paste of 6 digits fills all cells.
3. OtpField Required + Field.Error fires on incomplete submit.
4. Meter aria-valuenow reflects current value.
5. Progress determinate aria-valuenow updates as value changes.
6. Progress indeterminate has aria-valuetext "Loading" (no specific %).

## Verification gates

A11y 203 stories. Aria-wiring 71 PASS + 2 SKIP + 0 FAIL (65 + 6 new). 22 PNGs.

## Contingencies (decide inline)

- **OtpField input mode**: each cell is a single-char input. Base UI handles auto-advance and paste-split. Verify on paste.
- **Meter intent colors**: use `--zs-system-{green,amber,red}` if those tokens exist; otherwise inline `oklch()` IS allowed in styles.css FOUNDATION (just not in component CSS).
- **Progress indeterminate animation**: linear shimmer or sweep. Respect prefers-reduced-motion → no animation.
- **Meter Ranges heuristic**: each story can pick a static intent based on value bucket.

## Report

End with files changed; per-component API (file:line); token purity; tsc + build status; a11y count; aria-wiring counts; contingencies fired; 22 PNGs; one taste note; "I did NOT commit, push, or merge."

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice9`. Leave uncommitted.
