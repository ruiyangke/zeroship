# Dialog + AlertDialog visual polish (Phase 4 of the missing-reviews plan)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `721e5d89`, post-Phase-2.C).

Closes the two parallel codex visual reviews of post-fix Dialog and AlertDialog. 9 merged items (Dialog 5 + AlertDialog 4 — the 🔴 in each is the same `zs-button--md` → `zs-button--medium` story typo, deduped to one fix). Codex transcripts at `/tmp/claude-1000/-home-ruiyang-Projects-appbase/fd3537bf-21aa-4f35-8e6c-6bb2b6f1cf67/tasks/b385k7nkt.output` lines 5060–5222 (Dialog) and `…/b6h1fuv29.output` lines 4280–4448 (AlertDialog).

## Goal

One commit closing the visual calibration items both reviews flagged. The character of the findings is consistent: the modal stack reads heavier than light glass should — scrim too dark, shadow too bloomy, no backdrop-filter on the alert backdrop, size variants don't carry proportional density. Plus the two story-evidence typos that hide consumer button styling.

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`.
- No raw hex, no raw px (incl. comments).
- HIG anchor as principle but **not in source**.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bug (1, twin instances)

**1. Story typo `zs-button--md` → `zs-button--medium`.** Two locations:
- `sdks/ui/src/stories/Dialog.stories.tsx:370` (CloseAsChild story)
- `sdks/ui/src/stories/AlertDialog.stories.tsx:452` (CancelAsChild story)

Both stories hand-roll a custom close target with `className="zs-button zs-button--gray zs-button--md"`. The Button size class is `zs-button--medium`, not `--md`. The result is a flat strip without the button radius / height / inner-label structure — undercutting the asChild stories' whole point (verify consumer button styling survives the Slot composition).

Fix: rename `zs-button--md` → `zs-button--medium` in both stories. Verify visually that the custom Done/Done targets now match the Cancel/Proceed siblings in radius, height, and label inset.

### 🟡 Calibration / refinement (7)

**2. Crystal scrim too assertive.**
`sdks/ui/src/styles.css:311` (`--zs-scrim` in the `[data-theme="crystal"]` block).

Codex: "in the modal captures, the Crystal mesh collapses into a flat gray-violet field. The dialog still reads, but the backdrop feels more like a generic dimmer than light glass." Current value is `color-mix(in oklch, black 40%, transparent)` — a black-weighted mix. On a light-glass theme, dimming should suppress the app while preserving the cool mesh and material depth; a label-based mix at lower opacity reads less generic.

CSS recipe (replace the Crystal override at styles.css:311):
```css
[data-theme="crystal"] {
  --zs-scrim: color-mix(in oklch, var(--zs-label) 30%, transparent);
  --zs-material-tint: color-mix(in oklch, var(--zs-surface-overlay) 78%, transparent);
}
```

Note: `--zs-surface-overlay` may need defining if it isn't already a foundation token — check `:root` in `styles.css` first. If it doesn't exist, use `var(--zs-surface)` (the Crystal panel surface) instead.

**3. Dialog popup elevation too heavy for glass.**
`sdks/ui/src/styles.css:174` (`--zs-shadow-dialog` foundation token).

Codex: "the sheet has a large dark bloom, especially in nested and non-modal captures. The shadow draws more attention than the surface edge." Current token is a single big drop at 35% black `0 1.5rem 4rem oklch(0 0 0 / 0.35)`. Softer treatment uses two layered shadows (one for distance, one for proximity) at label-based opacity, mirroring how Card.css already builds its elevation:

```css
:root {
  --zs-shadow-dialog:
    0 var(--zs-space-6) var(--zs-space-10)
      color-mix(in oklch, var(--zs-label) 24%, transparent),
    0 var(--zs-space-2) var(--zs-space-6)
      color-mix(in oklch, var(--zs-label) 12%, transparent),
    0 0 0 0.0625rem color-mix(in oklch, var(--zs-label) 6%, transparent);
}
```

The inner hairline (third stack) substitutes for the bigger shadow's bloom — gives the popup a defined edge without the dark glow. If `--zs-card-surface-edge` already exists in the codebase (codex referenced it), prefer that token over the inline `0 0 0 0.0625rem` ring; otherwise inline as shown.

**4. Dialog size variants only change width, not density.**
`sdks/ui/src/components/Dialog/Dialog.css` (multiple rules: `.zs-dialog__header`, `.zs-dialog__body`, `.zs-dialog__footer`, and the `[data-size="full"]` safe-area block).

Codex: "`md` looks balanced, but the same padding makes narrow dialogs feel chunky and leaves larger dialogs without a stronger sheet rhythm. The long-footer `sm` capture is the clearest symptom: content and actions are competing with the fixed inset." Currently `data-size` swaps only `max-inline-size`. Padding stays at a single `--zs-dialog-padding` regardless.

Add a per-size padding token resolved on the popup:
```css
:root {
  --zs-dialog-padding-sm: var(--zs-space-5);
  --zs-dialog-padding-md: var(--zs-space-6);
  --zs-dialog-padding-lg: var(--zs-space-7);
}

