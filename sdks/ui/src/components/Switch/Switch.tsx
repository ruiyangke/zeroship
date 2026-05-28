/*
 * Switch — binary state primitive (on / off, immediate effect).
 *
 * Wraps Base UI's `Switch.Root` + `Switch.Thumb`. The track is a
 * focusable <span> Base UI renders; the thumb is a child <span>
 * positioned absolutely inside it. A hidden <input> ships alongside
 * for form submission.
 *
 * Design guarantees (in source so the intent travels with the file):
 *
 *   1. Use Switch for BINARY settings that take effect on flip — the
 *      "wifi on", "dark mode", "notifications" idiom. If the change
 *      needs confirmation, model it as a Checkbox inside a form.
 *
 *   2. No tri-state. On / off. `defaultChecked` for uncontrolled
 *      forms; `checked` + `onCheckedChange` for controlled state.
 *
 *   3. The thumb's position carries the state (inline-start = off,
 *      inline-end = on). Color is the secondary signal. A colorblind
 *      user reads on/off from where the thumb sits, not from green
 *      vs gray.
 *
 *   4. RTL: the thumb slides toward the inline-end side when
 *      checked. CSS `translate` can't take a logical axis, so the
 *      stylesheet has explicit `[dir="rtl"]` rules that flip the
 *      sign of the translate. Verified by the RTL story capture.
 *
 *   5. Hit target ≥ 1.75rem (fine pointer), ≥ 2.75rem (coarse). The
 *      track's invisible `::before` overlay extends the tap rect so a
 *      bare track (no wrapping label) still meets the floor on coarse
 *      pointers (slice-4 review fix item 3).
 *
 *   6. Focus ring lives on the track ROOT (`.zs-switch:focus-visible`)
 *      so the canonical Field-without-component-label pattern shows
 *      a ring (slice-4 review fix item 2).
 *
 * Field integration mirrors Checkbox: `size` reads from
 * `useFieldVisualSize()`, `disabled` from `useFieldDisabledContext()`,
 * `required` from `useFieldContext()`; explicit prop always wins.
 *
 * The track itself takes `className` for custom styling — there's no
 * asChild escape hatch (selection primitives are chips with hidden
 * inputs, not button-shaped surfaces — slice-4 review fix item 1).
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Switch as BaseSwitch } from "@base-ui/react/switch";
import {
  useFieldContext,
  useFieldDisabledContext,
  useFieldVisualSize,
} from "../Field";
import { classnames } from "../_classnames";
import { SelectionRow } from "../_selection-row";

export type SwitchSize = "sm" | "md" | "lg";

type BaseSwitchRootProps = ComponentPropsWithRef<typeof BaseSwitch.Root>;

export interface SwitchProps
  extends Omit<BaseSwitchRootProps, "className" | "render" | "children"> {
  /** Visual size — small 1.5rem wide, medium 2rem (default), large 2.5rem. */
  size?: SwitchSize;

  /** Class name for the visible track. */
  className?: string;

  /**
   * Optional text label rendered to the inline-end of the track. When
   * present, the whole row becomes a single <label> so a click on the
   * text toggles the switch. Use Field.Label instead when the switch
   * lives inside a Field — the Field wires htmlFor automatically.
   */
  label?: ReactNode;

  /** Class name for the wrapping <label> row (the click surface). */
  fieldClassName?: string;

  /** Extra props for the wrapping <label> (when `label` is present). */
  fieldProps?: ComponentPropsWithRef<"label">;
}

// Base UI's SwitchRoot renders a `<span>` with `tabIndex=0`; its
// forwarded ref is typed `HTMLElement`. Narrow to `HTMLSpanElement`
// to reflect the actual rendered element (slice-4 review fix item 5).
export const Switch = forwardRef<HTMLSpanElement, SwitchProps>(function Switch(
  {
    size: sizeProp,
    className,
    label,
    fieldClassName,
    fieldProps,
    disabled: disabledProp,
    required: requiredProp,
    ...rest
  },
  ref,
) {
  // Unconditional hook calls — cascade resolution happens after.
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const fieldCtx = useFieldContext();
  const size: SwitchSize = sizeProp ?? fieldSize ?? "md";
  const disabled = disabledProp ?? fieldDisabled;
  const required = requiredProp ?? fieldCtx?.required ?? false;

  const trackClassName = classnames(
    "zs-switch",
    `zs-switch--${size}`,
    className,
  );

  const track = (
    <BaseSwitch.Root
      {...rest}
      ref={ref as React.Ref<HTMLElement>}
      disabled={disabled || undefined}
      required={required || undefined}
      className={trackClassName}
      data-size={size}
    >
      <BaseSwitch.Thumb className="zs-switch__thumb" />
    </BaseSwitch.Root>
  );

  if (label != null) {
    return (
      <SelectionRow
        base="switch"
        size={size}
        disabled={disabled}
        className={fieldClassName}
        fieldProps={fieldProps}
      >
        {track}
        <span className="zs-switch-field__text">{label}</span>
      </SelectionRow>
    );
  }

  return track;
});

Switch.displayName = "Switch";
