# Slice 6 visual polish — Select + Combobox + Autocomplete

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `f56bb301`).

Closes codex visual review: 2 🔴 + 3 🟡. Codex transcript at `/tmp/.../bgrtj7300.output:5455-5587`.

Plus 2 coverage gaps caught by codex that are story-state, not component CSS — fix in the same pass for honest evidence.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / `oklch()` in component CSS.
- Glass-surface invariant + forced-colors mappings.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bugs (2)

**1. Combobox/Autocomplete chevron doesn't rotate when popup is open.**
`Combobox.css:146`, `Autocomplete.css:182`. Select rotates (`Select.css:175`); the family reads as two different open-state languages.

```css
.zs-combobox-input-group__icon,
.zs-autocomplete-input-group__icon {
  transition: transform var(--zs-motion-fast) var(--zs-motion-ease);
}
.zs-combobox-input-group[data-popup-open] .zs-combobox-input-group__icon,
.zs-autocomplete-input-group[data-popup-open] .zs-autocomplete-input-group__icon {
  transform: rotate(180deg);
}
@media (prefers-reduced-motion: reduce) {
  .zs-combobox-input-group__icon,
  .zs-autocomplete-input-group__icon { transition: none; }
}
```

If `[data-popup-open]` isn't emitted on Combobox's input-group (it's emitted on the trigger button for Select), check Base UI's combobox state — fall back to a `:focus-within`-derived class or wrap the icon in a `data-attr`-bearing host.

**2. Long popups clip against viewport bottom.**
`Select.css:222`, `Combobox.css:236`. LongList captures both run off-screen. Brief said popup max-height `min(50vh, 28rem)` — appears not applied or the dvb sums break on tall stories.

Fix — cap max-block-size on the popup using dvb units (dynamic viewport):
```css
.zs-select-popup,
.zs-combobox-popup,
.zs-autocomplete-popup {
  max-block-size: min(50dvb, 24rem);
}
```

Don't introduce a new foundation token for this — too niche; inline the calc. (Codex's suggested `--zs-popover-max-block` recipe is more complex than needed; the simpler `min(50dvb, 24rem)` matches the brief's original cap.)

### 🟡 Calibration (3)

**3. Popup sheets need the promised inset rim.**
`Select.css:205`, `Combobox.css:223`.

Outer dialog shadow alone is soft on Crystal. Add an inset hairline matching the Selection/Toggle rim language:
```css
.zs-select-popup,
.zs-combobox-popup,
.zs-autocomplete-popup {
  box-shadow:
    var(--zs-shadow-dialog),
    inset 0 0 0 var(--zs-selection-hairline) var(--zs-separator);
}
```

**4. Select group labels too loud.**
`Select.css:336`.

Uppercase + tracking compete with option text. Drop both, use caption-1 scale + label-secondary tone.

```css
.zs-select-group-label {
  padding-block: var(--zs-space-half);
  padding-inline: var(--zs-space-2);
  font-size: var(--zs-text-caption-1-size);
  line-height: var(--zs-text-caption-1-line);
  font-weight: var(--zs-text-caption-1-weight);
  color: var(--zs-label-secondary);
  text-transform: none;
  letter-spacing: 0;
}
```

If `--zs-text-caption-1-*` tokens don't exist in `styles.css`, fall back to `--zs-text-subheadline-*` + `font-weight: 500`.

**5. Option text overindented by checkmark gutter.**
`Select.css:257`, `Combobox.css:264`.

Gap is too wide; rows read optically centered rather than list-aligned. Tighten:
```css
.zs-select-item,
.zs-combobox-item {
  grid-template-columns: var(--zs-space-4) 1fr;
  gap: var(--zs-space-1);
}
.zs-select-item__indicator,
.zs-combobox-item__indicator {
  inline-size: var(--zs-space-4);
  block-size: var(--zs-space-4);
}
```

### Coverage fixes (2 — story state, not component)

**6. Combobox Multiple story captures empty value — no chips visible.**
`stories/Combobox.stories.tsx:81`. Initialize `value` to e.g. `["apple", "orange"]` so the captured PNG actually shows chips and dispatches the chip-label-resolution code.

**7. Required stories capture pre-submit state — Field.Error not visible.**
Same pattern as Slice 4 visual polish item 1. Add `RequiredInvalid` companion stories (or use the existing useEffect+RAF auto-submit pattern Slice 4 introduced) for Select + Combobox + Autocomplete Required.

Register the new RequiredInvalid stories in `check-storybook-a11y.mjs` + capture scripts.

## Files to modify

- `sdks/ui/src/components/Select/Select.css` — items 2, 3, 4, 5.
- `sdks/ui/src/components/Combobox/Combobox.css` — items 1, 2, 3, 5.
- `sdks/ui/src/components/Autocomplete/Autocomplete.css` — items 1, 2, 3.
- `sdks/ui/src/stories/Select.stories.tsx` — item 7.
- `sdks/ui/src/stories/Combobox.stories.tsx` — items 6, 7.
- `sdks/ui/src/stories/Autocomplete.stories.tsx` — item 7.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register the 3 new RequiredInvalid stories.
- `sdks/ui/scripts/capture-{select,combobox,autocomplete}-evidence.mjs` — register the new stories.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5 + raw `oklch(` in component CSS = 0.
5. A11y clean for 153 stories (150 + 3 new RequiredInvalid).
6. Aria-wiring 52 PASS + 2 SKIP + 0 FAIL (unchanged — no logic changes).
7. Re-capture all Slice 6 PNGs. Confirm:
   - Combobox + Autocomplete chevron points UP when popup is open.
   - LongList popups visibly bounded with rounded corner intact.
   - Combobox Multiple capture shows chips with proper labels.
   - RequiredInvalid captures show red error text below trigger.

## Contingencies

- **Item 1 chevron rotation**: if Combobox's `data-popup-open` lives on the trigger button only (not InputGroup), wrap the chevron icon in a `[data-popup-open]`-bearing element or use a parent selector `.zs-combobox-input-group:has([aria-expanded="true"])`. Decide inline.
- **Item 4 group-label tokens**: grep `styles.css` for `--zs-text-caption-1-*`. If absent, fall back per the recipe.
- **Item 7 RequiredInvalid**: reuse Slice 4's useEffect+RAF+submitRef pattern (commit `3c650863` Checkbox/Radio RequiredInvalid stories). Storybook's `play` function isn't available without `@storybook/test`.

## Report

End with files changed; per-item confirmation (1–7) with file:line; token purity; build status; a11y count; aria-wiring counts; contingencies fired; ~33 screenshot paths; one taste note; "I did NOT commit, push, or merge."
