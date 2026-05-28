# Slice 4 code review fixes — Checkbox + Switch + Radio

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `d20915ab`).

Closes the two parallel code reviews of Slice 4 — codex (4 🔴 + 2 🟡) + claude reviewer (3 🔴 + 5 🟡 + 2 🟢), deduped to 11 fix items. Both reviewers converged on the same three critical issues: asChild unusable, focus ring keyed to hidden input, hit-target floor only on the wrapper row. Codex added a fourth 🔴 on Switch CSS token-purity. Transcripts:

- codex: `/tmp/claude-1000/.../tasks/bdrzmuo1a.output` lines 10693–10812.
- claude reviewer: `/tmp/claude-1000/.../tasks/a14c44e0f794cf9b9.output` (full markdown report inline).

## Hard constraints (unchanged)

- Pre-launch, no back-compat. Rename / break / restructure freely.
- `--zs-*` tokens only. No raw hex. No raw `oklch(...)` literals in component CSS. No raw px (incl. comments).
- HIG as principle but NOT in source.
- Glass-surface invariant: opaque `background-color` on every visible element.
- `@media (forced-colors: active)` mappings reach every focusable surface.
- RTL via logical properties.
- Real-path aria-wiring tests (no shims, no unit stubs).
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (4)

**1. `asChild` is exposed but unusable — no consumer can supply a child element.**
`Checkbox.tsx:60-61, 183-195`, `Switch.tsx:52, 113-124`, `Radio.tsx:160-163, 248-259`.

Both reviewers caught this. The public prop interfaces `extend Omit<BaseXRootProps, "className" | "render" | "children">` — `children` is explicitly omitted, so consumers can never supply an element. The render callback does `<Slot {...props} … />` without explicitly passing a child; what reaches `Slot` is `props.children` — the internal `<BaseCheckbox.Indicator>` / `<BaseSwitch.Thumb>` scaffolding. Slot would `cloneElement` the wrong inner element with the focusable-chip's class/data attrs. No stories exercise the path; the bug ships invisibly.

**Decision: REMOVE `asChild` from Checkbox / Switch / Radio.** Pre-launch, no back-compat — the cleaner move than designing-around-the-bug. Rationale: these three primitives are *visual chips with a hidden input*, not button-shaped surfaces. Unlike Dialog.Close / AlertDialog.Cancel (which are Buttons with a close handler — an obvious `asChild`-with-Slot pattern), the selection primitives have no natural "swap the whole element" use case. Consumers who want a custom-styled checkbox can write `<Field><Field.Label><Checkbox /></Field.Label></Field>` and style the chip via `className` — Base UI's `[data-checked]` / `[data-indeterminate]` data attributes do the work.

Concrete moves:
- Drop the `asChild?: boolean` prop from `CheckboxProps` / `SwitchProps` / `RadioProps`.
- Remove the `if (asChild) { … }` branches in all three render functions.
- Remove the `Slot` import where it's the only user of `_slot.ts` in the file.
- Update the file-header comment in each component to drop the asChild bullet (or replace with "the chip itself takes `className` for custom styling").
- Keep the brief's principle 6 ("Hit target ≥ 1.75rem regardless of visual size") intact — that's handled by fix item 3, not by asChild.

(If a future slice adds asChild back for one of these — say, a styled custom radio button — that slice can design it properly with `children: ReactElement` + Slot + a test story. Today, no.)

**2. Focus ring never paints on the canonical `<Field><Field.Label>…</Field.Label><Chip /></Field>` usage.**
`Checkbox.css:170-179, 281-285`, `Switch.css:136-140, 265-269`, `Radio.css:141-150, 229-233`.

The focus-ring selector keys off `.zs-X-field:has(> input:focus-visible) .zs-X`. The `.zs-X-field` row exists ONLY when the component's own `label` prop is set; the brief's canonical pattern is to wrap in `<Field>` and let `Field.Label` provide the label. In that pattern, the bare chip's focused state shows no ring — WCAG 2.4.7 violation. The forced-colors fallback has the same blind spot.

Codex pointed out a second issue: Base UI emits the chip as a focusable `<span>` with `tabIndex=0` AND a hidden `<input tabIndex=-1>` — the focus actually lives on the chip itself, not the hidden input. Selecting via the hidden-input chain is doubly wrong.

Fix — style the chip root directly:
```css
.zs-checkbox:focus-visible {
  outline: 0.125rem solid var(--zs-button-focus-ring-color);
  outline-offset: 0.0625rem;
}
@media (forced-colors: active) {
  .zs-checkbox:focus-visible {
    outline-color: Highlight;
  }
}
```
Mirror for `.zs-switch:focus-visible` and `.zs-radio:focus-visible`. Drop the `.zs-X-field:has(> input:focus-visible)` selectors entirely (they were the wrong target).

