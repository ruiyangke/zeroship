# Slice 5 code review fixes — Toggle + Toggle.Group

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `4e420d71`).

Closes dual code review of Slice 5 — codex (1 🔴 + 4 🟡 + 3 🟢) + claude (1 🔴 + 2 🟡 + 1 🟢), deduped to 12 items. Transcripts: codex at `/tmp/claude-1000/.../tasks/bz3nun4oi.output:9870-9946`; claude reviewer task output `a4a3c3f9e234be9e2`.

## Hard constraints (unchanged)

- Pre-launch, no back-compat. **API rewrites allowed** — single-mode discriminated value is the right shape going forward.
- `--zs-*` tokens only. No raw hex / px / oklch() in component CSS.
- HIG as principle but NOT in source.
- Glass-surface invariant.
- `@media (forced-colors: active)` mappings reach every focusable surface.
- RTL via logical properties.
- Real-path aria-wiring (no shims).
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (2)

**1. forced-colors specificity escape — state selectors outrank the forced-colors block.**
`sdks/ui/src/components/Toggle/Toggle.css:403+` (codex).

The forced-colors block keys off `.zs-toggle`, `.zs-toggle[data-pressed]`, `.zs-toggle-group` — single-class specificity. Earlier state rules like `.zs-toggle--default:not([data-disabled]):not([data-pressed]):hover` (`:159`), `.zs-toggle[data-pressed]:not([data-disabled]):hover` (`:229`), group hover (`:339`, `:359`), and disabled group (`:386`) all have higher specificity AND `.zs-toggle` carries `forced-color-adjust: none` (`:407`). Result: under forced-colors active, hover/active/disabled states keep their token/color-mix backgrounds — fails the WCAG forced-colors requirement.

Fix — mirror every state selector inside `@media (forced-colors: active)` with equal or higher specificity:
```css
@media (forced-colors: active) {
  .zs-toggle--default:not([data-disabled]):not([data-pressed]):hover,
  .zs-toggle--plain:not([data-disabled]):not([data-pressed]):hover,
  .zs-toggle--tinted:not([data-disabled]):not([data-pressed]):hover {
    background-color: Canvas;
    color: CanvasText;
  }
  .zs-toggle[data-pressed]:not([data-disabled]):hover {
    background-color: Highlight;
    color: HighlightText;
  }
  .zs-toggle[data-disabled],
  .zs-toggle-group[data-disabled] .zs-toggle {
    background-color: Canvas;
    color: GrayText;
  }
  /* group hovers */
  .zs-toggle-group:not([data-disabled]) .zs-toggle:not([data-disabled]):not([data-pressed]):hover {
    background-color: Canvas;
    color: CanvasText;
  }
  /* plain-group hover */
  .zs-toggle-group--plain:not([data-disabled]) .zs-toggle:not([data-disabled]):not([data-pressed]):hover {
    background-color: Canvas;
  }
}
```

Verify by toggling forced-colors in DevTools after the fix — every visible state must paint with system tokens.

**2. `role="toolbar"` cannot be overridden by consumer but the inline comment claims it can.**
`Toggle.tsx:228-246` (claude + codex agreement).

JSX `{...rest}` is spread at `:229` BEFORE `role="toolbar"` at `:246`. Later attribute wins — `role="toolbar"` always clobbers `rest.role`. The comment at `:244-245` says the opposite.

