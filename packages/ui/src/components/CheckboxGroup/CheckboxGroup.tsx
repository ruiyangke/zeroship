/*
 * CheckboxGroup — parent state manager for a set of <Checkbox> items.
 *
 * Wraps Base UI's headless `CheckboxGroup` (from `@base-ui/react/checkbox-group`).
 * The group OWNS the value array (the names of checked items); each
 * contained Checkbox carries only its `name` and the group toggles it
 * in / out of the value array on click.
 *
 *   <CheckboxGroup defaultValue={["news"]}>
 *     <Checkbox label="Newsletter" name="news" />
 *     <Checkbox label="Product updates" name="product" />
 *     <Checkbox label="Beta invites" name="beta" />
 *   </CheckboxGroup>
 *
 * Design guarantees encoded:
 *
 *   1. Layout-only primitive. CheckboxGroup paints NOTHING of its own —
 *      it's a flex container with a gap. Children render the chips, the
 *      labels, and any focus / forced-colors / RTL treatment they need.
 *      The group's job is state plumbing + layout flow.
 *
 *   2. Two orientations: `vertical` (the default — flex column) and
 *      `horizontal` (flex row with wrap). No `column-count` prop, no
 *      grid layout primitive — consumers compose grid themselves when
 *      they want columns (api-design-guidelines.md: keep the primitive
 *      narrow; consumers can always wrap).
 *
 *   3. No `name` prop on the group. Form-name lives on the individual
 *      Checkbox children — that keeps a single composed Field around a
 *      CheckboxGroup form-submitting cleanly with multiple checkbox
 *      values (one per checked child).
 *
 *   4. `disabled` cascades to every contained Checkbox via Base UI's
 *      CheckboxGroupContext — the wrapper just forwards the prop.
 *
 *   5. RTL by way of logical properties on the layout (the layout uses
 *      `flex-direction: row` for horizontal; logical inset/inline gaps
 *      handle the LTR/RTL flip in the children).
 *
 *   6. Reduced motion: nothing to disable. The group paints no
 *      transitions — children inherit reduced-motion overrides via the
 *      root selector mirror.
 *
 *   7. Forced colors: nothing to mirror at this level. The chips paint
 *      their own forced-colors surface; the group is invisible chrome.
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { CheckboxGroup as BaseCheckboxGroup } from "@base-ui/react/checkbox-group";
import { useFieldDisabledContext } from "../Field";
import { useFieldsetDisabledContext } from "../Fieldset";
import { classnames } from "../_classnames";

export type CheckboxGroupOrientation = "vertical" | "horizontal";
export type CheckboxGroupValue = string[];

type BaseCheckboxGroupProps = ComponentPropsWithRef<typeof BaseCheckboxGroup>;

export interface CheckboxGroupProps
  extends Omit<
    BaseCheckboxGroupProps,
    "className" | "render" | "value" | "defaultValue" | "onValueChange" | "children"
  > {
  /**
   * Controlled value — names of every Checkbox child that should be
   * ticked. Provide together with `onValueChange`. Mutually exclusive
   * with `defaultValue`; if both are set the controlled `value` wins.
   */
  value?: CheckboxGroupValue;

  /**
   * Uncontrolled initial value — names of the Checkbox children ticked
   * on first render. Drop alongside `onValueChange` if a consumer wants
   * to observe changes without owning the value.
   */
  defaultValue?: CheckboxGroupValue;

  /**
   * Fires every time the group's checked set changes. The first
   * argument is the next value array (order matches click order — Base
   * UI appends new selections and removes by value).
   */
  onValueChange?: (
    value: CheckboxGroupValue,
    eventDetails: unknown,
  ) => void;

  /**
   * Layout flow — `vertical` stacks (default); `horizontal` is a
   * wrapping inline row. Consumers wanting a grid wrap the group in
   * their own grid container — the primitive stays narrow.
   */
  orientation?: CheckboxGroupOrientation;

  /**
   * Disable every contained Checkbox. Cascades through Base UI's
   * CheckboxGroupContext to children with no per-checkbox `disabled`
   * override. Inherits from a wrapping `<Field disabled>` /
   * `<Fieldset disabled>` if not set here.
   */
  disabled?: boolean;

  /** Class name applied to the group `<div>`. */
  className?: string;

  /**
   * Children — typically a sequence of `<Checkbox />` with `name` set
   * to a discriminant string for each option. Any non-Checkbox node
   * (descriptions, dividers) renders inline; only Base UI Checkboxes
   * participate in the value array.
   */
  children: ReactNode;
}

/**
 * `CheckboxGroup` — parent state manager for grouped checkboxes.
 *
 * The group is a Base UI `<div>` with the CheckboxGroupContext provider
 * baked in. Children are arbitrary; the Checkboxes inside read the
 * group's value array and emit their own state via the context.
 */
export const CheckboxGroup = forwardRef<HTMLDivElement, CheckboxGroupProps>(
  function CheckboxGroup(
    {
      value,
      defaultValue,
      onValueChange,
      orientation = "vertical",
      disabled: disabledProp,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    // Cascade disabled from a wrapping Field / Fieldset when the prop
    // isn't set. Mirrors Radio/RadioGroup. OR the two booleans rather
    // than ??-chain — `??` would short-circuit on a legitimate `false`
    // from the inner Field and never consult the outer Fieldset.
    const fieldDisabled = useFieldDisabledContext();
    const fieldsetDisabled = useFieldsetDisabledContext();
    const disabled = disabledProp ?? (fieldDisabled || fieldsetDisabled);

    return (
      <BaseCheckboxGroup
        {...rest}
        ref={ref}
        value={value}
        defaultValue={defaultValue}
        onValueChange={onValueChange}
        disabled={disabled || undefined}
        className={classnames(
          "zs-checkbox-group",
          `zs-checkbox-group--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      >
        {children}
      </BaseCheckboxGroup>
    );
  },
);
CheckboxGroup.displayName = "CheckboxGroup";