Regression test: add a Playwright assertion that focuses the chip via keyboard (`Tab` into a bare `<Field><Field.Label>…</Field.Label><Checkbox /></Field>` story) and asserts the chip's computed `outline-width` is `2px` (the rendered value of `0.125rem` at default font-size). One assertion per primitive (3 new aria-wiring assertions).

**3. Hit-target floor (≥ 1.75rem) only applies to the in-component label row.**
`Checkbox.css` `.zs-checkbox-field` (1.75rem floor + 2.75rem on `pointer: coarse`), `Switch.css` `.zs-switch-field`, `Radio.css` `.zs-radio-field`.

Same shape as #2. The floor exists on the `.zs-X-field` wrapper but bare chips are 1rem (sm) / 1.125rem (md) / 1.25rem (lg) — all below the floor. The brief's principle 6 ("Hit target ≥ 1.75rem regardless of visual size") and the Checkbox.tsx file-header comment claim the floor is honored. It isn't.

Fix — apply the floor on the chip ROOT using a pseudo-element overlay so the visual chip stays its drawn size but the tap rect is ≥ 1.75rem:

```css
.zs-checkbox {
  position: relative;  /* anchor for ::before */
}
.zs-checkbox::before {
  content: "";
  position: absolute;
  inset: 50% 50% auto auto;
  translate: 50% -50%;
  min-inline-size: var(--zs-hit-min, 1.75rem);
  min-block-size: var(--zs-hit-min, 1.75rem);
  /* invisible — extends the hit rect without changing visual chip size */
}
@media (pointer: coarse) {
  .zs-checkbox::before {
    --zs-hit-min: 2.75rem;
  }
}
```

Same pattern for `.zs-switch` and `.zs-radio`. Verify with a unit test: query `getBoundingClientRect()` on the chip's `::before` (or hit-test at the chip's outer-1.75rem rect to confirm the click reaches the input). Aria-wiring can simulate a `page.click()` at a slight offset from the chip center and assert the state changed.

Add 3 aria-wiring assertions (one per primitive): click at `(chipCenterX - 0.6rem, chipCenterY)` — outside the visual chip but inside the hit floor — and assert state flipped.

**4. Switch CSS breaks token / glass-surface invariants.**
`Switch.css:63-64` (`--zs-switch-thumb-shadow` uses raw `oklch(0 0 0 / 0.18)` and `oklch(0 0 0 / 0.04)`); `Switch.css:149` (`background-color: color-mix(in oklch, var(--zs-accent) 45%, transparent)` — mixing with transparent on a glass surface).

Two-part fix:
- Replace the raw `oklch(0 0 0 / α)` shadow values with `color-mix(in oklch, var(--zs-label) Nα%, transparent)` so the shadow inherits the theme's label and de-shifts on dark themes when those land. Recipe:
  ```css
  --zs-switch-thumb-shadow:
    0 0.0625rem 0.125rem color-mix(in oklch, var(--zs-label) 18%, transparent),
    0 0 0 0.0625rem color-mix(in oklch, var(--zs-label) 4%, transparent);
  ```
- Replace the disabled-checked `transparent`-mix with an opaque background so the glass-surface invariant holds:
  ```css
  .zs-switch[data-disabled][data-checked] {
    background-color: color-mix(in oklch, var(--zs-accent) 45%, var(--zs-fill-secondary));
  }
  ```
  Same principle as the Button-disabled-destructive fix shipped in Phase 2.A item 1 (gray-with-accent-hint, not faded-accent).

Verify with the token purity x5 grep — the regex `[0-9]+px` should still be ≤2 pre-existing, but a separate manual `oklch\(` grep in component CSS should now return 0 in `Switch.css` (the global `styles.css` is allowed to define raw `oklch()` — those are foundation tokens; the constraint applies to component-level CSS).

### 🟡 Calibration / API (5)

**5. Ref typing claims `HTMLButtonElement` for `<span>` roots.**
`Checkbox.tsx:139`, `Switch.tsx:81`, `Radio.tsx:204`.

Base UI's `CheckboxRoot` / `SwitchRoot` / `RadioRoot` are typed as `HTMLElement` (verified in `node_modules/.pnpm/@base-ui+react@1.5.0/.../CheckboxRoot.d.ts:9`). The wrappers type their `forwardRef` as `HTMLButtonElement`, which is wrong — Base UI renders `<span>`. Misleads consumers; `inputRef.current.disabled` (a button-only property) would type-check but return undefined.

Fix — type as `HTMLElement` (or `HTMLSpanElement` since we know Base UI renders span). Since item 1 removes `asChild`, there's no need for a wider type union. Pick `HTMLSpanElement` so consumers see the precise rendered element type.