Decision: **lock to `role="toolbar"`** (it's the canonical semantic for segmented controls and the reason `aria-orientation` is permitted). Fix:
- Add `Omit<…, "role">` to the public `ToggleGroupProps` interface so TypeScript blocks consumers from passing `role` at the call site.
- Delete the misleading comment.
- Move `role="toolbar"` BEFORE `{...rest}` defensively (in case a future maintainer drops the type omit). Belt-and-braces.

### 🟡 Calibration / API (6)

**3. Single-select API leaks Base UI's always-array shape.**
`Toggle.tsx:130` (codex 🟡 #1).

Codex: "Toggle.Group defaults to `multiple={false}`, but `value`, `defaultValue`, and `onValueChange` are always `Value[]`; the stories then need `useState<string[]>(["day"])` for single selection." That's wrong ergonomically — a segmented control's single mode should accept a scalar.

Fix via discriminated props (preserve the autonomous decision flag):
```tsx
export type ToggleGroupProps<Value extends string = string> =
  | ToggleGroupSingleProps<Value>
  | ToggleGroupMultipleProps<Value>;

interface ToggleGroupBaseProps {
  size?: ToggleSize;
  variant?: ToggleVariant;
  orientation?: ToggleOrientation;
  equalWidth?: boolean;
  disabled?: boolean;
  className?: string;
  children?: ReactNode;
}

interface ToggleGroupSingleProps<Value extends string> extends ToggleGroupBaseProps {
  multiple?: false;
  value?: Value;
  defaultValue?: Value;
  onValueChange?: (value: Value | undefined, eventDetails: BaseToggleGroup.ChangeEventDetails) => void;
}

interface ToggleGroupMultipleProps<Value extends string> extends ToggleGroupBaseProps {
  multiple: true;
  value?: Value[];
  defaultValue?: Value[];
  onValueChange?: (value: Value[], eventDetails: BaseToggleGroup.ChangeEventDetails) => void;
}
```

Internal adapter wraps scalar values into the array Base UI expects:
```tsx
const adaptedValue = multiple ? (value as Value[]) : value != null ? [value as Value] : undefined;
const adaptedOnChange = (next: Value[], details) => {
  if (multiple) onValueChange?.(next, details);
  else onValueChange?.(next[0], details);
};
```

Update the `TwoSegmentsSingle`, `FiveSegmentsSingle`, `WithLabel`, `Horizontal`, `Vertical`, `RTL`, `EqualWidthOff` stories to use scalar `value`/`defaultValue`. The `MultipleMode` story keeps arrays (and explicit `multiple={true}`).

**4. Group CSS overrides child-resolved size/variant attributes — breaks prop-wins cascade.**
`Toggle.css:326+` (codex 🟡 #2).

The cascade contract: child prop > group context > default. But `.zs-toggle-group--sm .zs-toggle` (`:331`) overrides child `data-size`, and `.zs-toggle-group--tinted .zs-toggle:not([data-pressed])` (`:370`) overrides child variant. So `<Toggle.Group size="sm"><Toggle size="lg">…</Toggle></Toggle.Group>` ignores the child override.

Fix — key group child rules off the child's resolved `data-size` / `data-variant` data attributes (the inner already emits `data-size={size}` and `data-variant={variant}` per Slot Slice 5 fix #2):
```css
.zs-toggle-group .zs-toggle[data-size="sm"] { /* sm radius/padding */ }
.zs-toggle-group .zs-toggle[data-size="md"] { /* md radius/padding */ }
.zs-toggle-group .zs-toggle[data-size="lg"] { /* lg radius/padding */ }

.zs-toggle-group--tinted .zs-toggle[data-variant="tinted"]:not([data-pressed]) {
  /* tinted rest tone */
}
```

Pull the size-bearing rules out of `.zs-toggle-group--{sm,md,lg}` and onto child `[data-size]` selectors. Same for variant.

**5. Mixed icon/text dev-warn conflicts with `equalWidth={false}` API + story.**
`Toggle.tsx:195` (codex 🟡 #3).

The docs at `:120` say `equalWidth={false}` is for icon-only + text-only mixed segments. The `EqualWidthOff` story (`stories.tsx:556+`) demonstrates exactly this pattern. In dev, that canonical story fires the mixed-content warn. Wrong shape.

Fix — treat `equalWidth={false}` as the explicit opt-in. Skip the mixed-content scan when `equalWidth === false`:
```tsx
useEffect(() => {
  if (equalWidth === false) return; // explicit opt-in to mixed content
  // ... mixed-content scan ...
}, [signature, equalWidth]);
```

**6. Refs fanned out twice — caller's ref invoked on both BaseToggle AND the rendered element.**
`Toggle.tsx:359` (codex 🟡 #4).

`<BaseToggle ref={ref}>` at `:361` registers the caller's ref via Base UI's forwarding; Base UI then includes that ref in `baseProps.ref`. The render callback at `:381` (asChild) and `:401` (default button) composes `ref` (the same caller's ref) WITH `basePropsRef` again. Callback refs invoked twice per mount/unmount.

Fix — don't pass `ref` to `<BaseToggle>`; only compose it through `basePropsRef` (or vice versa). Pick: pass `ref` only to BaseToggle (idiomatic Base UI), then the render callback uses `basePropsRef` alone:
```tsx
return (
  <BaseToggle.Root
    ref={ref}
    pressed={pressed}
    // ... no extra ref composition in the render fn ...
    render={(baseProps, { ref: basePropsRef }) => {
      if (asChild) {
        return <Slot {...baseProps} ref={basePropsRef} data-size={size} data-variant={variant}>{children as ReactElement}</Slot>;
      }
      return <button {...baseProps} ref={basePropsRef} data-size={size} data-variant={variant}>{children}</button>;
    }}
  />
);
```

**7. `ToggleProps<Value extends string>` generic is unused on the standalone Toggle.**
`Toggle.tsx:274-308` (claude 🟡 #1).

The `<Value>` generic is declared but never referenced in the prop body. Base UI's `value` flows through `Omit<BaseToggleRootProps, …>` untyped at this surface. So `<Toggle<"day" | "week"> value="month">` doesn't error. Dead weight + implies a contract that doesn't hold.

Fix — pull `value` out of the omit list and re-declare it typed:
```tsx
export interface ToggleProps<Value extends string = string>
  extends Omit<BaseToggleRootProps, "className" | "render" | "value"> {
  value?: Value;
  size?: ToggleSize;
  variant?: ToggleVariant;
  asChild?: boolean;
  className?: string;
}
```

**8. `<Slot>` asChild path drops `data-size` / `data-variant` data attributes.**
`Toggle.tsx:378-388` vs `:398-412` (claude 🟡 #2).

Default-path `<button>` explicitly sets `data-size data-variant`. The asChild Slot relies on Base UI re-emitting them via `baseProps` — not contractually guaranteed.

Fix (covered as part of item 6's refactor): always set `data-size={size} data-variant={variant}` explicitly on both Slot and the default `<button>`.

### 🟢 Nits (4)

**9. Horizontal story doc claims arrows move focus AND selection.**
`stories.tsx:481` (codex 🟢). Update to: "Arrow keys move focus across segments; Space/Enter activates the focused segment. Toolbar semantics, not Radio-style selection."

**10. Multiple group stories lack `data-testid` on interactive segments.**
`stories.tsx:357+, 496+, 527+, 595+, 628+, 664+` (codex 🟢). Add stable `data-testid="toggle-{storyId}-{value}"` to each interactive Toggle child in FiveSegmentsSingle, Horizontal, Vertical, WithLabel, DisabledGroup, RTL.

**11. `Toggle` asChild dev-warn isn't deduped (consistent with Dialog.Close convention).**
`Toggle.tsx:342-349` (claude 🟢). The Dialog.Close pattern is the established convention (also undeduped). LEAVE AS-IS to match the repo's convention — no fix needed. Document in inline comment that this matches Dialog.Close.

**12. Comment + role override behavior (covered by fix 2).** Delete the misleading comment per fix 2.

## Files to modify

- `sdks/ui/src/components/Toggle/Toggle.tsx` — items 2 (role lock + Omit role), 3 (discriminated API), 5 (equalWidth-false skip), 6 (single-ref path), 7 (typed value), 8 (explicit data-attrs on Slot), 11 (inline comment), 12 (delete misleading comment).
- `sdks/ui/src/components/Toggle/Toggle.css` — items 1 (forced-colors specificity mirror), 4 (key off child data-attrs).
- `sdks/ui/src/stories/Toggle.stories.tsx` — item 3 (scalar value in single-mode stories), item 9 (Horizontal doc), item 10 (data-testids).
- `sdks/ui/scripts/check-aria-wiring.mjs` — may need to update assertion 32 (single-mode mutual exclusion) — assertion currently reads `value === ["day"]`; needs `value === "day"` after the discriminated API change. Same for assertion 34.

## Regression test mandate

Per the project's "every bug fix ships a regression test" rule, both 🔴s need a test that would fail pre-fix:

- **Item 1 (forced-colors)**: Add an aria-wiring assertion that loads a hovered Toggle inside `@media (forced-colors: active)` emulation and asserts computed background-color is the system value `Canvas` or `Highlight`. Playwright's `emulateMedia({ forcedColors: 'active' })` enables this. Without the fix, the test will see a token-derived oklch background, not Canvas.
- **Item 2 (role override)**: Add a TypeScript-level test (e.g., a `// @ts-expect-error` comment) at the consumer site asserting `<Toggle.Group role="radiogroup">` is now a type error. Plus an aria-wiring assertion that the rendered group has `role="toolbar"` no matter what — already covered by existing assertion if it queries by role.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5 + raw `oklch(` in component CSS = 0.
5. Spin `npx http-server storybook-static -p 6119 --silent`. A11y clean for 116+ stories (count may grow if new test stories added).
6. `node scripts/check-aria-wiring.mjs` — current 37 PASS + 2 SKIP + 0 FAIL, plus the new forced-colors regression assertion makes 38+ PASS.
7. Re-capture all 16 Toggle PNGs (plus any new test stories).

## Contingencies

- **Discriminated API regression risk**: existing aria-wiring assertions 31–34 reference array-shape state. Update inline to scalar shape. Verify all 4 still pass.
- **Group CSS rewrite**: pulling size rules out of `.zs-toggle-group--sm` and onto child `[data-size]` selectors may double up specificity. If any existing story regresses visually (sm group now looks like md), check the captures after the change.
- **forced-colors test**: Playwright's `emulateMedia({ forcedColors: 'active' })` works in Chromium 86+; verify against the installed @playwright/test version.
- **Discriminated type narrowing**: TypeScript needs `multiple: true` to narrow `value` to `Value[]`. Make `multiple: false | undefined` the single-mode discriminant; consumers writing `<Toggle.Group>` without `multiple` get the scalar branch automatically.

## Report

End with:
- Files changed.
- Per-item confirmation (1–12) with file:line refs.
- Token purity grep results.
- Build status (3 builds).
- A11y story count + clean status.
- Aria-wiring counts.
- Contingencies that fired (especially items 1 emulateMedia viability, 3 type narrowing, 4 group CSS specificity changes).
- 16+ screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
