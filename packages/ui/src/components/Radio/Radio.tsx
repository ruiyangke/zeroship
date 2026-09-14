/*
 * Radio + RadioGroup — mutually-exclusive choice primitive.
 *
 * Wraps Base UI's `RadioGroup` + `Radio.Root` + `Radio.Indicator`.
 * The group is a <div> with arrow-key roving focus + selection
 * (Base UI's job); each Radio is a focusable chip plus a hidden
 * <input> for form submission.
 *
 * Design guarantees (in source so they travel with the code):
 *
 *   1. Use Radio for MUTUALLY EXCLUSIVE choice — exactly one of N.
 *      Two to five options is comfortable; beyond that, a Select
 *      ships later in the forms slate. A single Radio with no group
 *      is a usage error (warned ONCE at module level in dev — the
 *      warning is module-deduped rather than per-mount, so noisy
 *      reload-heavy dev sessions don't drown in repeat-warnings —
 *      slice-4 review fix item 11).
 *
 *   2. The group OWNS the value (`value` / `defaultValue` /
 *      `onValueChange`); each Radio only carries its discriminant
 *      `value`. This keeps the controlled/uncontrolled story
 *      simple — no per-Radio `checked` prop to manage.
 *
 *   3. Arrow-key roving lives in Base UI. ArrowUp/Left moves to the
 *      previous radio; ArrowDown/Right to the next; Home/End jump
 *      to the first / last. Don't override; the aria-wiring
 *      assertion verifies the wiring stays intact.
 *
 *   4. The checked chip has BOTH a thicker accent ring AND a centered
 *      dot — two visual signals so a colorblind user reads checked
 *      vs unchecked from shape, not just hue.
 *
 *   5. RTL: the chip stays at the inline-start of its row; the dot
 *      stays centered inside the chip. Logical properties handle
 *      the flip automatically.
 *
 *   6. Focus ring on the chip ROOT (slice-4 review fix item 2) so
 *      the canonical Field pattern paints a ring even on a bare
 *      Radio without the in-component `label` prop.
 *
 *   7. Hit-target floor lives on an invisible `::before` overlay on
 *      the chip — a bare chip still meets ≥ 2.75rem on coarse
 *      pointers (slice-4 review fix item 3).
 *
 * Field integration mirrors Checkbox / Switch: `size` inherits via
 * `useFieldVisualSize()`, `disabled` via `useFieldDisabledContext()`,
 * `required` via `useFieldContext()`; explicit prop always wins.
 * RadioGroup itself ALSO accepts `size` which cascades to every
 * contained Radio (children read via the Field context fallback OR a
 * group-local context — we use the Field context for symmetry with
 * the other primitives).
 *
 * The chip itself takes `className` for custom styling — there's no
 * asChild escape hatch (selection primitives are chips with hidden
 * inputs, not button-shaped surfaces — slice-4 review fix item 1).
 */
import {
  createContext,
  forwardRef,
  useContext,
  useEffect,
  useId,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Radio as BaseRadio } from "@base-ui/react/radio";
import { RadioGroup as BaseRadioGroup } from "@base-ui/react/radio-group";
import {
  useFieldContext,
  useFieldDisabledContext,
  useFieldVisualSize,
} from "../Field";
import { useFieldsetDisabledContext } from "../Fieldset";
import { classnames } from "../_classnames";
import { SelectionRow } from "../_selection-row";

export type RadioSize = "sm" | "md" | "lg";
export type RadioOrientation = "vertical" | "horizontal";

/* ─── group-local context ───────────────────────────────────────────── *
 *
 * Carries the group's `size` so child Radios pick it up without
 * climbing the React tree manually. A group nested inside a Field
 * already inherits from the Field, but a bare RadioGroup with
 * `size="lg"` outside a Field still wants to cascade — that's why
 * we keep a dedicated context rather than relying on Field alone.
 */
interface RadioGroupContextValue {
  size: RadioSize | undefined;
  disabled: boolean;
}