**6. Indeterminate glyph follows the wrapper prop, not Base UI's computed state.**
`Checkbox.tsx:202` (`indeterminate ? <IndicatorMinus /> : <IndicatorCheck />`).

If the Checkbox is inside a `CheckboxGroup` and Base UI computes `data-indeterminate` from group state (the parent-of-children pattern), but the wrapper prop `indeterminate` isn't explicitly set, the wrapper still renders the checkmark. The CSS opacity-fade comment in `Checkbox.tsx:198-201` ("we want the indicator container to persist") also doesn't match the code (only one glyph renders at a time).

Fix — render both glyphs simultaneously, let CSS swap visibility from data attributes on the chip:
```tsx
function CheckboxIcon() {
  return (
    <>
      <IndicatorCheck data-glyph="check" />
      <IndicatorMinus data-glyph="minus" />
    </>
  );
}
```
CSS:
```css
.zs-checkbox [data-glyph] { opacity: 0; transition: opacity 120ms ease-out; }
.zs-checkbox[data-checked]:not([data-indeterminate]) [data-glyph="check"] { opacity: 1; }
.zs-checkbox[data-indeterminate] [data-glyph="minus"] { opacity: 1; }
```
Also pass `keepMounted` on the BaseCheckbox.Indicator so the indicator span persists across state flips and the fade-in/out actually animates.

Update the file-header comment to match the implementation. Add a regression story `IndeterminateFromGroup` that nests Checkboxes in a `<CheckboxGroup>` parent and verifies the parent shows the minus when children are partially checked — without the wrapper having an explicit `indeterminate` prop.

**7. `required` doesn't inherit from Field context (asymmetric with Input).**
`Checkbox.tsx`, `Switch.tsx`, `Radio.tsx`.

Input reads `required` from `useFieldContext()` (`Input.tsx:211`). Selection primitives don't — only `size` + `disabled` cascade. Consequence: `<Field required>…<Checkbox /></Field>` does NOT mark the Checkbox as required. The Required stories (`Checkbox.stories.tsx:198-206` etc.) set `required` on BOTH the Field and the chip, masking the gap.

