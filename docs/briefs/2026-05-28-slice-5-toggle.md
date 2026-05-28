# Slice 5 — Toggle + ToggleGroup (segmented control)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `3c650863`, post-Slice-4-complete).

Toggle is a two-state pressable button (pressed / unpressed). ToggleGroup chains Toggles into a segmented control — single-selection (`multiple={false}` default, mutually exclusive) or multi-selection (`multiple={true}`, independent booleans). Verified APIs from `@base-ui/react@1.5.0/{toggle,toggle-group}`.

## Goal

`@zeroship/ui` ships Toggle + ToggleGroup wrapping Base UI's headless primitives. The component shape encodes the design guarantee that:

- **Toggle = pressable button with on/off state.** Distinct from Switch (binary settings toggle): a Toggle reads as a button affordance; a Switch reads as a settings widget. Same logical "on/off" but different visual register.
- **ToggleGroup = segmented control.** Apple-style: 2–5 items, mutually-exclusive default, equal-width segments, single inset rim around the whole group. Multiple-mode for filter pill rows.

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / no raw px / no raw `oklch()` in component CSS.
- HIG as principle but NOT in source.
- Glass-surface invariant: opaque `background-color` on every visible element.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- Real-path aria-wiring (no shims).
- DO NOT commit, push, or merge.

## Principles encoded (design rationale, NEVER named in source)

1. **Current state must be obvious.** Pressed segments paint with accent fill; unpressed segments are transparent over the group's track. Shape signal: pressed segment has full radius pill; unpressed shows only hover/focus tint.
2. **2–5 segments is the sweet spot.** Beyond 5, recommend a Select (Slice 6). Dev-warn if a ToggleGroup ships with 6+ Toggle children.
3. **Equal-width segments by default.** `grid-template-columns: repeat(<n>, 1fr)` lays out the group; intrinsic-width override via `data-equal-width="false"` for icon-only toolbars.
4. **Keep content types consistent.** Either all-text or all-icon segments — don't mix. Dev-warn if some segments have text-only labels and others have icon-only.
5. **Hover-intent on standalone Toggle.** Standalone Toggle uses the same hover-tint Button's `--gray` variant has. Inside a Group, the unpressed segments share a unified track surface, no per-segment hover tint (Group hover is at the segment-level, not the chip-level).
6. **No bleed on focus ring.** The Group's inset rim must not clip the focus ring of an individual Toggle when it's focused. Use `outline` (not `box-shadow` on the segment) so the ring escapes the group's overflow.

## Component API

### Toggle (standalone)

```tsx
// sdks/ui/src/components/Toggle/Toggle.tsx
export interface ToggleProps<Value extends string = string>
  extends Omit<BaseToggleProps<Value>, "render" | "className"> {
  /** Visual size — small 32, medium 40 (default), large 48 — matches Button + Input rhythm. */
  size?: "sm" | "md" | "lg";
  /**
   * Visual variant. `default` is gray (matches Button gray) when unpressed,
   * accent when pressed. `plain` strips the gray surface entirely, only paints
   * pressed/focused — useful in compact toolbars. `tinted` is accent-tinted
   * unpressed (loud, signals interactivity).
   */
  variant?: "default" | "plain" | "tinted";
  className?: string;
  /** Render-as a custom element. Composes via Slot from _slot.ts (canonical pattern from Dialog.Close). */
  asChild?: boolean;
}
```

