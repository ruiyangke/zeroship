# Slice 6 review fixes — Select + Combobox + Autocomplete

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `99a3bc42`).

Closes dual code review of Slice 6. Codex (4🔴+4🟡) + Claude (3🔴+2🟡), deduped to **7 🔴 + 5 🟡 = 12 items**. Critical: Select Multiple story fails `tsc --noEmit`, Select still in modal mode (mounts internal backdrop + scroll lock — brief said popover-feel), forced-colors specificity regression (Slice 5 lesson repeat), caller `aria-*` props land on InputGroup not Input.

## Hard constraints

- Pre-launch, no back-compat. Type rewrites allowed.
- `--zs-*` tokens only.
- Forced-colors specificity must match base-rule specificity (proven from Slice 5).
- Real-path aria-wiring with actual filter assertions.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (7)

**1. Select Multiple discriminated union doesn't narrow — `Multiple` story fails `tsc --noEmit`.**
`Select.tsx:429`, `stories/Select.stories.tsx:204`.

Codex: TypeScript infers `SelectProps<string[]>` from the `value={["apple"]}` setter — so the multiple branch expects `readonly string[][]` and `onValueChange` receives `string[][]`. The discriminated union doesn't help inference because `Value` is locked before `multiple` is read.

Fix: separate overloads on the call signature. Mirror the Toggle.Group Slice-5 fix shape but with overloads, not union:
```tsx
export interface SelectComponent {
  <Value>(props: SelectSingleProps<Value> & React.RefAttributes<HTMLDivElement>): React.JSX.Element;
  <Value>(props: SelectMultipleProps<Value> & React.RefAttributes<HTMLDivElement>): React.JSX.Element;
  Item: typeof SelectItem;
  // ...
}
```

