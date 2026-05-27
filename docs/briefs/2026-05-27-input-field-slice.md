# Slice 2 — Input + Field (Apple-HIG anchored, library-survey informed)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `ea91dc1c`).
**Status when this brief is written:** slice 1 (Button + Crystal) shipped
and polished. `Input` is still a placeholder in `placeholders.tsx`. Other
placeholders (Badge, Card, Dialog) remain.

## Goal

Replace the `Input` placeholder with a real, HIG-anchored Input + the Field
wrapper that the form-frame pattern depends on. Set the pattern every
later form control (Checkbox / Radio / Switch / Select / Slider /
Textarea) will inherit. **Correct + powerful** — informed by a 13-library
DS survey (`/tmp/input-field-research.md`).

ONE pair of components: `Field` (namespace) + `Input`. No Textarea, no
PasswordInput, no InputGroup, no clearable, no character counter — those
are slice 2.5 / 3.

## Hard constraints (unchanged from slice 1)

- Worktree single-writer.
- Pre-launch, no back-compat — replace placeholder cleanly.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex, no raw px (oklch + rem only).
- HIG is the design principle. Visual sizes / radii / motion / focus
  mirror Button so an Input next to a Button reads coherent.
- Base UI is the headless layer. We render its `Field.*` parts and use
  `Field.Control` for the Input shell. **Do not bypass Base UI's
  context** — that's how we get free aria wiring.
- `prefers-reduced-motion` honored, `@media (forced-colors: active)`
  rules, RTL via logical properties (`inline-size`, `padding-inline`,
  `margin-inline-*`, `border-inline-start`).
- DO NOT commit, push, or merge.

## HIG citations driving choices

