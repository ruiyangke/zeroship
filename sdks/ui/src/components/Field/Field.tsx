/*
 * Field — namespace component for form-frame composition.
 *
 * Wraps Base UI's `Field.*` headless primitives so consumers can write the
 * canonical decomposed form-row:
 *
 *   <Field>
 *     <Field.Label>Email <Field.Required /></Field.Label>
 *     <Input type="email" />
 *     <Field.Description>We'll never share it.</Field.Description>
 *     <Field.Error match="typeMismatch">Enter a valid email.</Field.Error>
 *   </Field>
 *
 * Why a wrapper over Base UI directly:
 *
 *   1. Visual rhythm — vertical / horizontal orientations expressed as
 *      modifier classes so descendants don't need to know how the shell
 *      lays them out.
 *   2. `required` propagation — Base UI doesn't carry a `required`
 *      attribute on `Field.Root`; the input owns it natively (drives
 *      `aria-required` automatically). We mirror that prop onto a local
 *      React context so `Field.Required` (the visible indicator glyph)
 *      can render at the label, and so combined-shorthand `<Input
 *      label …>` can pick the requiredness up from the enclosing Field.
 *   3. Inherited size — a `Field size="sm"` cascades to a contained
 *      `<Input>` that doesn't set its own size. Same shape as Fluent's
 *      Field.size → Input.size pattern.
 *   4. Inherited `disabled` — Base UI's Field.Root `disabled` greys the
 *      whole row at the AT layer, but Input still needs to read it for
 *      its visual state. We mirror it onto context (item 8 of the
 *      slice-2 review fix brief).
 *
 * Aria wiring stays Base UI's job. `Field.Label` auto-binds `htmlFor`;
 * `Field.Description`/`Field.Error` auto-extend `aria-describedby`; the
 * control gets `aria-invalid` when `match` fires. We never overwrite
 * those attrs — see Input.tsx for the spread order discipline.
 *
 * ----------------------------------------------------------------------
 * `composeBaseClass` invariant (slice-2 review fix item 25):
 * ----------------------------------------------------------------------
 * Base UI's `className` prop accepts a `string | ((state) => string |
 * undefined)`. Every styled passthrough below uses `composeBaseClass`
 * so our static class always wins while preserving whatever the consumer
 * passes — string concats, callbacks wrap. Field.Required is the lone
 * exception (it's a native <span>, not a Base UI part, so its className
 * is just a string).
 */
import {
  createContext,
  forwardRef,
  useContext,
  useMemo,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Field as BaseField } from "@base-ui/react/field";
import { classnames, composeBaseClass } from "../_classnames";

export type FieldOrientation = "vertical" | "horizontal";
export type FieldSize = "sm" | "md" | "lg";

type BaseFieldRootProps = ComponentPropsWithRef<typeof BaseField.Root>;

export interface FieldProps extends Omit<BaseFieldRootProps, "className"> {
  /**
   * Label-to-control axis. Default `vertical` (label above control —
   * the standard form-row). Use `horizontal` for inspector-style
   * dense forms (label left, control + helper right).
   */
  orientation?: FieldOrientation;

  /**
   * Visual size — cascades via FieldContext to a contained Input that
   * doesn't set its own size. There is intentionally no
   * `--zs-field-control-size` CSS custom property (descendants read
   * size from React context, then choose their own size tokens).
   */
  size?: FieldSize;

  /**
   * Marks the field as required. Drives the `<Field.Required />`
   * indicator's visible state and is forwarded onto the inner control
   * (via Input or via the consumer's explicit `required` prop on
   * `Field.Control`). Base UI's `Field.Root` itself takes no
   * `required` — the HTML attribute on the control is what drives
   * `aria-required`, which is precisely what we want.
   */
  required?: boolean;

  /** Class hook for the root wrapper. */
  className?: string;
}

interface FieldContextValue {
  required: boolean;
  disabled: boolean;
  size: FieldSize | undefined;
  orientation: FieldOrientation;
}

const FieldContext = createContext<FieldContextValue | null>(null);

/**
 * Read the Field context. Returns `null` when used outside of a Field
 * (so combined-shorthand consumers don't crash). Internal to the
 * package — not exported from the public surface.
 */
export function useFieldContext(): FieldContextValue | null {
  return useContext(FieldContext);
}

/* ─── styled wrappers around Base UI parts ───────────────────────────── */

// `composeBaseClass` was hoisted to `../_classnames` (AlertDialog
// review-fix item 10, Phase 2.C). Same shape — see file-header
// invariant note above.

type LabelProps = ComponentPropsWithoutRef<typeof BaseField.Label>;
const FieldLabel = forwardRef<HTMLLabelElement, LabelProps>(
  function FieldLabel({ className, ...rest }, ref) {
    return (
      <BaseField.Label
        // Base UI's Label ref is typed `HTMLElement`; ours narrows to
        // `HTMLLabelElement` because Label is a native <label> by
        // default. One safe widening cast keeps the public type clean.
        ref={ref as React.Ref<HTMLElement>}
        className={composeBaseClass("zs-field__label", className)}
        {...rest}
      />
    );
  },
);
FieldLabel.displayName = "Field.Label";

type DescriptionProps = ComponentPropsWithoutRef<typeof BaseField.Description>;
const FieldDescription = forwardRef<HTMLParagraphElement, DescriptionProps>(
  function FieldDescription({ className, ...rest }, ref) {
    return (
      <BaseField.Description
        ref={ref}
        className={composeBaseClass("zs-field__description", className)}
        {...rest}
      />
    );
  },
);
FieldDescription.displayName = "Field.Description";

