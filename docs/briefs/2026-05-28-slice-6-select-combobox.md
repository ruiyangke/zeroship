# Slice 6 — Select + Combobox + Autocomplete (popover-anchored)

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `59cd0dbd`, post Slice-5-complete).

Three popover-anchored input surfaces wrapping Base UI's `select`, `combobox`, `autocomplete` headless primitives. All three share the Dialog Portal/Popup/Backdrop pattern from Slice 3 + Floating UI anchoring.

## Goal

`@zeroship/ui` ships:

- **Select** — dropdown from a fixed option list. Single or multiple selection. Trigger looks like Input; popup looks like Dialog (light glass + opaque base). Items keyboard-navigable with arrow keys.
- **Combobox** — typeahead + selection hybrid. Type to filter; commit picks one. Multi-mode shows selected items as chips.
- **Autocomplete** — pure suggestion list under an Input. Doesn't commit selections to a managed list; just offers completions for free text.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / `oklch()` in component CSS.
- Glass-surface invariant: opaque `background-color` + optional `backdrop-filter`.
- HIG as design principle but NOT in source (no "HIG"/"Apple" strings).
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- Real-path aria-wiring via Playwright.
- DO NOT commit, push, or merge.

## Design principles encoded (in source as rationale, never branded)

1. **Select is the "long option list" surface.** When >5 options or freeform-required → Combobox. <5 mutually-exclusive → use Toggle.Group (Slice 5) instead.
2. **Combobox is text input + filtered list + commit.** Type to filter; ↓/↑ navigate; Enter commits; ESC closes. The text input IS the trigger.
3. **Autocomplete is text input + suggestions, no commit.** Suggestions help, but the value the user types is the value. Useful for emails, URLs, search boxes.
4. **Popup uses Dialog's surface tokens.** Same `--zs-shadow-dialog`, same Crystal opaque base. Read as part of the same family as Dialog/AlertDialog.
5. **Trigger reads like Input.** Same height (`--zs-control-h-{sm,md,lg}`), same Field integration, same focus ring (`--zs-button-focus-ring-color`).
6. **Multi-select shows chips.** Selected items render as removable chips inside (Combobox) or below (Select) the trigger. Reuses Combobox.Chip primitives.

## API shape

### Select

```tsx
// sdks/ui/src/components/Select/Select.tsx
export interface SelectProps<Value, Multiple extends boolean = false>
  extends Omit<BaseSelectRootProps<Value, Multiple>, "render"> {
  /** Size — sm 32 / md 40 (default) / lg 48 — matches Input. */
  size?: "sm" | "md" | "lg";
  /** Visual variant — `default` filled / `outline` border-only. Mirrors Input. */
  variant?: "default" | "outline";
  /** Placeholder shown when no value selected. */
  placeholder?: string;
  /** Visually align the popup to the trigger's start/center/end. */
  align?: "start" | "center" | "end";
  className?: string;
}

// Namespace export
export const Select = ForwardedSelect as SelectComponent & {
  Item: typeof SelectItem;
  Group: typeof SelectGroup;
  GroupLabel: typeof SelectGroupLabel;
  Separator: typeof SelectSeparator;
};
```

Composition:
```tsx
<Select value={value} onValueChange={setValue}>
  <Select.Item value="apple">Apple</Select.Item>
  <Select.Group label="Citrus">
    <Select.Item value="orange">Orange</Select.Item>
    <Select.Item value="lemon">Lemon</Select.Item>
  </Select.Group>
</Select>
```

### Combobox

```tsx
export interface ComboboxProps<Value, Multiple extends boolean = false>
  extends Omit<BaseComboboxRootProps<Value, Multiple>, "render"> {
  size?: "sm" | "md" | "lg";
  variant?: "default" | "outline";
  placeholder?: string;
  /** Items — array of `{ value, label }` or render-prop. */
  items?: ReadonlyArray<{ value: Value; label: string }>;
  className?: string;
}

export const Combobox = ForwardedCombobox as ComboboxComponent & {
  Item: typeof ComboboxItem;
  Empty: typeof ComboboxEmpty;
  Chip: typeof ComboboxChip;
};
```

Multi-mode: returns `Value[]`; chips render inside the trigger.

### Autocomplete

```tsx
export interface AutocompleteProps<Value extends string = string>
  extends Omit<BaseAutocompleteRootProps<Value>, "render"> {
  size?: "sm" | "md" | "lg";
  /** Suggestion items. */
  items?: ReadonlyArray<{ value: Value; label?: string }>;
  className?: string;
}
```

Single-only — Autocomplete is freeform completion, not selection.

## Files to create

```
sdks/ui/src/components/Select/
  Select.tsx          (~350 lines — Root + Item + Group + GroupLabel + Separator)
  Select.css
  index.ts

sdks/ui/src/components/Combobox/
  Combobox.tsx        (~350 lines — Root + Item + Empty + Chip)
  Combobox.css
  index.ts

sdks/ui/src/components/Autocomplete/
  Autocomplete.tsx    (~250 lines — Root + Item)
  Autocomplete.css
  index.ts

sdks/ui/src/stories/
  Select.stories.tsx       (12 stories)
  Combobox.stories.tsx     (10 stories)
  Autocomplete.stories.tsx (8 stories)

sdks/ui/scripts/
  capture-select-evidence.mjs       (NEW)
  capture-combobox-evidence.mjs     (NEW)
  capture-autocomplete-evidence.mjs (NEW)
```