- `https://developer.apple.com/design/human-interface-guidelines/text-fields` —
  "Use a text field to request a small amount of information." "Show a
  hint to help communicate purpose." "Use secure text fields to hide
  private data." "Display a Clear button in the trailing end" (iOS).
  "Validate fields when it makes sense." (No floating labels;
  HIG-specific platforms don't use them.)
- `https://developer.apple.com/design/human-interface-guidelines/labels` —
  Labels are small static text, system fonts, semantic label colors.
- `https://developer.apple.com/design/human-interface-guidelines/virtual-keyboards` —
  Map to web `inputMode`: `text` / `email` / `numeric` / `decimal` /
  `tel` / `url` / `search`.

## Library-survey takeaways (full report: `/tmp/input-field-research.md`)

**Decomposed + combined ergonomics is the dominant strong pattern**
(Mantine, Chakra v3, Fluent v9, shadcn). Bedrock APIs are decomposed
(`<Field><Field.Label/><Input/></Field>`); a top-level convenience
takes `label` / `description` / `error` props for one-off use. We
adopt this shape.

**11 anti-patterns the survey called out — we avoid every one:**
1. MUI's floating label + outlined notch (confusing, not HIG, layout-shift).
2. MUI Base's `slots` / `slotProps` triple-layer prop API.
3. Polymorphic `as` / `asChild` on every subpart — Base UI's `render`
   covers it; we don't add a second axis.
4. Overloading `type` to express status (Geist `type="error"`).
   `type` is HTML-sacred (email keyboards, autofill, ValidityState);
   status goes on a separate `invalid` prop.
5. `addonBefore`/`addonAfter` *inside* the input box vs `prefix`/`suffix`
   *outside* — we keep them distinct. `startSlot`/`endSlot` live INSIDE
   the bordered shell; the outside-attached pattern is `InputGroup`
   (slice 2.5+).
6. `loadingPosition` prop — when a spinner in `endSlot` does the same.
7. Two competing labelling stories — `<Field.Label>` is canonical;
   `label` prop on Input is sugar that internally renders a Field.
8. `error` ambiguity — two props: `invalid: boolean` (state) and
   `<Field.Error>` (message). In combined shorthand, `error: ReactNode | true`.
9. Auto-focus first invalid on submit — confusing for screen readers.
   Don't move focus implicitly; announce via Field.Error live region.
10. Reinventing validation — use Base UI's `validate` + `validationMode`.
    Standard Schema (Zod/Valibot/ArkType) bindings happen in userland.
11. One slot for helper/error that flips on `invalid` — we let
    `<Field.Description>` and `<Field.Error>` co-exist; data-state
    drives visibility.

## Files to create

- `sdks/ui/src/components/Field/Field.tsx`
- `sdks/ui/src/components/Field/Field.css`
- `sdks/ui/src/components/Field/index.ts`
- `sdks/ui/src/components/Input/Input.tsx`
- `sdks/ui/src/components/Input/Input.css`
- `sdks/ui/src/components/Input/index.ts`
- `sdks/ui/src/stories/Input.stories.tsx`
- `sdks/ui/scripts/capture-input-evidence.mjs` (mirrors capture-button-evidence)

## Files to modify

- `sdks/ui/src/styles.css` — add Input + Field control-related foundation
  tokens AND a small set of palette tokens to the crystal theme block
  (border colors, input bg fills). Also `@import "./components/Field/Field.css"`
  and `@import "./components/Input/Input.css"`.
- `sdks/ui/src/components/index.ts` — export `Field`, `Input` and their
  prop types.
- `sdks/ui/src/index.ts` — drop `Input` and `InputProps` from
  placeholders re-export; re-export from `./components`. Add `Field` /
  `FieldProps`. Keep Badge / Card / Dialog placeholders.
- `sdks/ui/src/placeholders.tsx` — DELETE the `Input` export and
  `InputProps` interface (it had `label`/`hint`/`error` accepted-and-dropped
  props; the real Input picks those up properly).
- `sdks/ui/.storybook/preview.ts` — no changes (theme stays `crystal`).
- `sdks/ui/scripts/check-storybook-a11y.mjs` — add the new Input story IDs.

## Foundation tokens to add — `:root` in styles.css

Theme-invariant. Add these alongside the existing block:

```css
:root {
  /* (existing slice-1 tokens unchanged) */

  /* ─── Control sizing (Input today; Select/Textarea/etc. follow) ──────── */
  --zs-control-h-sm: 2rem;                    /* 32 — matches Button.small */
  --zs-control-h-md: 2.5rem;                  /* 40 — matches Button.medium */
  --zs-control-h-lg: 3rem;                    /* 48 — matches Button.large */
  --zs-control-px-sm: var(--zs-space-3);      /* 12 */
  --zs-control-px-md: var(--zs-space-4);      /* 16 */
  --zs-control-px-lg: var(--zs-space-5);      /* 20 */
  --zs-control-radius-sm: var(--zs-radius-2); /* 6 */
  --zs-control-radius-md: var(--zs-radius-3); /* 8 */
  --zs-control-radius-lg: var(--zs-radius-4); /* 10 */

  /* ─── Field layout ──────────────────────────────────────────────────── */
  --zs-field-gap: var(--zs-space-2);          /* 8 — vertical gap between
                                                 Label / Control / Description / Error */
  --zs-field-label-gap: var(--zs-space-1);    /* 4 — label-to-control */
  --zs-field-required-symbol: "*";             /* required indicator glyph */
}
```

No `prefers-reduced-motion` override needed for tokens; motion-using
selectors already cascade from the existing block.

## Crystal theme tokens to ADD — `[data-theme="crystal"]` block

Append to the existing crystal block (do NOT touch slice-1 entries):

```css
[data-theme="crystal"] {
  /* (existing slice-1 entries unchanged) */

  /* ─── Input palette ─────────────────────────────────────────────────── */
  /* Outline variant: a subtle delineation from the surface */
  --zs-input-bg: var(--zs-fill-quaternary);
  --zs-input-bg-hover: var(--zs-fill-tertiary);
  --zs-input-bg-focus: var(--zs-surface-raised);
  --zs-input-bg-disabled: var(--zs-fill-quaternary);
  /* Filled variant: more pronounced */
  --zs-input-bg-filled: var(--zs-fill-secondary);
  --zs-input-bg-filled-hover: var(--zs-fill);
  /* Borders */
  --zs-input-border: var(--zs-separator);
  --zs-input-border-hover: var(--zs-separator-strong);
  --zs-input-border-focus: var(--zs-accent);
  --zs-input-border-invalid: var(--zs-system-red);
  /* Text */
  --zs-input-ink: var(--zs-label);
  --zs-input-placeholder: var(--zs-label-tertiary);
  --zs-input-ink-disabled: var(--zs-label-tertiary);
  /* Field text */
  --zs-field-label-ink: var(--zs-label);
  --zs-field-description-ink: var(--zs-label-secondary);
  --zs-field-error-ink: var(--zs-system-red);
  --zs-field-required-ink: var(--zs-system-red);
}
```

## Field — API

Wraps Base UI's `Field.Root` and exposes a flat namespace. Re-exports
Base UI's subparts with our styles applied.

```tsx
// sdks/ui/src/components/Field/Field.tsx (sketch)
import { Field as BaseField } from "@base-ui/react/field";

export interface FieldProps extends ComponentPropsWithoutRef<typeof BaseField.Root> {
  /** Orientation. Default 'vertical' (label above control). */
  orientation?: "vertical" | "horizontal";
  /** Inherits to descendant Input if Input has no own size. */
  size?: "sm" | "md" | "lg";
  /** Class hook for the root wrapper. */
  className?: string;
}

export const Field = forwardRef<HTMLDivElement, FieldProps>(function Field(
  { orientation = "vertical", size, className, ...rest },
  ref,
) {
  return (
    <BaseField.Root
      ref={ref}
      className={clsx("zs-field", `zs-field--${orientation}`, className)}
      data-size={size}
      {...rest}
    />
  );
}) as FieldComponent;

// Re-export Base UI subparts with our styled wrappers
Field.Label = StyledLabel;             // <Field.Label>
Field.Description = StyledDescription; // <Field.Description>
Field.Error = StyledError;             // <Field.Error match="…">
Field.Required = RequiredIndicator;    // <Field.Required fallback={…} />

type FieldComponent = typeof BaseFieldRoot & {
  Label: typeof StyledLabel;
  Description: typeof StyledDescription;
  Error: typeof StyledError;
  Required: typeof RequiredIndicator;
};
```

`Field.Label`, `Field.Description`, `Field.Error` are thin styled
wrappers around the Base UI parts (preserve all Base UI props + add
className + classes).

`Field.Required` is new — a small indicator that shows the required-symbol
when the enclosing Field is `required`, OR shows a `fallback` ReactNode
when it isn't:

```tsx
<Field.Label>Email <Field.Required /></Field.Label>
<Field.Label>Bio   <Field.Required fallback={<span>(optional)</span>} /></Field.Label>
```

Use Base UI's `useFieldRootContext()` hook to read the `required` state
(it's a documented Base UI hook).

## Input — API

```tsx
// sdks/ui/src/components/Input/Input.tsx (sketch)
import { Field as BaseField } from "@base-ui/react/field";

export type InputSize = "sm" | "md" | "lg";
export type InputVariant = "outline" | "filled" | "plain";

export interface InputProps extends Omit<
  ComponentPropsWithoutRef<"input">,
  "size" | "prefix" | "children"
> {
  /** HIG-aligned visual size. Default 'md'. */
  size?: InputSize;
  /** Visual variant. Default 'outline'. */
  variant?: InputVariant;
  /** Leading adornment INSIDE the bordered shell (icon, prefix). */
  startSlot?: ReactNode;
  /** Trailing adornment INSIDE the bordered shell (icon, button, clear). */
  endSlot?: ReactNode;
  /** Force invalid state independent of Field validation. */
  invalid?: boolean;

  /** Combined shorthand: when set, internally wraps in a Field. */
  label?: ReactNode;
  /** Combined shorthand description (renders Field.Description). */
  description?: ReactNode;
  /** Combined shorthand error. ReactNode → message, `true` → just invalid state. */
  error?: ReactNode | boolean;

  /** Escape hatch for the bordered-shell wrapper element. */
  wrapperClassName?: string;
  wrapperProps?: ComponentPropsWithoutRef<"div">;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  props,
  ref,
) {
  const { label, description, error, ...inputProps } = props;
  const hasCombined = label != null || description != null || error != null;

  if (hasCombined) {
    return (
      <BaseField.Root invalid={!!error || props.invalid}>
        {label != null ? <Field.Label>{label}</Field.Label> : null}
        <InputInner {...inputProps} ref={ref} />
        {description != null ? <Field.Description>{description}</Field.Description> : null}
        {error != null && error !== true ? <Field.Error>{error}</Field.Error> : null}
      </BaseField.Root>
    );
  }
  return <InputInner {...inputProps} ref={ref} />;
});
```

`InputInner` renders the styled shell. It uses `Base UI Field.Control`
with a `render` callback to project our shell onto the headless control
so aria wiring is preserved:

```tsx
function InputInner({
  size = "md",
  variant = "outline",
  startSlot,
  endSlot,
  invalid,
  className,
  wrapperClassName,
  wrapperProps,
  ...rest
}: InputProps, ref) {
  return (
    <BaseField.Control
      {...rest}
      ref={ref}
      render={(controlProps, state) => (
        <div
          {...wrapperProps}
          className={clsx(
            "zs-input",
            `zs-input--${variant}`,
            `zs-input--${size}`,
            wrapperClassName,
          )}
          data-size={size}
          data-variant={variant}
          data-invalid={state.valid === false || invalid || undefined}
          data-disabled={state.disabled || undefined}
          data-readonly={controlProps.readOnly || undefined}
          data-focused={state.focused || undefined}
        >
          {startSlot != null ? (
            <span className="zs-input__slot zs-input__slot--start" aria-hidden="true">
              {startSlot}
            </span>
          ) : null}
          <input
            {...controlProps}
            className={clsx("zs-input__control", className)}
            aria-invalid={invalid || state.valid === false || undefined}
          />
          {endSlot != null ? (
            <span className="zs-input__slot zs-input__slot--end">
              {endSlot}
            </span>
          ) : null}
        </div>
      )}
    />
  );
}
```

Key invariants:
- `aria-invalid`, `aria-describedby`, `aria-required`, `id` are all
  populated by Base UI's `controlProps` — don't override them in the
  spread of `{...rest}`. Spread `{...rest}` FIRST in the inner
  `<input>` element, then `{...controlProps}` from Base UI WINS (the
  inverse of Button's pattern — here Base UI's aria wiring is what we
  protect, not internal props).
- `aria-hidden="true"` on `startSlot` is decorative-only. The `endSlot`
  is NOT aria-hidden because it's often interactive (clear button,
  password toggle eventually); consumers must own aria for interactive
  end slots.

## CSS structure — Field.css

```css
.zs-field {
  display: flex;
  flex-direction: column;
  gap: var(--zs-field-gap);
  min-width: 0;
  font-family: var(--zs-font-system);
  color: var(--zs-label);
}

.zs-field--horizontal {
  flex-direction: row;
  align-items: flex-start;
  gap: var(--zs-space-4);
}
.zs-field--horizontal > .zs-field__label-cell {
  flex: 0 0 auto;
  padding-block-start: var(--zs-space-2); /* align with control center */
}
.zs-field--horizontal > .zs-field__control-cell {
  flex: 1 1 auto;
  min-width: 0;
}

.zs-field__label {
  display: inline-flex;
  align-items: baseline;
  gap: var(--zs-space-1);
  font-size: var(--zs-text-subheadline-size);
  line-height: var(--zs-text-subheadline-line);
  font-weight: 600;
  color: var(--zs-field-label-ink);
}

.zs-field__description {
  font-size: var(--zs-text-footnote-size);
  line-height: var(--zs-text-footnote-line);
  color: var(--zs-field-description-ink);
}

.zs-field__error {
  font-size: var(--zs-text-footnote-size);
  line-height: var(--zs-text-footnote-line);
  color: var(--zs-field-error-ink);
  display: flex;
  flex-direction: column;
  gap: var(--zs-space-half);
}

.zs-field__required {
  color: var(--zs-field-required-ink);
  font-weight: 600;
}
.zs-field__required--fallback {
  color: var(--zs-label-tertiary);
  font-weight: 400;
}

@media (forced-colors: active) {
  .zs-field__label   { color: CanvasText; }
  .zs-field__description { color: GrayText; }
  .zs-field__error   { color: Mark; }       /* Mark = high-contrast emphasis */
  .zs-field__required { color: Mark; }
}
```

## CSS structure — Input.css

```css
.zs-input {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: var(--zs-space-2);
  inline-size: 100%;                 /* fill Field control cell */
  block-size: var(--zs-control-h-md);
  padding-inline: var(--zs-control-px-md);
  border-radius: var(--zs-control-radius-md);
  background: var(--zs-input-bg);
  color: var(--zs-input-ink);
  font-family: var(--zs-font-system);
  font-size: var(--zs-text-body-size);
  line-height: var(--zs-text-body-line);
  transition:
    background-color var(--zs-motion-fast) var(--zs-motion-ease),
    box-shadow var(--zs-motion-fast) var(--zs-motion-ease);
  /* outline variant: 1px (logical) hairline border via box-shadow inset
     so border doesn't shift the layout when state changes */
  box-shadow: inset 0 0 0 0.0625rem var(--zs-input-border);
}

/* sizes */
.zs-input--sm {
  block-size: var(--zs-control-h-sm);
  padding-inline: var(--zs-control-px-sm);
  border-radius: var(--zs-control-radius-sm);
  font-size: var(--zs-text-subheadline-size);
  line-height: var(--zs-text-subheadline-line);
}
.zs-input--lg {
  block-size: var(--zs-control-h-lg);
  padding-inline: var(--zs-control-px-lg);
  border-radius: var(--zs-control-radius-lg);
  font-size: var(--zs-text-headline-size);
  line-height: var(--zs-text-headline-line);
}

/* variants */
.zs-input--filled {
  background: var(--zs-input-bg-filled);
  box-shadow: none;
}
.zs-input--plain {
  background: transparent;
  box-shadow: none;
  padding-inline: 0;
}

/* hover (only with hover-capable pointers) */
@media (hover: hover) {
  .zs-input:not([data-disabled]):not([data-focused]):hover {
    background: var(--zs-input-bg-hover);
    box-shadow: inset 0 0 0 0.0625rem var(--zs-input-border-hover);
  }
  .zs-input--filled:not([data-disabled]):not([data-focused]):hover {
    background: var(--zs-input-bg-filled-hover);
  }
  .zs-input--plain:hover { background: transparent; }  /* keep plain plain */
}

/* focused — focus ring lives on the wrapper, not the input */
.zs-input[data-focused] {
  background: var(--zs-input-bg-focus);
  box-shadow:
    inset 0 0 0 0.0625rem var(--zs-input-border-focus),
    0 0 0 var(--zs-focus-ring-width) var(--zs-focus-ring-color);
}
.zs-input--filled[data-focused] { background: var(--zs-input-bg-filled); }
.zs-input--plain[data-focused] {
  background: transparent;
  box-shadow: 0 0 0 var(--zs-focus-ring-width) var(--zs-focus-ring-color);
}

/* invalid */
.zs-input[data-invalid] {
  box-shadow:
    inset 0 0 0 0.0625rem var(--zs-input-border-invalid),
    0 0 0 0 transparent;
}
.zs-input[data-invalid][data-focused] {
  box-shadow:
    inset 0 0 0 0.0625rem var(--zs-input-border-invalid),
    0 0 0 var(--zs-focus-ring-width)
      color-mix(in oklch, var(--zs-system-red) 35%, transparent);
}

/* readonly */
.zs-input[data-readonly] {
  background: var(--zs-input-bg);
  color: var(--zs-input-ink);
  cursor: default;
}

/* disabled */
.zs-input[data-disabled] {
  background: var(--zs-input-bg-disabled);
  color: var(--zs-input-ink-disabled);
  cursor: not-allowed;
}
.zs-input[data-disabled] .zs-input__control { cursor: not-allowed; }

/* control */
.zs-input__control {
  flex: 1 1 auto;
  min-width: 0;
  inline-size: 100%;
  block-size: 100%;
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  outline: none;
  padding: 0;
  margin: 0;
}
.zs-input__control::placeholder {
  color: var(--zs-input-placeholder);
}
.zs-input__control:-webkit-autofill,
.zs-input__control:-webkit-autofill:hover,
.zs-input__control:-webkit-autofill:focus,
.zs-input__control:-webkit-autofill:active {
  /* override Chrome's yellow autofill — long box-shadow trick */
  -webkit-box-shadow: 0 0 0 1000rem var(--zs-input-bg) inset;
  -webkit-text-fill-color: var(--zs-input-ink);
}

/* slots */
.zs-input__slot {
  display: inline-flex;
  align-items: center;
  flex: 0 0 auto;
  color: var(--zs-label-secondary);
}
.zs-input__slot--start { margin-inline-end: var(--zs-space-1); }
.zs-input__slot--end { margin-inline-start: var(--zs-space-1); }

/* forced-colors / Windows High Contrast */
@media (forced-colors: active) {
  .zs-input {
    background: Field;
    color: FieldText;
    box-shadow: inset 0 0 0 0.0625rem CanvasText;
    forced-color-adjust: none;
  }
  .zs-input[data-focused] {
    box-shadow:
      inset 0 0 0 0.0625rem Highlight,
      0 0 0 0.125rem Highlight;
  }
  .zs-input[data-invalid] {
    box-shadow: inset 0 0 0 0.0625rem Mark;
  }
  .zs-input[data-disabled] {
    color: GrayText;
    box-shadow: inset 0 0 0 0.0625rem GrayText;
  }
}
```

## Stories (`Input.stories.tsx`)

Title: `Components/Input`. Mirror the Button stories' shape. Each story
sits in a `.zs-story-row` (opaque-base glass surface from slice 1).

1. **AllVariants** — `outline / filled / plain` × medium size, decomposed
   Field around each.
2. **AllSizes** — `sm / md / lg` × outline variant, single Field per row.
3. **AllStates** — default / hover (visual only) / focused (autofocus on
   first) / disabled / readOnly / invalid (with Field.Error message).
4. **WithSlots** — startSlot only (search icon), endSlot only (clear
   button), both (currency prefix + unit suffix).
5. **Decomposed** — full canonical pattern: `<Field><Field.Label/>
   <Input/><Field.Description/><Field.Error match="typeMismatch"/></Field>`.
   Story description tells reviewer to type invalid email to see error.
6. **Combined** — top-level shorthand: `<Input label description error />`.
7. **Required** — `<Field required>` with `<Field.Required/>`. Plus a
   second example with `<Field.Required fallback={"(optional)"}/>` on an
   unrequired Field.
8. **InputTypes** — gallery of `<Input type="email|tel|url|search|date|
   number|password" inputMode="…" autoComplete="…">`. Story description
   notes virtual-keyboard hints fire on mobile.
9. **HorizontalLayout** — `<Field orientation="horizontal">` with label
   left, control + helper right. Three rows demonstrating compact form
   density.
10. **RTL** — wrapper `<div dir="rtl">` with same buttons; verifies
    startSlot lands on visual right.
11. **LongLabelAndDescription** — verifies wrap behavior, narrow
    container, ellipsis on input value if needed.
12. **InsideForm** — `<form>` wrapping a Field + a Button submit;
    demonstrates aria-describedby + tab order; Button uses real
    `type="submit"`.

Each story is a CSF3 export. Update `scripts/check-storybook-a11y.mjs`
to include all 12 new IDs (alongside the 10 existing Button stories =
22 total).

## Verification

1. `pnpm --filter @zeroship/ui build` → green (ESM + DTS).
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity:
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty.
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty.
4. `pnpm --filter zeroship-builder build` → green (Input swap from
   placeholder shouldn't break the builder — verify it still uses Input
   correctly; if SettingsCanvas uses `label` prop on Input, the new
   combined shorthand picks it up cleanly).
5. **A11y violations**: serve `storybook-static` on a free port, run
   `STORYBOOK_URL=… node scripts/check-storybook-a11y.mjs`. Must print
   `A11y clean for 22 stories across 1 themes (no serious/critical
   violations)`.
6. **A11y incomplete**: write a temp scanner under `sdks/ui/scripts/`
   that lists both `violations` and `incomplete` for the
   `color-contrast` and `label` rules across the 22 stories. The
   `background gradient` incompletes that bedeviled slice 1 must NOT
   reappear. Delete the temp scanner after run.
7. **a11y wiring** (manual sanity check inside the scanner OR a one-off
   playwright assertion): for each `Decomposed` story, verify the
   rendered `<input>` has:
   - `aria-invalid="true"` when Field.Error matches
   - `aria-required="true"` when Field has `required`
   - `aria-describedby` referencing both description and (when shown)
     error
   - the `<input>` `id` matches the `<label for>` value
8. Capture screenshots — extend `scripts/capture-input-evidence.mjs`
   (mirror of capture-button-evidence). All 12 PNGs land in
   `storybook-static/theme-evidence/crystal-input-<story-id>.png`.

## Report (stdout)

End with:

- Files changed/created.
- Per-decision confirmation referencing this brief's section name
  (Field API ✓, Input API ✓, anti-patterns avoided ✓, etc.).
- Token-purity grep results (both empty).
- Build status — both packages.
- A11y violations result line (22 stories × 1 theme).
- A11y incomplete result — confirm zero new "background gradient" or
  pseudo-element entries.
- A11y wiring assertion result — htmlFor / aria-* checks.
- Screenshot paths — all 12.
- One taste note (if any judgment call beyond the brief).
- Explicit: "I did NOT commit, push, or merge."