.zs-dialog-popup {
  --zs-dialog-padding-current: var(--zs-dialog-padding-md);
}
.zs-dialog-popup[data-size="sm"] {
  --zs-dialog-padding-current: var(--zs-dialog-padding-sm);
}
.zs-dialog-popup[data-size="lg"] {
  --zs-dialog-padding-current: var(--zs-dialog-padding-lg);
}

.zs-dialog__header {
  padding: var(--zs-dialog-padding-current);
  padding-block-end: var(--zs-space-3);
}
.zs-dialog__body {
  padding-inline: var(--zs-dialog-padding-current);
}
.zs-dialog__footer {
  padding: var(--zs-dialog-padding-current);
  padding-block-start: var(--zs-space-4);
}
```

Update the full-size safe-area rules (item 12 of Phase 2.B) to use `--zs-dialog-padding-current` instead of the old `--zs-dialog-padding` so the safe-area max() still composes against the right base.

**5. Long footer wrap reads accidental in `sm` + 3-button.**
`sdks/ui/src/components/Dialog/Dialog.css` (the `.zs-dialog__footer` block, around the `flex-wrap: wrap` line from Phase 2.B item 8).

Codex: "two neutral actions sit on the first row and the primary action drops below. It avoids overflow, but the row geometry feels like an emergency wrap rather than an intentional action layout." Fix the policy with a `:has(> :nth-child(3))` rule that stretches the cells in `sm` 3+ states:

```css
.zs-dialog__footer {
  gap: var(--zs-space-3);
  row-gap: var(--zs-space-2);
}

.zs-dialog-popup[data-size="sm"] .zs-dialog__footer:has(> :nth-child(3)) {
  justify-content: stretch;
}

.zs-dialog-popup[data-size="sm"] .zs-dialog__footer:has(> :nth-child(3)) > * {
  flex: 1 1 calc((100% - var(--zs-space-3)) / 2);
}

.zs-dialog-popup[data-size="sm"]
  .zs-dialog__footer:has(> :nth-child(3))
  > .zs-button--filled {
  flex-basis: 100%;
}
```

Larger sizes keep the trailing-aligned `flex-wrap` shape — they have room for a single row.

**6. AlertDialog backdrop lets background compete.**
`sdks/ui/src/components/AlertDialog/AlertDialog.css` `.zs-alertdialog-backdrop` rule.

Codex: "in the ESC and asChild evidence screenshots, the underlying story row is still visible as a soft horizontal pill behind the popup… the silhouette remains strong enough to read as a second surface attached to the alert." Alerts demand harder focus than Dialogs — the background should feel structurally unavailable. Add a real `backdrop-filter` (the Dialog backdrop has none on Crystal because `--zs-dialog-backdrop-filter: none` is the Crystal gate):

```css
.zs-alertdialog-backdrop {
  background-color: var(--zs-scrim);
  backdrop-filter: var(--zs-material-thick);
  -webkit-backdrop-filter: var(--zs-material-thick);
}
```

This bypasses the Crystal Dialog-backdrop-filter gate intentionally — alerts get the harder material treatment regardless of theme. Confirm the rule lands AFTER any Dialog backdrop inheritance in the cascade.

**7. Compact alert popup inherits full Dialog shadow.**
`sdks/ui/src/components/AlertDialog/AlertDialog.css` `.zs-alertdialog-popup` rule.

Codex: "The small alert card has a large dark shadow bloom, especially in the one-button and two-button states. It gives the compact alert more mass than its content warrants." AlertDialog inherits `--zs-shadow-dialog`, which is tuned for larger modal sheets. After item 3 already softens that token, this fix layers a smaller elevation specifically for alerts:

```css
.zs-alertdialog-popup {
  box-shadow: var(--zs-shadow-4);
}
```

Verify `--zs-shadow-4` exists in `:root` (it does — styles.css:172). Captures one-button + two-button should look meaningfully lighter; three-button should still feel substantial.

**8. AlertDialog body text scale doesn't match alert density.**
`sdks/ui/src/components/AlertDialog/AlertDialog.css` — add a `.zs-alertdialog__body` rule.

Codex: "AlertDialog.Body also inherits the regular Dialog body size, which is larger than the alert description size and can make body prose feel too loud in compact alerts." Drop alert body to the subheadline size; reset embedded form fields to body size so an Input inside the body keeps its own typography:

```css
.zs-alertdialog__body {
  font-size: var(--zs-text-subheadline-size);
  line-height: var(--zs-text-subheadline-line);
  color: var(--zs-label-secondary);
}

