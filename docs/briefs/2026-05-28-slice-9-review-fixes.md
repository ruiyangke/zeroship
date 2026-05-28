# Slice 9 review fixes — OtpField + Meter + Progress

**Worktree** `.worktrees/wave1-slice9` on branch `wave1-slice9`.

Merged: codex 2🔴+4🟡+1🟢 + claude 0🔴+2🟡+2🟢 = 1🔴 + 4🟡 + 2🟢 (the syntax 🔴 was fixed inline).

## Hard constraints

Standard. DO NOT commit; orchestrator merges.

## Fix list

### 🔴 (1)

**1. OtpField cell-0 still relies on ignored `aria-label`.**
`OtpField.tsx:224`.

Base UI ignores aria-label on cell 0 (`OTPFieldInput.js` `ariaLabel = index === 0 ? undefined : slotAriaLabel`). Opus's contingency wrapped bare stories in visually-hidden Field.Label — but cell 0 still has no accessible name when consumer doesn't use Field. Fix:
- When no Field context: stamp `aria-label` on the wrapper that owns cell 0's labeling via Base UI's exposed slot, OR
- Render a visually-hidden `<label htmlFor={cell0Id}>` inside the OtpField wrapper when no Field context detected.

Decision (autonomous): wrap each OtpField in a visually-hidden `<span>` that contributes via aria-labelledby on the wrapper, with text "Verification code". Document inline.

### 🟡 (4)

**2. OtpField forced-colors block omits `:hover` mirror.**
`OtpField.css:107-122` (oklch hover) vs `:223-279` (forced-colors).

Add inner `@media (hover: hover)` inside the `@media (forced-colors: active)` block mapping hover to system tokens — mirror Toggle.css:581-600.

**3. OtpField sm cells miss coarse-pointer hit-target floor.**
`OtpField.css:42-44, 52`.

Add `@media (pointer: coarse)` block bumping `--zs-otp-cell-size` for sm to `var(--zs-hit-min)` (2.75rem).

**4. Progress reduced-motion indeterminate styling can outrank forced-colors fallback.**
`Progress.css:157`.

The `prefers-reduced-motion: reduce` rule sits at higher specificity than the `forced-colors: active` fallback. Fix: combine the media queries (`@media (prefers-reduced-motion: reduce) and (forced-colors: active)`) OR move the forced-colors rule to higher specificity.

**5. Progress indeterminate RTL sweep misses `dir` on the root.**
`Progress.css:109`.

The `[dir="rtl"]` selector doesn't reach when `dir` is set on `<html>` not `.zs-progress`. Use `:dir(rtl)` instead (modern browsers) OR document the requirement in JSDoc.

### 🟢 (2)

**6. OtpField sm font-size duplicated.**
`OtpField.css:88-91` and `:192-194`. Delete the second; keep iOS-zoom comment in the first.

**7. Paste assertion only verifies split values, not paste focus advance.**
`check-aria-wiring.mjs:2502`. Extend assertion to verify `document.activeElement` is the last cell after paste.

**8. Progress.tsx TSDoc claims logical translation but CSS uses translateX.**
`Progress.tsx:50-52` vs `Progress.css:109-130`. Rewrite the comment to match reality.

## Verification gates

Standard. Token purity x5. Aria-wiring all PASS.

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice9`. Leave uncommitted.