## Files to modify

- `sdks/ui/src/components/index.ts` — exports.
- `sdks/ui/src/index.ts` — type re-exports.
- `sdks/ui/src/styles.css` — `@import` for the 3 new CSS files.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register ~30 new story IDs.
- `sdks/ui/scripts/check-aria-wiring.mjs` — add 6 new assertions (see below).

## Story coverage matrix

### Select (12)

1. `Basic` — single-select with 4 options. `data-testid="select-basic"`.
2. `WithGroups` — Citrus / Berries grouped options + GroupLabels.
3. `AllSizes` — sm/md/lg row.
4. `AllVariants` — default vs outline.
5. `Multiple` — multi-select with checkmark indicators.
6. `Disabled` — whole select disabled.
7. `WithLabel` — wrapped in `<Field><Field.Label>…</Field.Label><Select/></Field>`.
8. `Required` — Field.Required + invalid-after-submit chain.
9. `LongList` — 50 options to verify scroll-area + keyboard ↓/↑ rovers.
10. `AlignStart/Center/End` — popup alignment to trigger.
11. `Placement` — popup above / below / auto.
12. `RTL`.

### Combobox (10)

13. `Basic` — type to filter 10 options.
14. `Multiple` — chip-based multi-select.
15. `AllSizes`.
16. `AllVariants`.
17. `Empty` — Combobox.Empty renders custom "No results" when filter excludes all.
18. `WithLabel`.
19. `Required`.
20. `LongList` — 100 items, filter narrows.
21. `Disabled`.
22. `RTL`.

### Autocomplete (8)

23. `Basic` — email-like suggestions.
24. `AllSizes`.
25. `WithLabel`.
26. `Empty` — no suggestions visible until user types.
27. `Disabled`.
28. `RTL`.
29. `WithDescription` — Field.Description below.
30. `LongList` — 50 suggestions.

## Aria-wiring assertions (6 new)

1. **Select Basic — keyboard ↓ navigates AND Enter commits.** Click trigger → ↓ → Enter; assert value === "orange" + popup closed.
2. **Select Multiple — multiple values commit cleanly.** Click 2 items; assert `aria-pressed`/`data-selected` on both + value is `["a", "b"]`.
3. **Combobox Basic — typing filters list.** Click trigger; type "or"; assert only "Orange" visible.
4. **Combobox Empty — empty-state renders when filter excludes all.** Type nonsense; assert `[data-testid="combobox-empty"]` text rendered.
5. **Autocomplete Basic — suggestions render below + Enter commits to value.** Type "hello@"; ↓ → Enter; assert input value === "hello@gmail.com".
6. **All three — ESC closes popup AND restores focus to trigger.** Open each; press ESC; assert popup hidden + focus on trigger.

## Verification gates

1. `pnpm --filter @zeroship/ui build` → green (expect DTS +10-15 KB for 3 components).
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. `pnpm --filter zeroship-builder build` → green.
4. Token purity x5 + raw `oklch(` in component CSS = 0.
5. A11y clean for ~148 stories (118 baseline + 30 new).
6. Aria-wiring 44 PASS + 2 SKIP + 0 FAIL (38 current + 6 new).
7. ~30 PNGs captured across the 3 components.

## Contingencies (decide inline, never stop)

- **Multiple value mode for Select/Combobox**: Base UI uses generic `Multiple extends boolean | undefined = false` with conditional `Value[] | Value` return. Mirror this. If TypeScript inference gets weird with the discriminated union, fall back to declaring `SelectProps` as a discriminated union (matches the Toggle.Group fix from Slice 5).
- **Combobox.Chip styling**: chips render INSIDE the trigger input. Use Combobox's `chips` + `chip` primitives. Keep chip visual lightweight — single-line, label + remove-X button, accent-tinted rest tone.
- **Popup max-height**: the LongList stories will overflow. Cap at `min(50vh, 28rem)` + `overflow-y: auto`. Use `--zs-shadow-dialog` for the floating surface.
- **Popup backdrop**: Base UI's Combobox has its own Backdrop primitive. Decide: use it (modal-feel) or skip (popup-feel). Default: SKIP backdrop for Select/Combobox/Autocomplete — they're not dialogs, they're popover-anchored inputs.
- **Form submission**: Base UI handles hidden input via `inputRef`. Cascade `required` from Field context like Slice 4 did.
- **Filter algorithm**: Combobox/Autocomplete use Base UI's default substring-includes filter. Don't customize.
- **`asChild` on Select.Item / Combobox.Item**: don't ship. Items are leaf nodes; no compelling render-as case.
- **Forced-colors**: every visible state (trigger / popup / item / chip / selected / disabled / focused) must map to Canvas / CanvasText / Highlight / GrayText.
- **Hit-target**: items in the popup ≥ 1.75rem block-size (touch-friendly).
- **Slot for trigger asChild**: Combobox's trigger IS the input; no asChild. Select's trigger could support asChild (custom dropdown opener) — defer; not in stories.
- **Story `data-testid` discipline**: every interactive surface (trigger, items, chips, remove buttons) gets a stable `data-testid`.

## Report

End with files changed, per-component API surface (file:line), token purity, build status, a11y story count + clean status, aria-wiring counts, contingencies fired, ~30 screenshot paths, one taste note, "I did NOT commit, push, or merge."
