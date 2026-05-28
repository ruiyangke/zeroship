# Slice 4 visual polish — Checkbox + Switch + Radio (Phase 4)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `4e0a795a`, post-Slice-4-fix).

Closes the codex visual review of post-fix Slice 4 selection primitives. 6 findings (1 🔴 + 4 🟡 + 1 🟢). Codex transcript at `/tmp/claude-1000/.../tasks/besunxhax.output` lines 5440–5660; also at `/dev/shm/codex-slice4-visual-review.md` (in-sandbox).

## Goal

One commit closing the visual calibration items. Most findings center on **state legibility** — readonly looks like enabled, disabled-selected Radio is too faint, tinted unchecked Checkbox looks disabled-adjacent, size rhythm is too subtle. Plus the Required stories don't actually evidence the error state (screenshot capture gap).

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / no raw px / no raw `oklch(...)` in component CSS (foundation `styles.css` is allowed).
- HIG as principle but NOT in source.
- Glass-surface invariant.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bug (1)

**1. Required error state isn't captured in the Required screenshots.**
`sdks/ui/src/stories/Checkbox.stories.tsx:213`, `sdks/ui/src/stories/Radio.stories.tsx:235`.

Codex: "both required screenshots show only the pre-submit state. There is no red error line below the control." Story-only fix. The aria-wiring assertion already verifies the error fires (and we have it — `Checkbox Required — aria-invalid + Field.Error after submit` and `RadioGroup Required — aria-invalid + Field.Error after submit`); the visual evidence is the gap.

Two-part fix:
- Add a **`RequiredInvalid` companion story** for each (Checkbox + Radio) that lands in the post-submit state at story-render time. Use Storybook's `play` function (Storybook 8 supports it): on render, query the submit button via `data-testid`, click it, wait one frame, then resolve. The capture script will photograph the invalid state.
- Register both new stories in `check-storybook-a11y.mjs` (under the existing Slice 4 block) and `capture-checkbox-evidence.mjs` / `capture-radio-evidence.mjs`.

If the `play` approach hits Storybook autodocs friction, alternative: a tiny `defaultOpen=true` controlled-state story where the Form is rendered already in invalid state. Either path works — the capture must show red error text below the chip.

If a small CSS tweak surfaces while wiring this: `Field.css:85` `.zs-field__error { margin-block-start: var(--zs-space-half); }` (or similar token nudge) keeps the error from being too tight against the chip. Apply only if visually warranted after seeing the captured state.

### 🟡 Calibration / refinement (4)

**2. Readonly selected/on states are visually indistinguishable from normal selected/on.**
`Checkbox.css:201`, `Switch.css:178`, `Radio.css` (analog).

Codex: "the only visible signal is the story label 'Read only'. In a real settings form, a user would not know whether the selected value is editable until trying to interact with it."

Input already has a readonly palette (`--zs-input-bg-readonly`, `--zs-input-ink-readonly`). The selection primitives need an equivalent — a quiet "presented, not editable" treatment. Use a hairline ring + readonly surface tone instead of the full accent fill:

Add to `styles.css` foundation:
```css
:root {
  --zs-selection-hairline: 0.0625rem;
  --zs-selection-bg-readonly: var(--zs-input-bg-readonly);
  --zs-selection-ink-readonly: var(--zs-input-ink-readonly);
}
```

`Checkbox.css`:
```css
.zs-checkbox[data-readonly]:not([data-checked]):not([data-indeterminate]) {
  background-color: var(--zs-selection-bg-readonly);
  color: var(--zs-selection-ink-readonly);
}
.zs-checkbox[data-readonly][data-checked],
.zs-checkbox[data-readonly][data-indeterminate] {
  background-color: var(--zs-selection-bg-readonly);
  color: var(--zs-accent);
  box-shadow:
    inset 0 0 0 var(--zs-selection-hairline) var(--zs-accent),
    0 0 0 0 transparent;
}
```