const RadioGroupContext = createContext<RadioGroupContextValue | null>(null);

function useRadioGroupContext() {
  return useContext(RadioGroupContext);
}

/* ─── RadioGroup ────────────────────────────────────────────────────── */

type BaseRadioGroupProps = ComponentPropsWithRef<typeof BaseRadioGroup>;

export interface RadioGroupProps<T = string>
  extends Omit<
    BaseRadioGroupProps,
    "className" | "render" | "children" | "value" | "defaultValue" | "onValueChange"
  > {
  /** Layout — `vertical` stacks (default); `horizontal` is a wrapping row. */
  orientation?: RadioOrientation;

  /** Visual size — cascades to each Radio child. */
  size?: RadioSize;

  /** Extra CSS class names merged onto the group `<div>` after the base
   *  `zs-radio-group` and modifier classes. */
  className?: string;

  /** Controlled selected value. Pair with `onValueChange`; omit for the
   *  uncontrolled story and use `defaultValue` instead. */
  value?: T;

  /** Uncontrolled initial selection. Ignored when `value` is set. */
  defaultValue?: T;

  /** Fires when the user picks a new value (keyboard or pointer). The
   *  second argument is Base UI's event-details payload (`event`,
   *  `reason`, `cancel`, etc.) — typed as `unknown` to stay independent
   *  of the upstream BaseUIChangeEventDetails shape. */
  onValueChange?: (value: T, eventDetails: unknown) => void;

  /** Child `<Radio>` elements — typically two to five options. Beyond
   *  that, prefer a Select. */
  children?: ReactNode;
}

function RadioGroupInner<T = string>(
  {
    orientation = "vertical",
    size: sizeProp,
    disabled: disabledProp,
    required: requiredProp,
    className,
    children,
    // Pull the typed value props out so Base UI sees them as named
    // props with our `<T>` typing rather than as part of `...rest` —
    // that drops the `as any` cast the old code used (slice-4 review
    // fix item 8). The named props win over `...rest` if Base UI ever
    // adds its own non-generic surface for them.
    value,
    defaultValue,
    onValueChange,
    ...rest
  }: RadioGroupProps<T>,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  // Cascade size + disabled from the enclosing Field / Fieldset if
  // not set on the group directly. Same shape Input / Checkbox /
  // Switch use. The Fieldset signal is a separate boolean context
  // because the visible chip is a non-native Base UI part.
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const fieldsetDisabled = useFieldsetDisabledContext();
  // `required` lives on the GROUP, not the individual radio (the WAI-
  // ARIA pattern paints `aria-required="true"` on `role="radiogroup"`).
  // Cascading it here means a `<Field required>` ancestor flows
  // straight through to Base UI's `RadioGroup`, which both emits
  // `aria-required` on the group element AND propagates `required` to
  // every child Radio internally. Slice-8 review yellow #1: before
  // this fix, the cascade ran per-Radio (`fieldCtx.required` read
  // inside `RadioInner`), which skipped Base UI's group-level
  // `aria-required` path entirely.
  const fieldCtx = useFieldContext();
  const size = sizeProp ?? fieldSize;
  // OR the two booleans rather than ??-chain — `??` would short-
  // circuit on a legitimate `false` from the inner Field.
  const disabled = disabledProp ?? (fieldDisabled || fieldsetDisabled);
  const required = requiredProp ?? fieldCtx?.required ?? false;

  return (
    <RadioGroupContext.Provider value={{ size, disabled }}>
      <BaseRadioGroup
        {...rest}
        // Base UI's value props are untyped (`unknown` at the public
        // surface). The `as never` keeps our `<T>` discipline visible
        // to TypeScript while threading through cleanly — no `any`.
        value={value as never}
        defaultValue={defaultValue as never}
        onValueChange={onValueChange as never}
        ref={ref}
        disabled={disabled || undefined}
        required={required || undefined}
        className={classnames(
          "zs-radio-group",
          `zs-radio-group--${orientation}`,
          size ? `zs-radio-group--${size}` : null,
          className,
        )}
        data-orientation={orientation}
        data-size={size}
      >
        {children}
      </BaseRadioGroup>
    </RadioGroupContext.Provider>
  );
}