Fix — add `required` to the same context plumbing as `size` and `disabled`. Use the existing `useFieldContext()` if it carries `required` (it should — Field's TypeScript type extends BaseField.Root which includes `required`). Cascade: prop wins, then context, then default false. Remove the redundant `required` from the stories' chips (test the inheritance explicitly).

**8. RadioGroup forwards generic `T` via lossy cast.**
`Radio.tsx:127-131` (`{...(rest as ComponentPropsWithoutRef<typeof BaseRadioGroup>)}`).

The cast drops the `<T>` typing on `value` / `defaultValue` / `onValueChange` going into Base UI. The outer public API still infers correctly for callers but the inner call uses `any`. Refactor: destructure `value`, `defaultValue`, `onValueChange` out of `rest`, declare with the typed `<T>` signature, pass as named props alongside `...rest`. Drop the cast.

**9. Hidden-input direct-sibling selectors are fragile.**
`Checkbox.css:118-123`. Codex flagged this as low-priority; the canonical case works today. With items 2 + 3 above removing the hidden-input chain from focus + hit, this entire selector family becomes dead code — delete it.

### 🟢 Nits (2)

**10. Three near-identical label-row JSX blocks beg for a shared helper.**
`Checkbox.tsx:210-227`, `Switch.tsx:130-147`, `Radio.tsx:265-282`. The blocks differ only in the `zs-X` class prefix. Hoist a `SelectionRow` internal helper into `sdks/ui/src/components/_selection-row.tsx`:

```tsx
interface SelectionRowProps {
  base: "checkbox" | "switch" | "radio";
  size: "sm" | "md" | "lg";
  disabled?: boolean;
  fieldClassName?: string;
  fieldProps?: HTMLAttributes<HTMLLabelElement>;
  children: ReactNode;  // chip + label text
}
export function SelectionRow({ base, size, disabled, fieldClassName, fieldProps, children }: SelectionRowProps) {
  return (
    <label
      {...fieldProps}
      className={classnames(`zs-${base}-field`, `zs-${base}-field--${size}`, fieldClassName, fieldProps?.className)}
      data-size={size}
      data-disabled={disabled || undefined}
    >
      {children}
    </label>
  );
}
```

Pull the focus-ring (now on the chip after fix 2) and hit-target floor (now on the chip after fix 3) out of the row CSS — the row should be purely a layout helper, not a hit-target source. Update each component's branch to use `<SelectionRow base="checkbox" …>{chip}<span className="zs-checkbox-field__text">{label}</span></SelectionRow>`.

**11. `process.env?.NODE_ENV` optional chain defeats Vite/esbuild static replacement.**
`Radio.tsx:219`. Replace with `process.env.NODE_ENV === "production"` (drop the optional chain — the `typeof process === "undefined"` guard above already handles the missing-process case).

Plus dedupe the Radio dev-warn across instances (module-level `let warned = false`, not per-mount `useRef`).

## Files to modify

- `sdks/ui/src/components/Checkbox/Checkbox.tsx` — items 1 (drop asChild), 5 (ref type), 6 (both-glyphs render), 7 (required cascade), 9 (delete dead selectors via CSS), 10 (use SelectionRow).
- `sdks/ui/src/components/Checkbox/Checkbox.css` — items 2 (focus ring on chip), 3 (hit floor on chip), 9 (delete dead selectors), 10 (move row layout to SelectionRow's CSS or keep here but slim it).
- `sdks/ui/src/components/Switch/Switch.tsx` — items 1, 5, 7, 10.
- `sdks/ui/src/components/Switch/Switch.css` — items 2, 3, 4 (token + opaque background), 10.
- `sdks/ui/src/components/Radio/Radio.tsx` — items 1, 5, 7, 8 (generic forwarding), 10, 11 (env check + dev-warn dedup).
- `sdks/ui/src/components/Radio/Radio.css` — items 2, 3, 10.
- `sdks/ui/src/components/_selection-row.tsx` — NEW (item 10).
- `sdks/ui/src/components/_selection-row.css` — NEW (item 10, shared row layout).
- `sdks/ui/src/styles.css` — add `@import "./components/_selection-row.css";` near the top.
- `sdks/ui/src/stories/Checkbox.stories.tsx` — drop redundant `required` from Required story (item 7); add `IndeterminateFromGroup` story (item 6).
- `sdks/ui/src/stories/Switch.stories.tsx` — drop redundant `required` if present.
- `sdks/ui/src/stories/Radio.stories.tsx` — drop redundant `required` from Required story.
- `sdks/ui/scripts/check-aria-wiring.mjs` — 6 new assertions:
  - 3 focus-ring (Checkbox, Switch, Radio: keyboard Tab → bare chip → computed outline-width === "2px").
  - 3 hit-target (Checkbox, Switch, Radio: click at chip-center + 0.6rem offset → state flipped).
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register `components-checkbox--indeterminate-from-group` story.
- `sdks/ui/scripts/capture-checkbox-evidence.mjs` — register the new story for capture.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5: hex=0, px ≤2 pre-existing, zs-blur=0, Card.Body=0, CardBody=0.
5. Plus an extra **raw `oklch\(` grep in component CSS** must return 0 (foundation `styles.css` is allowed; component CSS is not).
6. A11y clean across all 96 stories (95 baseline + 1 new IndeterminateFromGroup).
7. Aria-wiring: **32 PASS** + 2 SKIP + 0 FAIL (26 current + 6 new = 3 focus-ring + 3 hit-target).
8. Re-capture all 28 + 1 Slice 4 PNGs.

## Contingencies

- **Item 1 (drop asChild)**: if any story or downstream consumer accidentally uses `asChild` (grep `sdks/ui/src` for the literal string `asChild` to confirm), update those call sites. Pre-launch, no back-compat — the rename/break should be clean.
- **Item 2 (focus-ring selector change)**: verify with the existing `RadioGroup TwoOptions keyboard` aria-wiring assertion that ArrowDown still moves focus visibly. If Base UI's roving-focus implementation doesn't trigger `:focus-visible` consistently, fall back to `[data-focused]` (Base UI emits this).
- **Item 3 (hit-target overlay)**: the `::before` overlay must NOT block the click — `pointer-events: none` is ESSENTIAL or the chip itself becomes unclickable. Add it explicitly even though invisible elements default to clickable.
- **Item 6 (indeterminate both-glyphs)**: verify `keepMounted` on `<BaseCheckbox.Indicator>` keeps the wrapper span in the DOM during unchecked state. If Base UI's Indicator API doesn't accept `keepMounted` directly (the prop is documented on the Indicator: see `CheckboxIndicator.d.ts:23`), pass it through.
- **Item 7 (required cascade)**: if `useFieldContext()` doesn't currently expose `required`, plumb it from Field.Root's prop into the context value object. The Input precedent (`Input.tsx:211`) confirms the field is meant to carry it.

## Report

End with:
- Files changed.
- Per-item confirmation (1–11) with file:line refs.
- Token purity grep results (5 standard + 1 oklch-in-component-CSS).
- Build status (3 builds).
- A11y story count + clean status.
- Aria-wiring counts (PASS / SKIP / FAIL).
- Contingencies that fired.
- 29 Slice 4 screenshot paths (28 existing + 1 new IndeterminateFromGroup).
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