`Switch.css`:
```css
.zs-switch[data-readonly]:not([data-checked]) {
  background-color: var(--zs-selection-bg-readonly);
}
.zs-switch[data-readonly][data-checked] {
  background-color: var(--zs-selection-bg-readonly);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-accent);
}
.zs-switch[data-readonly][data-checked] .zs-switch__thumb {
  background-color: var(--zs-accent);
}
```

`Radio.css`: add equivalent — readonly checked dot inside a readonly-toned chip with accent-color hairline ring.

**3. Tinted unchecked Checkbox reads close to disabled on Crystal.**
`Checkbox.css:142`.

Codex: "the tinted unchecked checkbox loses the border entirely and becomes a soft filled square… on the Crystal surface it lands close to the disabled unchecked visual." Fix: restore a visible hairline border on tinted variant.

```css
.zs-checkbox {
  --zs-checkbox-border-tinted: var(--zs-fill-secondary);
  --zs-checkbox-border-tinted-hover: var(--zs-fill);
}
.zs-checkbox--tinted {
  background-color: var(--zs-checkbox-bg-tinted);
  box-shadow:
    inset 0 0 0 var(--zs-selection-hairline) var(--zs-checkbox-border-tinted),
    0 0 0 0 transparent;
}
@media (hover: hover) {
  .zs-checkbox--tinted:not([data-disabled]):not([data-checked]):not([data-indeterminate]):hover {
    background-color: var(--zs-checkbox-bg-tinted-hover);
    box-shadow:
      inset 0 0 0 var(--zs-selection-hairline) var(--zs-checkbox-border-tinted-hover),
      0 0 0 0 transparent;
  }
}
```

**4. Disabled-selected Radio is weaker than disabled-selected Checkbox/Switch.**
`Radio.css:164,196`.

Codex: "the ring and dot are faint enough that the selected state nearly disappears."

```css
.zs-radio[data-disabled][data-checked] {
  box-shadow:
    inset 0 0 0 0.125rem var(--zs-label-quaternary),
    0 0 0 0 transparent;
}
.zs-radio[data-disabled] .zs-radio__indicator {
  background-color: var(--zs-label-quaternary);
}
```

If still too faint after capture, step to `var(--zs-label-tertiary)` to match the disabled Checkbox glyph strength. Note the contingency in a CSS comment.

**5. Size rhythm is too subtle — sm/md/lg mostly read as the same control.**
`_selection-row.css:39`, `Checkbox.css:57`, `Radio.css:48`, `Switch.css:56`.

Codex: "small/medium/large mostly read as the same control, with only the chip changing slightly. SelectionRow receives size classes but the shared row CSS does not consume them."

Wire `SelectionRow` size variants to label text size + row gap so the whole row scales, not just the chip:

`_selection-row.css`:
```css
.zs-checkbox-field--sm,
.zs-switch-field--sm,
.zs-radio-field--sm {
  gap: var(--zs-space-1);
}
.zs-checkbox-field--lg,
.zs-switch-field--lg,
.zs-radio-field--lg {
  gap: var(--zs-space-3);
}
.zs-checkbox-field--sm .zs-checkbox-field__text,
.zs-switch-field--sm .zs-switch-field__text,
.zs-radio-field--sm .zs-radio-field__text {
  font-size: var(--zs-text-subheadline-size);
  line-height: var(--zs-text-subheadline-line);
}
.zs-checkbox-field--lg .zs-checkbox-field__text,
.zs-switch-field--lg .zs-switch-field__text,
.zs-radio-field--lg .zs-radio-field__text {
  font-size: var(--zs-text-headline-size);
  line-height: var(--zs-text-headline-line);
}
```

If `--zs-control-h-{sm,md,lg}` tokens exist (32/40/48 control rhythm), bind chip sizes to half those values so the family inherits the Input rhythm:
```css
.zs-checkbox, .zs-radio {
  --zs-selection-size-sm: calc(var(--zs-control-h-sm) / 2);
  --zs-selection-size-md: calc(var(--zs-control-h-md) / 2);
  --zs-selection-size-lg: calc(var(--zs-control-h-lg) / 2);
}
```
Skip if those tokens don't exist — defer to a later cleanup. Document the decision inline.

