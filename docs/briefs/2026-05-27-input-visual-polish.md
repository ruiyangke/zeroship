# Input + Field visual polish (11 items, no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `359149f0`).

Closes the actionable findings from the codex visual review of 20 Input/Field screenshots — `/tmp/codex-input-visual-review.stdout` (the `-o` file was again blocked by codex's read-only sandbox; the full content is in stdout).

5 🔴 real bugs + 6 selected 🟡 taste calls. The 8 🟢 nits + remaining 🟡 stage-airiness/typography minutiae are story/demo concerns and deferred. No deferrals on the 11 items below.

## Goal

One commit that closes all 11 items. Builds + a11y verifier stay green. The visual review's "would ship after fixing these few regressions" verdict converts to "ready to ship".

## Hard constraints (unchanged)

- Worktree single-writer.
- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex, no raw px (incl. comments).
- HIG anchor; Base UI is the headless layer.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties — mandatory.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bugs (5)

**1. `custom-validate` story must actually trigger validation before capture.**
`Input.stories.tsx — CustomValidate` + `scripts/capture-input-evidence.mjs`.
The story configures Base UI's `validate={(v) => v === "admin" ? "Reserved" : null}` but the screenshot shows the value typed without the validation having fired. Two-part fix:
- In the story, render a multi-cell layout: one cell with the EMPTY default, one cell with the INVALID state pre-set via `defaultValue="admin"` so the validation fires on mount. Visual evidence is preserved without needing playwright interaction.
- Optionally also add a third cell where the validation has been triggered via user interaction (consumers can read it as documentation).
- Re-run `capture-input-evidence.mjs`; the resulting PNG should now visibly show the red invalid shell + "Reserved" error text.

**2. Long input value gets a right-edge fade mask + visible "more content" cue.**
`Input.css — .zs-input__control`. Native `<input>` clips and scrolls on focus; the value disappearing into the right edge without a visible cue is what codex flagged. Add a subtle mask gradient that fades the input's content at the right edge when overflowing:
```css
.zs-input__control {
  /* When the value overflows the visible area, fade the trailing edge
     so users see they've reached the end. The mask only applies when
     not focused — focus reveals the full value via native scroll. */
  mask-image: linear-gradient(to right, black calc(100% - 1.5rem), transparent 100%);
  -webkit-mask-image: linear-gradient(to right, black calc(100% - 1.5rem), transparent 100%);
}
.zs-input__control:focus {
  mask-image: none;
  -webkit-mask-image: none;
}
/* In RTL, flip the gradient. */
.zs-input__control[dir="rtl"],
[dir="rtl"] .zs-input__control {
  mask-image: linear-gradient(to left, black calc(100% - 1.5rem), transparent 100%);
  -webkit-mask-image: linear-gradient(to left, black calc(100% - 1.5rem), transparent 100%);
}
```
Verify on the `long-label-and-description` story — the clipped value should now have a soft right-edge fade.

**3. `autofill` story forces a visible autofilled state.**
`Input.stories.tsx — Autofill`. Today the story just shows an empty input with `autoComplete="email"` — autofill never visually fires in a static screenshot. Force it:
- Add a `defaultValue` to the autofill story's input so a value appears.
- Add an inline comment in the story description noting that the WebKit yellow background suppression has been verified (per the slice-2 commit's CSS override) and the autofill state visually matches a regular filled control. The yellow `-webkit-autofill` background overlay is the thing we suppress; the screenshot should show clean form chrome, NOT browser-yellow.
- Optionally add a second cell labelled "browser autofill (simulated)" with a manual styled overlay that mimics what Chrome would render WITHOUT our suppression rule, so reviewers see the before/after visually. Skip if too involved; the defaultValue alone is sufficient.

**4. Plain variant gets field affordance.**
`Input.css — .zs-input--plain`. Codex flagged "almost no text-field affordance — reads as static placeholder text unless the user already knows it's interactive." Add a persistent bottom border (the HIG iOS-keyboard text-field style — flat with a baseline hairline):
```css
.zs-input--plain {
  background: transparent;
  padding-inline: 0;
  box-shadow:
    /* hairline along the baseline only */
    inset 0 -0.0625rem 0 0 var(--zs-separator),
    0 0 0 0 transparent;
}
.zs-input--plain[data-focused] {
  /* on focus, intensify to the accent color */
  box-shadow:
    inset 0 -0.0625rem 0 0 var(--zs-accent),
    0 0 0 var(--zs-focus-ring-width) var(--zs-focus-ring-color);
}
@media (hover: hover) {
  .zs-input--plain:not([data-disabled]):not([data-focused]):not([data-readonly]):hover {
    box-shadow: inset 0 -0.0625rem 0 0 var(--zs-separator-strong);
  }
}
```
Visual verification: the `all-variants` story should now show the plain variant with a thin baseline that reads as "I'm an input you can type into".

**5. Readonly distinct from editable default state.**
`Input.css — .zs-input[data-readonly]`. Codex: "readonly is too close to default; it should not look disabled, but it needs a small visual cue that editing is unavailable." Add a subtle background shift + an italic value (HIG-aligned for non-editable text):
```css
.zs-input[data-readonly] {
  background: var(--zs-input-bg-readonly);
  cursor: default;
}
.zs-input[data-readonly] .zs-input__control {
  cursor: default;
  font-style: italic;       /* HIG: read-only / static text variants tend to italicize */
  color: var(--zs-input-ink-readonly);
}
```
Add the tokens to the crystal palette:
```css
--zs-input-bg-readonly: var(--zs-fill-tertiary);
--zs-input-ink-readonly: var(--zs-label-secondary);
```
Verify on `all-states` — the readonly "acme-prod" example should now visually read as "presented, not editable".

### 🟡 Taste calls (6 selected)

**6. Outline vs filled distinctness — bump filled's tint.**
`Input.css + crystal theme`. The current `--zs-input-bg-filled: var(--zs-fill-secondary)` (≈0.13 alpha) is close to the outline's `--zs-input-bg: var(--zs-fill-quaternary)` (≈0.06 alpha). Codex: "outline already has a tinted fill; the filled variant doesn't earn much distinctness." Push filled to a more substantial tint:
```css
--zs-input-bg-filled: var(--zs-fill);                /* was --zs-fill-secondary */
--zs-input-bg-filled-hover: color-mix(in oklch, var(--zs-label) 12%, transparent);
```
Resulting visual: outline = barely-tinted hairline-bordered shell; filled = clearly-filled-not-bordered shell. Distinction is now obvious.

**7. Focus ring slightly quieter.**
`Input.css + styles.css`. Codex: "focus ring is highly visible and correctly blue, but the double edge/halo is heavy compared with Apple's quieter field focus." Quiet it:
- Reduce `--zs-focus-ring-color` alpha from 0.35 → 0.25 (or use a NEW `--zs-focus-ring-color-soft` for inputs specifically, leaving Button's ring untouched if Button's needs the heavier ring).
- Apply ONLY to Input (Button keeps its current ring) so the Apple-quiet feel is in the form-control layer.

Implementation: define `--zs-input-focus-ring-color: color-mix(in oklch, var(--zs-accent) 25%, transparent);` in the crystal palette; use it specifically in `.zs-input[data-focused]`. Leave Button's `--zs-focus-ring-color` untouched.

**8. Optional fallback smaller + lighter.**
`Field.css — .zs-field__required--fallback`. Currently `font-size: var(--zs-text-footnote-size)` (already footnote 13). The complaint is it's "close enough in weight to the label that it competes." Refine:
```css
.zs-field__required--fallback {
  font-size: var(--zs-text-caption-1-size);   /* 12 — one step smaller */
  line-height: var(--zs-text-caption-1-line);
  font-weight: 400;
  color: var(--zs-label-tertiary);
  /* small inline-start gap so it reads as metadata-after-label */
  margin-inline-start: var(--zs-space-1);
}
```
NOTE: if `--zs-label-tertiary` against `--zs-surface` measures below 4.5:1 (the Card.Description fallback contingency), keep on `--zs-label-secondary` and let the size/weight differential carry the recede. Document the choice inline.

**9. Native date-input icon replacement via mask.**
`Input.css`. The native `::-webkit-calendar-picker-indicator` is a black calendar glyph that visually doesn't belong in the crystal palette. Replace via CSS mask + a small inline SVG data URI:
```css
.zs-input__control[type="date"]::-webkit-calendar-picker-indicator,
.zs-input__control[type="time"]::-webkit-calendar-picker-indicator,
.zs-input__control[type="datetime-local"]::-webkit-calendar-picker-indicator,
.zs-input__control[type="month"]::-webkit-calendar-picker-indicator,
.zs-input__control[type="week"]::-webkit-calendar-picker-indicator {
  /* Replace the UA icon with our own calendar glyph in the accent ink. */
  filter: none;
  background: var(--zs-label-secondary);
  -webkit-mask-image: url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><path d='M11 1v1.5h2.25A1.75 1.75 0 0 1 15 4.25v9.5A1.75 1.75 0 0 1 13.25 15.5h-10.5A1.75 1.75 0 0 1 1 13.75v-9.5C1 3.286 1.786 2.5 2.75 2.5H5V1h1.5v1.5h3V1H11Zm2.5 6h-11v6.75c0 .138.112.25.25.25h10.5a.25.25 0 0 0 .25-.25V7Z' fill='currentColor'/></svg>");
  mask-image: url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><path d='M11 1v1.5h2.25A1.75 1.75 0 0 1 15 4.25v9.5A1.75 1.75 0 0 1 13.25 15.5h-10.5A1.75 1.75 0 0 1 1 13.75v-9.5C1 3.286 1.786 2.5 2.75 2.5H5V1h1.5v1.5h3V1H11Zm2.5 6h-11v6.75c0 .138.112.25.25.25h10.5a.25.25 0 0 0 .25-.25V7Z' fill='currentColor'/></svg>");
  -webkit-mask-repeat: no-repeat;
  mask-repeat: no-repeat;
  -webkit-mask-position: center;
  mask-position: center;
  -webkit-mask-size: 1rem;
  mask-size: 1rem;
  cursor: pointer;
}
```
Note: SVG data URI inline; no new file. The icon is a simple calendar outline matching the lucide-style weight of the search slot icon in WithSlots. Verify visually on the `input-types` story — the date input's icon should now match the rest.

**10. Disabled ink recedes one step further.**
`Input.css + crystal palette`. Currently `--zs-input-ink-disabled: var(--zs-label-tertiary)`. Codex: "they could recede a notch more through text color." Shift to:
```css
--zs-input-ink-disabled: var(--zs-label-quaternary);
```
NOTE: same a11y contingency as item 8 — if quaternary measures below 3:1 against the disabled bg (`--zs-input-bg-disabled`), keep on tertiary. Disabled is usually exempt from 4.5:1 (axe doesn't require AA on disabled controls), but the rendering should still be readable. Document the choice.

**11. Clear-button hit area enlarged.**
`Input.stories.tsx — WithSlots` (the clear-button cell). Today the clear button (`×` glyph) has visually quiet hit area. This is a STORY-level demo fix (the clear button isn't a component primitive yet). Update the story's clear-button rendering to:
- Use `<Button variant="plain" size="small">` instead of a raw `×` glyph so it carries the existing 44pt hit-area extension from Button slice 1.
- The visible icon stays small; the tap target is comfortable.

(When we ship a real `clearable` prop on Input — slice 4 or later — the clear button becomes a first-class subpart with proper sizing baked in.)

## Files to modify

- `sdks/ui/src/components/Input/Input.css` — items 2, 4, 5, 6 (filled tint), 7 (focus ring), 9 (date icon mask), 10 (disabled ink)
- `sdks/ui/src/components/Field/Field.css` — item 8 (optional fallback)
- `sdks/ui/src/stories/Input.stories.tsx` — items 1 (custom-validate cells), 3 (autofill defaultValue), 11 (clear button uses Button)
- `sdks/ui/src/styles.css` — items 5 (readonly tokens), 6 (filled bg), 7 (input focus ring color), 10 (disabled ink override)
- `sdks/ui/scripts/capture-input-evidence.mjs` — no new captures needed; existing 20 stories already cover

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity (incl. comments):
   - hex / px / zs-blur / Card.Body / CardBody — all 5 greps empty.
4. `pnpm --filter zeroship-builder build` → green.
5. A11y violations: must report `A11y clean for 56 stories across 1 themes`. CRITICAL: the disabled-ink shift (item 10) is the highest-risk a11y change — verify contrast holds on disabled inputs against the disabled bg. The optional-fallback tertiary (item 8) is also at risk.
6. A11y incomplete sweep (one-off scanner; delete after): 0 background-gradient, 0 pseudo-element across all 56 stories. NOTE: the mask-image fade gradient on `.zs-input__control` (item 2) is a CSS gradient — verify axe doesn't flag it. If it does, the gradient is on the INPUT element which has no descendants for axe to walk past, so it should be safe; if axe reports otherwise, switch to a parent-level fade overlay.
7. Aria wiring: `check-aria-wiring.mjs` still 13 PASS + 1 SKIP + 0 FAIL.
8. Re-capture Input PNGs via `capture-input-evidence.mjs`. View on the screenshots that matter:
   - `custom-validate` — should now visibly show the invalid state + "Reserved" error
   - `long-label-and-description` — the value's right edge should fade rather than hard-clip
   - `autofill` — value should be present so the autofill-suppressed-but-clean state is visible
   - `all-variants` — plain variant should now have a visible baseline
   - `all-states` — readonly should look distinct (different bg + italic value)
   - `all-variants` — outline + filled should be visibly distinct
   - `required` — `(optional)` should read as metadata, smaller + lighter than the label

## Report (stdout)

End with:
- Files changed.
- Per-item confirmation (1–11) with file:line refs.
- Token purity grep results — all 5.
- Build status — both packages.
- A11y violations result.
- A11y incomplete result.
- Aria wiring result.
- Any contrast contingencies that fired (item 8 or 10 falling back from tertiary/quaternary).
- 20 Input screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