Add a type-only regression in `sdks/ui/src/components/Select/type-tests.ts` mirroring the story call site so `pnpm exec tsc --noEmit` would fail pre-fix. Same shape for Combobox (Combobox already has the union; verify it also doesn't regress; add overloads if so).

**2. Select still uses Base UI modal mode by default — internal backdrop + scroll lock.**
`Select.tsx:225`.

Brief contingency said "Backdrop SKIPPED for all three (popover-feel, not modal-feel)". But Base UI's `Select.Root` defaults `modal: true`, which mounts an internal backdrop AND scroll-locks the page. The wrapper never passes `modal={false}`, so the default is silently wrong.

Fix: default the wrapper's `modal` to `false`. Preserve explicit consumer override (`modal` prop in the union). Update the inline comment that claims "no Backdrop" to match.

**3. Forced-colors specificity regression — Slice 5 lesson repeat.**
`Select.css:101+`, `Combobox.css:76+`, `Autocomplete.css` (analog).

State selectors like `.zs-select-trigger:not([disabled]):not([data-popup-open]):hover`, `.zs-combobox-input-group:not([data-disabled]):not([data-focused]):hover`, `.zs-combobox-chip__remove:hover` outrank the lower-specificity `.zs-X` rules inside `@media (forced-colors: active)`. Token-derived backgrounds escape the system-color palette under high-contrast mode.

Fix: mirror EVERY state selector inside the forced-colors block at equal/higher specificity. Apply the Slice 5 fix pattern verbatim — pull each non-forced state rule's selector and re-declare with Canvas/CanvasText/Highlight/HighlightText/GrayText inside the @media block.

**4. Caller `aria-*` props land on InputGroup (`div role="group"`), not the focusable Input.**
`Combobox.tsx:166`, `Autocomplete.tsx:106`.

Brief contingency said aria-* "sifted onto the visible Trigger (Select) / InputGroup (Combobox, Autocomplete)". But the InputGroup is a `<div role="group">` — the actual focusable control is `BaseCombobox.Input` / `BaseAutocomplete.Input`. A consumer-provided `aria-label` / `aria-labelledby` / `aria-describedby` on `<Combobox>` therefore never reaches the focusable input.

Fix: sift aria-* (`aria-label`, `aria-labelledby`, `aria-describedby`) into a separate bucket and stamp them on the `<BaseCombobox.Input>` / `<BaseAutocomplete.Input>`. Keep `data-testid` on the group if convenient (it's the visible target for E2E), or move it to the input too for consistency.

**5. `Combobox.Chip` advertises a `value` prop that Base UI ignores; leaks onto DOM.**
`Combobox.tsx:384-415`.

`ComboboxChipProps` declares `value?: unknown` with doc-comment "passed up to Base UI for removal correlation." Base UI's `<ComboboxChip>` props are just `BaseUIComponentProps<'div', ComboboxChipState>` — no `value`. Removal is keyed by composite-list `index`, not `value`. Wrapper destructures `removeLabel` but NOT `value`, so it leaks into `...rest` and spreads onto the `<div>` as a stray `value="apple"` attribute.

Fix: drop the `value` field from `ComboboxChipProps` entirely. Update the JSDoc to clarify removal is index-based. Story call site (`stories/Combobox.stories.tsx`) just uses `key={v}` and `children={chipLabel(v)}` — no API change for consumers.

**6. Multi-mode chips render `String(v)` regardless of Value generic.**
`Combobox.tsx:226-235`.

For `Combobox<{id, name}>` with `items={[{value: {id:'a', name:'Apple'}, label: 'Apple'}]}`, the chip currently reads `"[object Object]"`. Generic API + string-only runtime is a contract drift.

Fix: resolve chip labels from the `items` array by value lookup. Add a `getChipLabel?: (value: Value) => ReactNode` escape hatch for callers without an `items` array. Default fallback: walk `items` to find matching value; if no match, use `String(v)` (current behavior, just as a last resort). Document inline.

**7. ESC-focus-restore assertion on Combobox leg is broken — only checks focus, not popup closed.**
`check-aria-wiring.mjs:1410-1434` (assertion 42, Combobox branch).

Codex + claude agree. Select leg correctly asserts `popupClosed && focused`; Combobox + Autocomplete only assert `focused`. If ESC were a no-op but focus stayed, the assertion would pass spuriously.

Fix: mirror the Select pattern. After `await page.keyboard.press("Escape")`, query a known popup item AND assert it's no longer visible AND focus stayed on the input. Apply to both Combobox AND Autocomplete branches.

### 🟡 Calibration / API (5)

**8. Single-select `onValueChange` drops the `null` case Base UI emits.**
`Select.tsx:132`, also applies to Combobox single-mode.

Base UI types `onValueChange` as `(value: Value | null, …) => void` for single mode (clear paths, item-disappears paths). Wrapper narrows to `(value: Value, …)`. Unsound.

Fix: `SelectSingleProps.onValueChange: ((value: Value | null, eventDetails) => void)`. Same for Combobox.

**9. Autocomplete loses TS inference via `as unknown` cast.**
`Autocomplete.tsx:143-147`.

`<BaseAutocomplete.Root {...(rootRest as unknown as Record<string, unknown>)} />` strips all Root prop typing inside the wrapper. Select and Combobox use `as never` per-field — narrower and honest.

Fix: destructure `value`, `defaultValue`, `onValueChange`, `items` off `rootRest` and forward with `as never` per field. Document the overload reason.

**10. Select trigger accessible-name fallback conflicts with Field.Label wiring.**
`Select.tsx:241-243`.

Emits `aria-label={placeholder}` even when Field auto-wires `aria-labelledby`. Per ARIA spec `aria-labelledby` wins, but the redundant `aria-label` is dead weight that drifts if the placeholder changes.

Fix: skip the aria-label fallback when `useFieldContext()` is non-null. Pull the consumer aria-label only — Field's auto-wiring handles the rest.

**11. Autocomplete filter assertion doesn't prove filtering.**
`check-aria-wiring.mjs:1356`.

Types `hello@` which matches every `EMAIL_DOMAINS` fixture entry, then checks the committed value matches an email regex. A broken filter would pass.

Fix: type a narrowing query like `gmail` and assert a non-matching item (e.g., `yahoo` domain) is no longer visible. Mirror the Combobox Basic assertion's structure.

**12. Autocomplete ESC assertion doesn't verify popup closes.**
Covered by fix 7 above. Apply the same Select-style pattern to the Autocomplete branch.

## Regression test mandate

The 7 🔴s and assertion-related 🟡s need real regression tests:

- **Item 1**: `Select/type-tests.ts` with `@ts-expect-error` directives proving the Multiple story shape compiles. Pre-fix: `tsc --noEmit` fails (codex verified).
- **Item 2**: Aria-wiring assertion that verifies `body.style.overflow` is NOT `hidden` while a Select popup is open (modal mode would lock scroll). Pre-fix: would catch the modal default.
- **Item 3**: Already covered by Slice 5's `ForcedColorsHover` pattern — extend to Select + Combobox + Autocomplete (3 new assertions).
- **Item 4**: Aria-wiring assertion that an `aria-label="Custom"` on `<Combobox>` propagates to the `<input>` (use `page.locator('input[aria-label="Custom"]')`).
- **Item 5**: Snapshot the DOM of a chip and assert no `value=` attribute on the `<div>`.
- **Items 7, 11, 12**: Update the existing aria-wiring assertions to be honest about what they verify.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm exec tsc -p tsconfig.json --noEmit --pretty false` — clean (this catches item 1).
3. `pnpm --filter @zeroship/ui build-storybook` — green.
4. `pnpm --filter zeroship-builder build` — green.
5. Token purity + raw `oklch(` in component CSS = 0.
6. A11y clean for 148 stories.
7. Aria-wiring 44 + new regression assertions = ~50 PASS + 2 SKIP + 0 FAIL.
8. Re-capture 30 PNGs.

## Files to modify

- `sdks/ui/src/components/Select/Select.tsx` — items 1 (overloads), 2 (modal=false default), 8 (null in callback), 10 (Field-context aria-label skip).
- `sdks/ui/src/components/Select/Select.css` — item 3 (forced-colors mirror).
- `sdks/ui/src/components/Select/type-tests.ts` — NEW (item 1 regression).
- `sdks/ui/src/components/Combobox/Combobox.tsx` — items 1 (overloads if needed), 4 (aria-* → input), 5 (drop Chip.value), 6 (chip label resolution), 8 (null).
- `sdks/ui/src/components/Combobox/Combobox.css` — item 3.
- `sdks/ui/src/components/Autocomplete/Autocomplete.tsx` — items 4 (aria-* → input), 9 (per-field cast).
- `sdks/ui/src/components/Autocomplete/Autocomplete.css` — item 3.
- `sdks/ui/scripts/check-aria-wiring.mjs` — items 7, 11, 12 + new modal-scroll assertion + new forced-colors assertions for the 3 components + aria-forwarding assertion.

## Contingencies

- **Item 1 (Select overloads)**: if overload-resolution still picks the wrong branch when `value={[...]}` is passed without `multiple={true}`, the call-site is incorrect — TypeScript should error. Add a stricter type test that exercises both branches.
- **Item 2 (modal=false)**: verify the Select popup still positions correctly via Floating UI without modal mode. Should — Base UI uses Floating UI regardless of modal.
- **Item 6 (chip label)**: when `items` isn't provided to Combobox, the chip resolver falls back to `String(v)`. Document inline.

## Report

End with files changed; per-item confirmation (1–12) with file:line refs; token purity; `tsc --noEmit` status (new gate); build status; a11y count; aria-wiring counts; contingencies fired; screenshot paths; one taste note; "I did NOT commit, push, or merge."
