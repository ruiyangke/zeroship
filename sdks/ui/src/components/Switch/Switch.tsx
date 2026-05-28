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
 *      wrapping <label> when `label` is set is the click surface;
 *      the visible track stays small.
 *
 * Field integration mirrors Checkbox: `size` reads from
 * `useFieldVisualSize()`, `disabled` from `useFieldDisabledContext()`,
 * explicit prop always wins.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Switch as BaseSwitch } from "@base-ui/react/switch";
import { useFieldDisabledContext, useFieldVisualSize } from "../Field";
import { classnames } from "../_classnames";
import { Slot } from "../_slot";

export type SwitchSize = "sm" | "md" | "lg";

type BaseSwitchRootProps = ComponentPropsWithRef<typeof BaseSwitch.Root>;

export interface SwitchProps
  extends Omit<BaseSwitchRootProps, "className" | "render" | "children"> {
  /** Visual size — small 1.5rem wide, medium 2rem (default), large 2.5rem. */
  size?: SwitchSize;

  /**
   * Render-as a custom element. Composes via Slot — the consumer's
   * element receives our classNames + data attributes; the focusable
   * surface semantics still come from Base UI.
   */
  asChild?: boolean;

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
  fieldProps?: ComponentPropsWithoutRef<"label">;
}

export const Switch = forwardRef<HTMLButtonElement, SwitchProps>(function Switch(
  {
    size: sizeProp,
    asChild = false,
    className,
    label,
    fieldClassName,
    fieldProps,
    disabled: disabledProp,
    ...rest
  },
  ref,
) {
  // Unconditional hook calls — cascade resolution happens after.
  const fieldSize = useFieldVisualSize();
  const fieldDisabled = useFieldDisabledContext();
  const size: SwitchSize = sizeProp ?? fieldSize ?? "md";
  const disabled = disabledProp ?? fieldDisabled;

  const trackClassName = classnames(
    "zs-switch",
    `zs-switch--${size}`,
    className,
  );

  const track = (
    <BaseSwitch.Root
      {...rest}
      ref={ref}
      disabled={disabled || undefined}
      className={trackClassName}
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
      <BaseSwitch.Thumb className="zs-switch__thumb" />
    </BaseSwitch.Root>
  );

  if (label != null) {
    return (
      <label
        {...fieldProps}
        className={classnames(
          "zs-switch-field",
          `zs-switch-field--${size}`,
          fieldClassName,
          fieldProps?.className,
        )}
        data-size={size}
        data-disabled={disabled || undefined}
      >
        {track}
        <span className="zs-switch-field__text">{label}</span>
      </label>
    );
  }

  return track;
});

Switch.displayName = "Switch";
