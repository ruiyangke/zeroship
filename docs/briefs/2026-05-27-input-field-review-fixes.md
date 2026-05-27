# Slice 2 Input + Field — review fixes (no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `1d89a5e3`).
Lands every finding from two review passes: pilot self-review + codex
read-only second pass. 26 items, nothing deferred per the established
slice-1 pattern.

## Goal
One focused commit that closes every item. Builds + a11y verifier
remain green; story coverage expands to demonstrate the fixed
behaviors.

## Hard constraints (unchanged)
- Worktree single-writer.
- Pre-launch, no back-compat — rename / restructure freely.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex, no raw px (oklch + rem only; in COMMENTS too — slice-1 fix's
  grep is the bright line).
- HIG anchor; Base UI is the headless layer — its aria wiring is what
  we protect; never overwrite it.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via
  logical properties — mandatory.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (6)

**1. Compose refs so Base UI's `controlProps.ref` reaches the input.**
`Input.tsx:218`. The current code (`ref={ref as Ref<HTMLInputElement>}`)
sets only the forwardRef'd ref and DROPS the ref Base UI emits via
`controlProps.ref`. Base UI uses that ref internally for validation
registration, autofill detection, focus management — dropping it breaks
those. Fix: compose both refs with a small helper:
```tsx
function setRef<T>(ref: React.Ref<T> | undefined, value: T | null) {
  if (typeof ref === "function") ref(value);
  else if (ref) (ref as React.MutableRefObject<T | null>).current = value;
}
function composeRefs<T>(...refs: Array<React.Ref<T> | undefined>) {
  return (value: T | null) => refs.forEach(r => setRef(r, value));
}
// In render:
ref={composeRefs(ref, controlProps.ref as React.Ref<HTMLInputElement>)}
```
Add a story `InputRefIntegration` that consumes a ref and verifies
`inputRef.current?.focus()` works (Storybook play function or simple
button trigger).

**2. Merge `aria-describedby` rather than letting consumer clobber Field's wiring.**
`Input.tsx:161` + the inner `<input>` spread. When the consumer passes
`aria-describedby="external-id"` via `...rest`, Base UI's `controlProps`
sends its own auto-wired `aria-describedby` (referencing description +
error ids) but our spread order causes one to overwrite the other. Fix:
destructure `aria-describedby` from `rest`, and after spreading
`controlProps`, set the final attribute to the union (space-separated):
```tsx
const { "aria-describedby": callerDescribedBy, ...restNoAria } = rest;
…
<input
  {...controlProps}
  aria-describedby={[
    controlProps["aria-describedby"],
    callerDescribedBy,
  ].filter(Boolean).join(" ") || undefined}
  …
/>
```
Add a story `WithExternalDescription` showing `<Input aria-describedby="external-help">`
inside a Field that has Field.Description — both ids should land in the
rendered `aria-describedby`.

**3. Replace the inert hit-area `::before` with real `min-block-size: var(--zs-hit-min)`.**
`Input.css:59–72`. The current ::before has `pointer-events: none` on a
non-focusable wrapper — clicks in the ::before's overflow pass through
to the parent layout, not the input. Functionally useless. Two
approaches; pick (a):
- **(a) [recommended]** Make small inputs actually 44pt tall on coarse
  pointers via `@media (pointer: coarse) { .zs-input--sm { min-block-size: var(--zs-hit-min); } }`.
  Simple, HIG-faithful (HIG says controls should be 44pt on touch),
  removes the broken ::before entirely.
- (b) Add `onClick={(e) => inputRef.focus()}` on the wrapper AND set
  `pointer-events: auto` on the ::before — more code, more state.
Drop the entire `@media (pointer: coarse) { .zs-input::before { … } }`
block in favor of (a). Update the Input.css anatomy comment.

**4. Switch horizontal Field layout to CSS Grid.**
`Field.css:33–60`. The current flex-wrap approach doesn't put
description+error in a column below the input on the right side — they
sit in flex flow alongside. Replace with grid:
```css
.zs-field--horizontal {
  display: grid;
  grid-template-columns: var(--zs-field-horizontal-label-w) 1fr;
  column-gap: var(--zs-space-4);
  row-gap: var(--zs-field-gap);
  align-items: start;
}
.zs-field--horizontal > .zs-field__label   { grid-column: 1; padding-block-start: var(--zs-space-2); }
.zs-field--horizontal > :not(.zs-field__label) { grid-column: 2; }
```
Drop the duplicate `.zs-field--horizontal { flex-wrap: wrap }` block.
Add a story `HorizontalLayoutWithError` showing label-left + control
+ description + error stacking correctly in the right column.