Toggle inherits NativeButtonProps from Base UI — `<Toggle>` renders a real `<button>` when not in `asChild`. Hidden `<input>` semantics: Toggle does NOT submit form values (it's a button, not a form control). Consumers wrap with a real form input if needed.

### ToggleGroup

```tsx
// sdks/ui/src/components/Toggle/Toggle.tsx (same file — namespace pattern like Radio.Group)
export interface ToggleGroupProps<Value extends string = string>
  extends Omit<BaseToggleGroupProps<Value>, "render" | "className"> {
  /** Visual size — inherited by children unless they override. */
  size?: "sm" | "md" | "lg";
  /**
   * Visual variant — inherited. `default` paints a unified track behind the
   * segments + accent pill on pressed; `plain` is segments-only (no track).
   */
  variant?: "default" | "plain" | "tinted";
  /**
   * Equal-width segments (default true). `false` lets each Toggle size to its
   * own content — useful in toolbars where icon-only segments mix with
   * text-only ones.
   * @default true
   */
  equalWidth?: boolean;
  className?: string;
}

// Namespace export (mirrors Radio.Group)
export const Toggle = ForwardedToggle as ToggleComponent & {
  Group: typeof ToggleGroup;
};
```

`Toggle.Group` is the namespace surface. Standalone `<Toggle>` is also valid (the file-level export). Both share size/variant axes.

### Cross-component inheritance

- `Toggle.Group` exposes `ToggleGroupContext` with `{ size, variant, multiple }`. Each `Toggle` child reads it — prop wins, then context, then default.
- ToggleGroup carries `equalWidth` to the CSS via `data-equal-width="true|false"`. Children read the data attribute (no need to pass it through context).
- `disabled` cascades from group to children (Base UI handles this natively via the group's state).

## Files to create

```
sdks/ui/src/components/Toggle/
  Toggle.tsx          (~280 lines — Toggle + ToggleGroup live in one file, namespace pattern)
  Toggle.css
  index.ts

sdks/ui/src/stories/
  Toggle.stories.tsx  (16 stories)

sdks/ui/scripts/
  capture-toggle-evidence.mjs  (NEW)
```

## Files to modify

- `sdks/ui/src/components/index.ts` — add `Toggle` export.
- `sdks/ui/src/index.ts` — re-export prop types from the dist barrel.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register 16 new stories under a `// Slice 5` block.
- `sdks/ui/scripts/check-aria-wiring.mjs` — add 4 new assertions (see below).

## Story coverage matrix (16 stories)

### Standalone Toggle

1. `AllStates` — unpressed / pressed / disabled-unpressed / disabled-pressed (single row).
2. `AllSizes` — sm / md / lg in pressed state.
3. `AllVariants` — default / plain / tinted, each in unpressed + pressed.
4. `WithIconOnly` — pressable button with a single icon (e.g., a Bold "B" toggle). `aria-label` required.
5. `WithIconAndText` — icon + label.
6. `Disabled` — disabled in pressed and unpressed states.

### ToggleGroup

7. `TwoSegmentsSingle` — single-selection mode, 2 segments.
8. `FiveSegmentsSingle` — single-selection mode, 5 segments (max recommended).
9. `MultipleMode` — `multiple={true}`, 3 filter-pill segments where 0–3 can be pressed.
10. `AllSizes` — sm / md / lg vertical stack of equal-content groups.
11. `Horizontal` — explicit `orientation="horizontal"` (default; story just makes it visible).
12. `Vertical` — `orientation="vertical"`, stacked segments (settings panel use case).
13. `EqualWidthOff` — `equalWidth={false}`, mixed icon + text segments at intrinsic widths.
14. `WithLabel` — wrapped in `<Field><Field.Label>View</Field.Label><Toggle.Group>…</Toggle.Group></Field>`.
15. `Disabled` — whole group disabled.
16. `RTL` — Hebrew labels (e.g., "יום" / "שבוע" / "חודש"); segments stack right-to-left.

## Aria-wiring assertions (real-path Playwright)

Add to `check-aria-wiring.mjs`:

1. **Toggle standalone — click toggles `aria-pressed`.** Click `data-testid="toggle-standalone"`; assert `aria-pressed` flipped from `"false"` to `"true"`.
2. **ToggleGroup single — selecting flips others off.** Click 2nd segment; assert it has `aria-pressed="true"` AND 1st segment has `aria-pressed="false"`.
3. **ToggleGroup multiple — selections independent.** Click segments 1, 2, 3 in sequence; assert all three have `aria-pressed="true"`. Click segment 2 again; assert it flipped to `"false"` while 1 and 3 stayed `"true"`.
4. **ToggleGroup arrow-key roving.** Focus 1st segment; press ArrowRight; assert focus + selection moved to 2nd segment (Base UI's `loopFocus` is `true` by default; just verify the arrow moves selection).

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5: hex=0, px ≤2 pre-existing, zs-blur=0, Card.Body=0, CardBody=0.
5. Raw `oklch(` in component CSS = 0.
6. **A11y clean for 116 stories** (100 baseline + 16 new).
7. **Aria-wiring 37 PASS** + 2 SKIP + 0 FAIL (33 current + 4 new).
8. 16 Toggle PNGs captured.

## Contingencies (decide inline, never stop)

- **Icon size in icon-only Toggle**: pick `1em` against the Toggle's font-size so icons scale with size. If consumer-supplied icons are SVG `<svg>` with intrinsic dimensions, force `width: 1em; height: 1em` in `Toggle.css` so all variants stay consistent. Document inline.
- **Pressed-fill contrast**: default-variant pressed segments use `--zs-accent` filled with `--zs-accent-ink` (white-on-indigo) text. Verify axe color-contrast on Group-Multiple with multiple pressed segments — should be ≥AA. If not, deepen the fill to `--zs-accent-hover` instead. Document inline.
- **Dev-warn for 6+ segments**: gate on `process.env.NODE_ENV !== "production"`, run via `useEffect` keyed by the count signature (mirroring AlertDialog.Footer's de-duped warn pattern). One warn per Group instance per signature.
- **Mixed text/icon dev-warn**: same effect, second guard. Look at React.Children for direct Toggle children, check whether each has text content vs icon-only content (heuristic: `typeof children === "string"` vs `isValidElement(children)`).
- **Vertical orientation row layout**: in vertical mode the group is `grid-template-rows` rather than columns. Force `inline-size: max-content` on each segment so vertical groups don't stretch unnaturally wide.
- **`asChild` on Toggle**: routes through `Slot` per the canonical Dialog.Close pattern (commit `3a64a726`). DON'T omit `children` from the public type. Decide nativeButton via heuristic (`children.type === "button"` → `nativeButton=true`).
- **forced-colors**: every pressed/unpressed/focused/disabled state maps to `Canvas` / `CanvasText` / `Highlight` / `GrayText`. Don't bleach focused state under forced-colors.

## Report

End with:
- Files changed.
- Component API surface (Toggle + Toggle.Group) with file:line refs.
- Token purity grep results (5 standard + oklch in components).
- Build status (3 builds).
- A11y story count + clean status.
- Aria-wiring counts.
- Contingencies that fired.
- 16 screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