### 🟢 Nit (1)

**6. Single Checkbox/Switch Field-label stories feel orphaned.**
`Checkbox.stories.tsx:127`, `Switch.stories.tsx:80`.

Codex: "a bold-ish Field label sits above a lone square/switch. The composition is semantically valid, but visually it reads like a section title with an unlabeled control underneath."

Story-only fix. Two approaches — codex preferred (a):
- (a) Keep the Field wiring story (it tests aria-wiring) but rename to `WithExternalLabel` to make intent clear. Add a separate `Inline` story showcasing the `label` prop pattern as the visually-recommended default for single booleans.
- (b) Add a horizontal Field-selection-row layout helper that fixes the orphan visual. Out-of-scope for Slice 4 polish — would belong in Slice 8 (Form + Fieldset) where horizontal-field-row layout is the natural place. Note for that brief.

Pick (a). Don't bloat Slice 4 with a Field layout extension.

## Files to modify

- `sdks/ui/src/styles.css` — foundation tokens for items 2 (readonly), 5 (selection-size aliases).
- `sdks/ui/src/components/Checkbox/Checkbox.css` — items 2 (readonly), 3 (tinted edge), 5 (size hooks).
- `sdks/ui/src/components/Switch/Switch.css` — item 2.
- `sdks/ui/src/components/Radio/Radio.css` — items 2, 4 (disabled-selected stronger), 5.
- `sdks/ui/src/components/_selection-row.css` — item 5 (gap + text size by row-size class).
- `sdks/ui/src/components/Field/Field.css` — item 1 (error margin nudge if needed after seeing post-submit capture).
- `sdks/ui/src/stories/Checkbox.stories.tsx` — item 1 (new RequiredInvalid story); item 6 (rename WithLabel→WithExternalLabel + new Inline story).
- `sdks/ui/src/stories/Switch.stories.tsx` — item 6.
- `sdks/ui/src/stories/Radio.stories.tsx` — item 1 (new RequiredInvalid).
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register new stories.
- `sdks/ui/scripts/capture-{checkbox,radio,switch}-evidence.mjs` — register new stories.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5: hex=0, px≤2 pre-existing, zs-blur=0, Card.Body=0, CardBody=0.
5. Raw `oklch(` grep in component CSS: 0.
6. A11y clean across all stories (96 baseline + ~3-4 new = ~99-100).
7. Aria-wiring 33 PASS + 2 SKIP + 0 FAIL (no logic changes; story renames don't affect existing assertions — update aria-wiring's storyId references if the renames touch the assertion stories).
8. Re-capture all Slice 4 PNGs.

## Contingencies

- **Item 1 (`play` function for Required Invalid)**: Storybook 8 supports `play` with `@storybook/test`'s `userEvent` + `expect`. If the `play` approach doesn't render in `build-storybook` capture (some Storybook setups skip `play` in static), use the controlled-state alternative — render a Form with submission state already at "invalid" via a small wrapper component.
- **Item 5 (control-rhythm tokens)**: grep `--zs-control-h-` in `styles.css`. If absent, skip the chip-size-derivation part and only ship the row-gap + label-size scaling. Document inline.
- **Item 4 (disabled Radio strength)**: if `--zs-label-quaternary` lands too faint after capture, escalate to `--zs-label-tertiary`. Document the escalation inline with a CSS comment.
- **Aria-wiring assertion stability**: items 6 (story rename `WithLabel` → `WithExternalLabel`) WILL break the existing assertion `Checkbox WithLabel — label click toggles`. Update the assertion to reference `components-checkbox--with-external-label` (or whatever the kebab-case becomes). Verify all 33 PASS still hold post-rename.

## Report

End with:
- Files changed.
- Per-item confirmation (1–6) with file:line refs.
- Token purity grep results (5 standard + raw-oklch-in-component grep).
- Build status (3 builds).
- A11y story count + clean status.
- Aria-wiring counts.
- Contingencies that fired.
- 29 (or 31 with new RequiredInvalid stories) screenshot paths — confirm RequiredInvalid actually shows red error text.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
