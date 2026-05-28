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
 *      is a usage error (warned once in dev).
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
 * Field integration mirrors Checkbox / Switch: `size` inherits via
 * `useFieldVisualSize()`, `disabled` via `useFieldDisabledContext()`,
 * explicit prop always wins. RadioGroup itself ALSO accepts `size`
 * which cascades to every contained Radio (children read via the
 * Field context fallback OR a group-local context — we use the
 * Field context for symmetry with the other primitives).
 */
import {
  createContext,
  forwardRef,
  useContext,
  useEffect,
  useRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Radio as BaseRadio } from "@base-ui/react/radio";
import { RadioGroup as BaseRadioGroup } from "@base-ui/react/radio-group";
import { useFieldDisabledContext, useFieldVisualSize } from "../Field";
import { classnames } from "../_classnames";
import { Slot } from "../_slot";

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

type BaseRadioGroupProps<V> = ComponentPropsWithRef<typeof BaseRadioGroup> & {
  value?: V;
  defaultValue?: V;
  onValueChange?: (value: V, eventDetails: unknown) => void;
};

export interface RadioGroupProps<T = string>
  extends Omit<
    BaseRadioGroupProps<T>,
    "className" | "render" | "children" | "value" | "defaultValue" | "onValueChange"
  > {
  /** Layout — `vertical` stacks (default); `horizontal` is a wrapping row. */
  orientation?: RadioOrientation;

  /** Visual size — cascades to each Radio child. */
  size?: RadioSize;

  className?: string;

  value?: T;
  defaultValue?: T;
  onValueChange?: (value: T, eventDetails: unknown) => void;

  children?: ReactNode;
}

function RadioGroupInner<T = string>(
  {
    orientation = "vertical",
    size: sizeProp,
    disabled: disabledProp,
    className,
    children,
    ...rest
  }: RadioGroupProps<T>,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  // Cascade size + disabled from the enclosing Field if not set on
  // the group directly. Same shape Input / Checkbox / Switch use.
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const size = sizeProp ?? fieldSize;
  const disabled = disabledProp ?? fieldDisabled;

  return (
    <RadioGroupContext.Provider value={{ size, disabled }}>
      <BaseRadioGroup
        // Cast keeps the typed value through Base UI's generic
        // signature without leaking `any` into our public API.
        {...(rest as ComponentPropsWithoutRef<typeof BaseRadioGroup>)}
        ref={ref}
        disabled={disabled || undefined}
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

type BaseRadioRootProps<V> = ComponentPropsWithRef<typeof BaseRadio.Root> & {
  value: V;
};

export interface RadioProps<T = string>
  extends Omit<
    BaseRadioRootProps<T>,
    "className" | "render" | "children" | "value"
  > {
  /** The discriminant value this Radio represents in the group. */
  value: T;

  /** Inherited from the enclosing RadioGroup / Field by default. */
  size?: RadioSize;

  /**
   * Render-as a custom element for the visible chip. Composes via
   * Slot — semantics still come from Base UI's RadioRoot.
   */
  asChild?: boolean;

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
  fieldProps?: ComponentPropsWithoutRef<"label">;
}

function RadioInner<T = string>(
  {
    value,
    size: sizeProp,
    asChild = false,
    className,
    label,
    fieldClassName,
    fieldProps,
    disabled: disabledProp,
    ...rest
  }: RadioProps<T>,
  ref: React.ForwardedRef<HTMLButtonElement>,
) {
  // Group context wins over field context for size — a group is a
  // tighter scope. Explicit prop still beats both.
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const groupCtx = useRadioGroupContext();

  const size: RadioSize = sizeProp ?? groupCtx?.size ?? fieldSize ?? "md";
  const disabled = disabledProp ?? groupCtx?.disabled ?? fieldDisabled;

  // Dev-mode usage check: a Radio outside a RadioGroup is almost
  // always a bug. Warn once per mount. Production builds DCE this.
  const warnedRef = useRef(false);
  useEffect(() => {
    if (typeof process === "undefined" || process.env?.NODE_ENV === "production") return;
    if (groupCtx == null && !warnedRef.current) {
      warnedRef.current = true;
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

  const chip = (
    <BaseRadio.Root
      // Same cast pattern as RadioGroup — keeps the typed `value`
      // through Base UI's generic signature.
      {...(rest as ComponentPropsWithoutRef<typeof BaseRadio.Root>)}
      ref={ref}
      // Pass the typed value through the Base UI prop.
      value={value as unknown as string}
      disabled={disabled || undefined}
      className={chipClassName}
      data-size={size}
      render={
        asChild
          ? (props, state) => (
              <Slot
                {...props}
                data-checked={state.checked || undefined}
                data-disabled={state.disabled || undefined}
                data-readonly={state.readOnly || undefined}
              />
            )
          : undefined
      }
    >
      <BaseRadio.Indicator className="zs-radio__indicator" />
    </BaseRadio.Root>
  );

  if (label != null) {
    return (
      <label
        {...fieldProps}
        className={classnames(
          "zs-radio-field",
          `zs-radio-field--${size}`,
          fieldClassName,
          fieldProps?.className,
        )}
        data-size={size}
        data-disabled={disabled || undefined}
      >
        {chip}
        <span className="zs-radio-field__text">{label}</span>
      </label>
    );
  }

  return chip;
}

// Cast through `unknown` so TypeScript sees the generic component +
// namespace shape without complaining about the forwardRef intermediate
// erasing the `<T>`. Same pattern used to surface AlertDialog's compound
// namespace on top of the forwardRef wrapper.
const RadioForwarded = forwardRef(RadioInner) as unknown as (<T = string>(
  props: RadioProps<T> & { ref?: React.Ref<HTMLButtonElement> },
) => React.JSX.Element) & {
  Group: typeof RadioGroup;
  displayName?: string;
};

(RadioForwarded as { displayName?: string }).displayName = "Radio";
RadioForwarded.Group = RadioGroup;

export const Radio = RadioForwarded;
export { RadioGroup };