const RadioGroup = forwardRef(RadioGroupInner) as <T = string>(
  props: RadioGroupProps<T> & { ref?: React.Ref<HTMLDivElement> },
) => React.JSX.Element;
(RadioGroup as React.FC).displayName = "Radio.Group";

/* ─── Radio ─────────────────────────────────────────────────────────── */

type BaseRadioRootProps = ComponentPropsWithRef<typeof BaseRadio.Root>;

export interface RadioProps<T = string>
  extends Omit<
    BaseRadioRootProps,
    "className" | "render" | "children" | "value"
  > {
  /** The discriminant value this Radio represents in the group. */
  value: T;

  /** Inherited from the enclosing RadioGroup / Field by default. */
  size?: RadioSize;

  /** Extra CSS class names merged onto the chip `<span>` after the base
   *  `zs-radio` and size modifier. */
  className?: string;

  /**
   * Optional text label rendered inline-end of the chip. The whole
   * row becomes a single <label> so clicking the text selects the
   * radio. Outside a Field, this is the normal way to label.
   */
  label?: ReactNode;

  /** Class name for the wrapping <label> row. */
  fieldClassName?: string;

  /** Extra props for the wrapping <label> (when `label` is present). */
  fieldProps?: ComponentPropsWithRef<"label">;
}

/* Module-level dev-warn dedup (slice-4 review fix item 11).
 * One warning per process lifetime — survives StrictMode double-render,
 * survives noisy reload-heavy dev sessions. Vite/esbuild DCE the whole
 * branch in production builds because `process.env.NODE_ENV === "production"`
 * (no optional-chain) is statically replaceable. */
let radioWithoutGroupWarned = false;