**5. `Field.Error` needs a live region.**
`Field.tsx:144–154`. Dynamic validation errors (e.g., `match="typeMismatch"`
firing after the user types) aren't announced by screen readers because
the error element has no `role` / `aria-live`. Fix: wrap or set on the
`Field.Error` element:
```tsx
<BaseField.Error
  ref={ref}
  role="alert"
  aria-live="polite"
  aria-atomic="true"
  className={composeBaseClass("zs-field__error", className)}
  {...rest}
/>
```
Note: Base UI may emit `role` itself — check whether spreading our
`role` collides with Base UI's `role` (would override or warn). If Base
UI emits `role="alert"` already, drop ours. Cite Base UI docs in a code
comment.

**6. Prevent iOS focus zoom on small inputs.**
`Input.css:79–80`. iOS Safari zooms when an input has `font-size < 16px`
on focus. Our small variant uses `--zs-text-subheadline-size` (0.9375rem
= 15px), tripping the zoom. Fix the small variant's font to clamp at
16px:
```css
.zs-input--sm .zs-input__control {
  font-size: max(1rem, var(--zs-text-subheadline-size));
  line-height: var(--zs-text-subheadline-line);
}
```
Note: this affects only the INNER `<input>` font; the wrapper visual
sizing (height, padding) stays as designed. Visually the text in a
small input becomes 16px which is fine — Mobile Safari's threshold is
absolute. Document the rationale inline.

### 🟡 Surface / API (12)