.zs-alertdialog__body :where(.zs-input, input, textarea, select) {
  font-size: var(--zs-text-body-size);
  line-height: var(--zs-text-body-line);
  color: var(--zs-input-ink);
}
```

Verify the `--zs-text-subheadline-*` and `--zs-text-body-*` tokens are defined in `:root` (they should be — they're part of the foundation type scale).

**9. AlertDialog WithBody raw input bypasses Input component.**
`sdks/ui/src/stories/AlertDialog.stories.tsx:166` (the raw `<input>` in the WithBody story).

Codex: "the input in the WithBody screenshot reads more like an unthemed native field than a Crystal control. Its text rhythm and focused border are visually louder and less refined than the surrounding alert text and buttons." Story-only fix: replace the raw `<input style={{...}}>` with the actual `Input` component from `@zeroship/ui` so the existing focus + shell + edge tokens are exercised. Keeps the story honest as evidence that AlertDialog + Input compose correctly.

## Files to modify

- `sdks/ui/src/styles.css` — items 2 (scrim Crystal override), 3 (shadow-dialog foundation), 4 (per-size padding tokens)
- `sdks/ui/src/components/Dialog/Dialog.css` — items 4 (per-size padding application), 5 (`sm` 3+ footer policy)
- `sdks/ui/src/components/AlertDialog/AlertDialog.css` — items 6 (backdrop-filter), 7 (smaller shadow), 8 (body type scale)
- `sdks/ui/src/stories/Dialog.stories.tsx` — item 1 (`--md` → `--medium`)
- `sdks/ui/src/stories/AlertDialog.stories.tsx` — items 1 (`--md` → `--medium`), 9 (raw input → Input component)

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity x5: 0 hex / px ≤3 pre-existing / zs-blur 0 / Card.Body 0 / CardBody 0.
4. `pnpm --filter zeroship-builder build` → green.
5. **A11y clean across all 67 stories.** Items 2, 3, 7 are the highest contrast risk:
   - Item 2: scrim moves from black-mix to label-mix at 30% — verify the dim-through-color still passes axe color-contrast checks for any text rendered with `aria-hidden=false` inside the inactive layer (typically none, but the Storybook docs page itself has live elements behind the popup in some captures).
   - Item 3: softer shadow-dialog won't affect contrast directly but should leave the popup-vs-backdrop edge clearly defined — a11y won't catch this, eyeball the screenshots.
   - Item 7: `--zs-shadow-4` is already in use elsewhere; same contrast story as item 3.
6. **A11y incomplete sweep**: 0 background-gradient + 0 pseudo-element across all stories (items 2, 3, 6, 7 all touch shadow/backdrop — the incomplete sweep is the canary).
7. **Aria-wiring**: 20 PASS + 2 SKIP + 0 FAIL (no change expected — visual-only polish).
8. Re-capture Dialog (13) + AlertDialog (14) PNGs. Eyeball:
   - `default`, `sizes`, `nested`, `with-form` — popup should read lighter, backdrop cooler, edge crisper.
   - `long-footer-labels` — 3-button `sm` layout should now look intentional (50/50 with primary spanning full width).
   - `close-as-child`, `cancel-as-child` — custom Done/Done targets should match the Cancel/Proceed siblings in height/radius.
   - AlertDialog `one-button`, `two-buttons` — alert visibly more compact in elevation than Dialog.
   - AlertDialog `with-body` — body prose secondary; embedded Input matches Crystal control rhythm.

## Contingencies

- **Item 2 contrast**: if axe flags a Dialog story for "color-contrast" after the scrim drops to 30% label mix, push to 35% (still label-based, slightly darker). Document the choice inline.
- **Item 3 (`--zs-card-surface-edge` reference)**: codex referenced this token in the shadow recipe. If it doesn't exist in `:root`, fall back to the inline `0 0 0 0.0625rem color-mix(in oklch, var(--zs-label) 6%, transparent)` as written in the brief recipe.
- **Item 4 (size padding rollout)**: if any Dialog story's existing layout depends on the old single-padding token (`--zs-dialog-padding`), the new `--zs-dialog-padding-current` should still default to `--zs-dialog-padding-md` so the existing visual matches `md` exactly. Sanity-check with `default` and `with-form` captures before/after — they should be byte-identical for the md size.
- **Item 6 (alert backdrop-filter)**: on browsers without `backdrop-filter` support the rule degrades to the scrim alone — that's the current behavior, no regression. Verify by toggling DevTools-disable-`backdrop-filter` if convenient; otherwise trust the `-webkit-` prefix + the standard property.
- **Item 8 (body text scale token names)**: if the codebase uses different naming than `--zs-text-subheadline-size`/`-line`, grep `styles.css` for the actual subheadline/body type-scale tokens and substitute. Don't introduce new tokens.

## Report

End with:
- Files changed.
- Per-item confirmation (1–9) with file:line refs.
- Token purity grep results (5 greps).
- Build status (3 builds).
- A11y violations + incomplete sweep.
- Aria-wiring result.
- Contingencies that fired.
- 13 Dialog + 14 AlertDialog screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