type ErrorProps = ComponentPropsWithoutRef<typeof BaseField.Error>;
const FieldError = forwardRef<HTMLDivElement, ErrorProps>(
  function FieldError({ className, ...rest }, ref) {
    return (
      // Base UI's FieldError implementation (verified against
      // @base-ui/react@1.4.1 source — node_modules/.pnpm/@base-ui+
      // react@1.4.1/.../field/error/FieldError.js) does NOT add
      // `role`, `aria-live`, or `aria-atomic` itself. Without those,
      // dynamic validity errors (e.g. typeMismatch firing after the
      // user types) are not announced by screen readers. We add the
      // live-region semantics here; spreading `...rest` LAST lets a
      // consumer override on a case-by-case basis.
      <BaseField.Error
        ref={ref}
        role="alert"
        aria-live="polite"
        aria-atomic="true"
        className={composeBaseClass("zs-field__error", className)}
        {...rest}
      />
    );
  },
);
FieldError.displayName = "Field.Error";

type ControlProps = ComponentPropsWithoutRef<typeof BaseField.Control>;
const FieldControl = forwardRef<HTMLInputElement, ControlProps>(
  function FieldControl({ className, ...rest }, ref) {
    return (
      <BaseField.Control
        // Base UI's FieldControl ref is `HTMLElement`. We narrow at
        // the namespace export so consumers calling `<Field.Control>`
        // without our `<Input>` shell still get the expected
        // `HTMLInputElement` ref type.
        ref={ref as React.Ref<HTMLElement>}
        className={composeBaseClass("zs-field__control", className)}
        {...rest}
      />
    );
  },
);
FieldControl.displayName = "Field.Control";

type ItemProps = ComponentPropsWithoutRef<typeof BaseField.Item>;
const FieldItem = forwardRef<HTMLDivElement, ItemProps>(
  function FieldItem({ className, ...rest }, ref) {
    return (
      <BaseField.Item
        ref={ref}
        className={composeBaseClass("zs-field__item", className)}
        {...rest}
      />
    );
  },
);
FieldItem.displayName = "Field.Item";

// Field.Validity is a render-prop subpart — no className, no ref. We
// re-export it directly so consumers can compose validity-driven UI
// without reaching into `@base-ui/react/field`.
const FieldValidity = BaseField.Validity;

export interface FieldRequiredProps
  extends ComponentPropsWithoutRef<"span"> {
  /**
   * Rendered instead of the required-symbol when the enclosing Field
   * is NOT marked `required`. Useful for the "(optional)" affordance
   * Polaris and Carbon both use.
   */
  fallback?: ReactNode;
}

/**
 * Visible required-indicator glyph. Reads the enclosing Field's
 * `required` state from context. Hidden from the AT tree
 * (`aria-hidden="true"`) because `aria-required` on the control is
 * what screen readers announce — duplicating it as text is noise.
 *
 * Default children: an asterisk `*`. The brief proposed a
 * `--zs-field-required-symbol` CSS custom property, but no CSS
 * actually reads it — the glyph is React children, so consumers
 * override per-instance with `<Field.Required>†</Field.Required>`.
 *
 * className uses plain `classnames` (not `composeBaseClass`) because
 * this is a native `<span>`, not a Base UI part — the callback
 * className signature doesn't apply. Asymmetric on purpose; see
 * the `composeBaseClass` invariant at the top of this file.
 */
const FieldRequired = forwardRef<HTMLSpanElement, FieldRequiredProps>(
  function FieldRequired({ fallback, className, children, ...rest }, ref) {
    const ctx = useFieldContext();
    const isRequired = ctx?.required ?? false;

    if (!isRequired) {
      if (fallback == null) return null;
      return (
        <span
          ref={ref}
          className={classnames(
            "zs-field__required",
            "zs-field__required--fallback",
            className,
          )}
          {...rest}
        >
          {fallback}
        </span>
      );
    }

    return (
      <span
        ref={ref}
        className={classnames("zs-field__required", className)}
        aria-hidden="true"
        {...rest}
      >
        {children ?? "*"}
      </span>
    );
  },
);
FieldRequired.displayName = "Field.Required";

/* ─── Field.Root ─────────────────────────────────────────────────────── */

function FieldRoot(
  {
    orientation = "vertical",
    size,
    required = false,
    disabled = false,
    className,
    children,
    ...rest
  }: FieldProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const ctxValue = useMemo<FieldContextValue>(
    () => ({ required, disabled, size, orientation }),
    [required, disabled, size, orientation],
  );

  return (
    <FieldContext.Provider value={ctxValue}>
      <BaseField.Root
        ref={ref}
        disabled={disabled}
        className={classnames(
          "zs-field",
          `zs-field--${orientation}`,
          size ? `zs-field--${size}` : null,
          className,
        )}
        data-orientation={orientation}
        data-size={size}
        {...rest}
      >
        {children}
      </BaseField.Root>
    </FieldContext.Provider>
  );
}

type FieldComponent = React.ForwardRefExoticComponent<
  Omit<FieldProps, "ref"> & React.RefAttributes<HTMLDivElement>
> & {
  Label: typeof FieldLabel;
  Description: typeof FieldDescription;
  Error: typeof FieldError;
  Required: typeof FieldRequired;
  Control: typeof FieldControl;
  Item: typeof FieldItem;
  Validity: typeof FieldValidity;
};

export const Field = forwardRef<HTMLDivElement, FieldProps>(
  FieldRoot,
) as FieldComponent;
Field.displayName = "Field";
Field.Label = FieldLabel;
Field.Description = FieldDescription;
Field.Error = FieldError;
Field.Required = FieldRequired;
Field.Control = FieldControl;
Field.Item = FieldItem;
Field.Validity = FieldValidity;