**7. `Field.Error match` boolean — verify or replace.**
`Input.tsx:279`. Base UI types `match` as `keyof ValidityState | ((value, formValues) => boolean)`. Passing `match` as a bare boolean (`match` shorthand for `match={true}`) is not in the documented surface. Verify with the actual @base-ui/react source / docs (fetch
https://base-ui.com/react/components/field if needed). Two options:
- If Base UI accepts boolean: add a code comment citing the source and
  keep current.
- If undocumented: switch to a callback that always returns true:
  `<BaseField.Error match={() => true}>{errorMessage}</BaseField.Error>`.
Either way, leave a code comment so the next maintainer knows why.

**8. Add `disabled` to `FieldContext` so Input inherits.**
`Field.tsx`. Add a `disabled?: boolean` prop on `<Field>` (mirroring
Base UI's `Field.Root` `disabled`), propagate via FieldContext, and let
`<Input>` read it (fallback when consumer didn't set their own).
Update FieldContextValue type. Add a story `FieldDisabledPropagation`
showing `<Field disabled>` disabling a contained Input.

**9. `error={false}` should not trigger combined-wrapping.**
`Input.tsx`. Current check is `error != null` (≈ "any defined non-null").
`false` is defined and non-null — so `<Input label="…" error={false}/>`
wraps in a Field unnecessarily. Tighten to:
```tsx
const errorIsTruthyOrMessage = error !== undefined && error !== null && error !== false;
const hasCombined = label != null || description != null || errorIsTruthyOrMessage;
```

**10. Shorthand `required` should render a visible marker.**
`Input.tsx`. When the combined shorthand is used (`<Input label="…" required />`),
the auto-generated Field wraps but no `<Field.Required />` glyph appears.
Insert `<Field.Required />` inside `<Field.Label>` when both `label` and
`required` are set:
```tsx
{label != null ? (
  <Field.Label>
    {label}
    {props.required ? <> <Field.Required /></> : null}
  </Field.Label>
) : null}
```

**11. Re-export `Field.Control` / `Field.Validity` / `Field.Item` from the namespace.**
`Field.tsx`. Base UI ships these subparts and consumers may want to
compose them directly. Add styled passthroughs (just like Label /
Description / Error) and attach to `Field.*` namespace. `Field.Control`
in particular is necessary for users who want a render-prop on the
control directly without going through our `Input`.

**12. Merge `wrapperProps.className` into the composed className.**
`Input.tsx:181–186`. Currently `wrapperClassName` is composed but
`wrapperProps.className` is ignored when set. Compose both:
```tsx
className={classnames(
  "zs-input",
  `zs-input--${variant}`,
  `zs-input--${size}`,
  wrapperClassName,
  wrapperProps?.className,
)}
```

**13. Add Firefox autofill rules.**
`Input.css:206–213`. Current rules use `:-webkit-autofill` only.
Firefox uses `:autofill` (without vendor prefix, Firefox 86+) and also
`-moz-appearance: none`. Add:
```css
.zs-input__control:autofill,
.zs-input__control:-webkit-autofill {
  /* … existing rules using both pseudo names … */
}
```

**14. Fix `InsideForm` story description.**
`Input.stories.tsx`. Currently the story description claims Base UI's
Form submission flow; the story actually uses native `<form onSubmit>`.
Either:
- Switch the story to use Base UI's `Form` component (`@base-ui/react/form`),
  OR
- Rewrite the description to accurately say "native form submission;
  Base UI's Field validation feeds into the native `submit` event."
Pick the second — less restructuring; lets us defer `Form` to a later slice.

**15. forced-colors: add explicit disabled slot color.**
`Input.css:252–288`. The forced-colors block colors `.zs-input__slot` to
`CanvasText` but the disabled state slot keeps that color instead of
falling to `GrayText`. Add:
```css
@media (forced-colors: active) {
  .zs-input[data-disabled] .zs-input__slot { color: GrayText; }
}
```

**16. `startSlot` shouldn't be unconditionally `aria-hidden`.**
`Input.tsx:196`. Currency prefixes (`$`, `€`, `¥`) carry semantic meaning
— a screen reader reading "49" without "dollars" loses the unit. Allow
opt-in via an `aria-label` on the slot OR a new prop. Simplest:
```tsx
startSlot != null ? (
  <span
    className="zs-input__slot zs-input__slot--start"
    aria-hidden={startSlotAccessible ? undefined : "true"}
  >
    {startSlot}
  </span>
) : null
```
Add a `startSlotAccessible?: boolean` / `endSlotAccessible?: boolean`
prop (default false for start — most slots are decorative icons; false
for end too, BUT make the WithSlots story's currency cell opt-in). OR
simpler: drop `aria-hidden` entirely and let consumers wrap decorative
content in `<span aria-hidden>` themselves. Either way, fix the
WithSlots story so its currency prefix is announced.

**17. Declare `--zs-field-horizontal-label-w` as a token.**
`styles.css`. Currently referenced via `var(--zs-field-horizontal-label-w, 10rem)`
fallback only. Promote to a foundation token so the override is
discoverable:
```css
:root {
  --zs-field-horizontal-label-w: 10rem;
}
```

**18. `Field.Required` should use `composeBaseClass`.**
`Field.tsx:170–205`. Label / Description / Error use `composeBaseClass`
(supports Base UI's `className: string | (state) => string`); Required
uses plain `classnames(...)`. Inconsistency. Since Required is a plain
`<span>` (not a Base UI part) the className signature is just `string`,
so the consistency argument is weaker — but bringing it under
`composeBaseClass` makes the file uniform. Either:
- Convert Required's className handling to `composeBaseClass`, OR
- Add a JSDoc comment explaining the asymmetry.

### 🟢 Nits / docs (8)

**19. Drop the duplicate `.zs-field--horizontal` rule block.**
Already handled by #4 (the Grid rewrite consolidates everything).

**20. Clean up type casts.**
- `Input.tsx:161` — `{...(rest as Record<string, unknown>)}` — try to
  type `rest` properly so the cast isn't needed. Likely needs
  `Omit<ComponentPropsWithRef<typeof BaseField.Control>, ...>`-shaped
  type. If the type system genuinely fights, leave the cast with a
  comment.
- `Field.tsx:122` — `ref as unknown as React.Ref<HTMLElement>` — same
  treatment. Base UI's Label ref type is HTMLElement; ours is
  HTMLLabelElement. The simplest cleanup is `ref as React.Ref<HTMLElement>`
  (one cast, not two).
- `Input.tsx:174, 224` — `(controlProps as { readOnly?: boolean })` etc.
  — leverage Base UI's exported `RenderProps` type if available, else
  type the render callback's `controlProps` parameter explicitly.

**21. Fix stale `--zs-field-control-size` JSDoc.**
`Field.tsx`. The JSDoc says "Visual size — sets a `--zs-field-control-size`
for descendants." We don't set that CSS variable — descendants pick up
size via FieldContext + cascade. Update wording: "Visual size — cascades
via FieldContext to a contained Input that doesn't set its own size."

**22. JSDoc for `Field.Required` glyph token.**
`Field.tsx`. Add JSDoc explaining the asterisk is the default `children`
and that there's intentionally no `--zs-field-required-symbol` CSS
custom property (the brief proposed one; we dropped it because nothing
in CSS reads it).

**23. Add `displayName` to Field subcomponents.**
`Field.tsx`. `FieldLabel`, `FieldDescription`, `FieldError`, `FieldRequired`
each need `.displayName = "Field.Label"` etc. for clean React DevTools
trees + Storybook docs autogeneration. The forwardRef inner functions
also need named display:
```tsx
const FieldLabel = forwardRef<…>(function FieldLabel(…) { … });
FieldLabel.displayName = "Field.Label";
```
(Same for the others.)

**24. Expand the stories matrix.**
`Input.stories.tsx`. Add:
- `FieldSizeInheritance` — `<Field size="sm">` containing an Input that
  doesn't set its own size. Story shows the cascade.
- `ErrorBooleanOnly` — `<Input invalid />` and `<Input error={true} />`
  side-by-side, demonstrating "error state without message".
- `WithExternalDescription` — see #2.
- `FieldDisabledPropagation` — see #8.
- `HorizontalLayoutWithError` — see #4.
- `InputRefIntegration` — see #1.
- `Autofill` — input with `name="email"` autoComplete="email" + a
  button that simulates filling; visual verification that the WebKit
  autofill background trick works.
- `CustomValidate` — uses Base UI's `validate={(v) => v === "admin" ? "Reserved" : null}`
  to demonstrate the custom-validation path.

Update `scripts/check-storybook-a11y.mjs` story list to include the
new IDs. Update `scripts/capture-input-evidence.mjs` similarly.

**25. Document the `composeBaseClass` invariant.**
`Field.tsx`. Add a JSDoc paragraph at the top of the file (or above
`composeBaseClass`) explaining: "Base UI's `className` accepts a string
OR a `(state) => string` callback. We compose ours on top while
preserving whatever the consumer passes." This makes the helper's
existence obvious to future maintainers.

**26. Code-comment on `Field.Error match` semantics.**
`Input.tsx` line ~279 (where the combined shorthand uses Field.Error).
Add a comment explaining whatever was decided in #7 — either citing
the Base UI source that supports boolean `match`, or noting why we
switched to `() => true`.

## Files to modify

- `sdks/ui/src/components/Field/Field.tsx` — items 5, 7, 8, 11, 18, 20, 21, 22, 23, 25
- `sdks/ui/src/components/Field/Field.css` — items 4, 19
- `sdks/ui/src/components/Input/Input.tsx` — items 1, 2, 7, 9, 10, 12, 16, 20, 26
- `sdks/ui/src/components/Input/Input.css` — items 3, 6, 13, 15
- `sdks/ui/src/stories/Input.stories.tsx` — items 14, 16, 24
- `sdks/ui/src/styles.css` — item 17
- `sdks/ui/scripts/check-storybook-a11y.mjs` — item 24 (new story IDs)
- `sdks/ui/scripts/capture-input-evidence.mjs` — item 24 (new story IDs)

## Verification (every step)

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity: BOTH greps empty (including inside CSS comments).
4. `pnpm --filter zeroship-builder build` → green.
5. A11y violations: serve `storybook-static`, run
   `STORYBOOK_URL=… node scripts/check-storybook-a11y.mjs` — must
   report `A11y clean for N stories across 1 themes (no serious/critical
   violations)` where N = 10 Button + 12 existing Input + 8 new Input = 30.
6. A11y incomplete: temp scanner — 0 background-gradient, 0
   pseudo-element entries. Delete scanner after.
7. Aria wiring assertion: for the new `WithExternalDescription` story,
   verify the rendered `<input>` has `aria-describedby` containing BOTH
   the external id AND Field's description id. For the
   `InputRefIntegration` story, verify the ref-consumer can call
   `.focus()` on the input. Print pass/fail; fix any failures before
   reporting.
8. Capture 20 Input PNGs (12 existing + 8 new) via the updated
   capture script.

## Report (stdout)
- Files changed/created.
- Per-decision confirmation referencing item numbers 1–26.
- Token purity grep output.
- Build status (both packages).
- A11y violations result line (must show 30 stories × 1 theme).
- A11y incomplete result — explicit zero on gradient/pseudo.
- Aria wiring assertions.
- Screenshot paths — all 20 Input PNGs.
- One taste note (if any).
- Explicit: "I did NOT commit, push, or merge."