function RadioInner<T = string>(
  {
    value,
    size: sizeProp,
    className,
    label,
    fieldClassName,
    fieldProps,
    disabled: disabledProp,
    required: requiredProp,
    "aria-labelledby": ariaLabelledByProp,
    ...rest
  }: RadioProps<T>,
  ref: React.ForwardedRef<HTMLSpanElement>,
) {
  // Group context wins over field context for size — a group is a
  // tighter scope. Explicit prop still beats both. A wrapping
  // Fieldset is the outermost fallback for disabled.
  //
  // `required` is NOT cascaded from Field here — RadioGroupInner reads
  // `useFieldContext().required` and threads it through Base UI's
  // RadioGroup, which both paints `aria-required` on the group element
  // (the WAI-ARIA radiogroup pattern) AND propagates `required` to every
  // child Radio internally. Cascading per-Radio here would re-emit
  // `aria-required` on each chip without ever touching the group, which
  // is the bug slice-8 review yellow #1 caught. An explicit
  // `required={true}` on a single Radio still wins (Base UI ORs the
  // group-level and per-Radio flags).
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const fieldsetDisabled = useFieldsetDisabledContext();
  const groupCtx = useRadioGroupContext();

  const size: RadioSize = sizeProp ?? groupCtx?.size ?? fieldSize ?? "md";
  // The ??-chain handles RadioGroup correctly (`groupCtx?.disabled`
  // is `boolean | undefined`), but the two boolean context values
  // need OR so a `false` from Field doesn't shadow a `true` from a
  // wrapping Fieldset.
  const disabled =
    disabledProp ?? groupCtx?.disabled ?? (fieldDisabled || fieldsetDisabled);
  const required = requiredProp ?? false;

  // Dev-mode usage check: a Radio outside a RadioGroup is almost
  // always a bug. Module-level dedup ensures one warn per process; the
  // production build DCEs this branch entirely (no optional-chain in
  // the NODE_ENV compare so esbuild can statically replace it).
  useEffect(() => {
    if (typeof process === "undefined") return;
    if (process.env.NODE_ENV === "production") return;
    if (groupCtx == null && !radioWithoutGroupWarned) {
      radioWithoutGroupWarned = true;
      // eslint-disable-next-line no-console
      console.warn(
        "[Radio] Rendered without a Radio.Group ancestor. Radios are " +
          "for MUTUALLY EXCLUSIVE choice — wrap multiple Radios in a " +
          "<Radio.Group> so the value can be tracked.",
      );
    }
  }, [groupCtx]);

  const chipClassName = classnames(
    "zs-radio",
    `zs-radio--${size}`,
    className,
  );

  // Per-option accessible name (wave10 🔴 a11y fix).
  //
  // Base UI's `useAriaLabelledBy` resolves a Radio's `aria-labelledby`
  // as `explicitAriaLabelledBy ?? fieldItemLabelId ?? wrappingLabelFallback`.
  // Inside a `<Field><Field.Label>…</Field.Label>` the Field provides a
  // single `labelId` (the Field.Label's id) to EVERY contained radio via
  // `FieldItemContext`, which shadows the per-radio wrapping-`<label>`
  // fallback. The result was that every radio in a Field-wrapped group
  // announced the GROUP label ("Plan") instead of its own option text
  // ("Free" / "Pro" / …) — a real WAI-ARIA radiogroup violation (the
  // group is named by the Field label; each radio must be named by its
  // option). Bare groups (no Field) worked because the fallback ran.
  //
  // Fix: when this Radio renders its own inline `label`, give the row
  // text a stable id and pass it as the chip's EXPLICIT `aria-labelledby`,
  // which is the first (winning) branch in Base UI's resolution — so the
  // per-option name beats the inherited Field labelId. A caller-supplied
  // `aria-labelledby` still wins over ours. The radiogroup itself keeps
  // the Field.Label as its name (unaffected — Field labels the group's
  // control, the RadioGroup element).
  const generatedTextId = useId();
  const hasInlineLabel = label != null;
  const rowTextId =
    ariaLabelledByProp == null && hasInlineLabel
      ? generatedTextId
      : undefined;
  const chipAriaLabelledBy = ariaLabelledByProp ?? rowTextId;

  const chip = (
    <BaseRadio.Root
      {...rest}
      ref={ref as React.Ref<HTMLElement>}
      // Pass the typed value through the Base UI prop. The `as never`
      // keeps the generic `<T>` story visible without leaking `any`.
      value={value as never}
      disabled={disabled || undefined}
      required={required || undefined}
      aria-labelledby={chipAriaLabelledBy}
      className={chipClassName}
      data-size={size}
    >
      <BaseRadio.Indicator className="zs-radio__indicator" />
    </BaseRadio.Root>
  );

  if (hasInlineLabel) {
    return (
      <SelectionRow
        base="radio"
        size={size}
        disabled={disabled}
        className={fieldClassName}
        fieldProps={fieldProps}
      >
        {chip}
        <span id={rowTextId} className="zs-radio-field__text">
          {label}
        </span>
      </SelectionRow>
    );
  }

  return chip;
}

// Cast through `unknown` so TypeScript sees the generic component +
// namespace shape without complaining about the forwardRef intermediate
// erasing the `<T>`. Same pattern used to surface AlertDialog's compound
// namespace on top of the forwardRef wrapper.
//
// Base UI renders RadioRoot as a `<span>` with `tabIndex=0` — narrow to
// `HTMLSpanElement` rather than `HTMLButtonElement` (slice-4 review fix
// item 5).
const RadioForwarded = forwardRef(RadioInner) as unknown as (<T = string>(
  props: RadioProps<T> & { ref?: React.Ref<HTMLSpanElement> },
) => React.JSX.Element) & {
  Group: typeof RadioGroup;
  displayName?: string;
};

(RadioForwarded as { displayName?: string }).displayName = "Radio";
RadioForwarded.Group = RadioGroup;

export const Radio = RadioForwarded;
export { RadioGroup };
