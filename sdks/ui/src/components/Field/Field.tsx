/*
 * Field — namespace component for HIG-anchored form-frame composition.
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
 *
 * Aria wiring stays Base UI's job. `Field.Label` auto-binds `htmlFor`;
 * `Field.Description`/`Field.Error` auto-extend `aria-describedby`; the
 * control gets `aria-invalid` when `match` fires. We never overwrite
 * those attrs — see Input.tsx for the spread order discipline.
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

export type FieldOrientation = "vertical" | "horizontal";
export type FieldSize = "sm" | "md" | "lg";

type BaseFieldRootProps = ComponentPropsWithRef<typeof BaseField.Root>;

export interface FieldProps extends Omit<BaseFieldRootProps, "className"> {
  /**
   * Label-to-control axis. Default `vertical` (label above control —
   * the HIG-standard form-row). Use `horizontal` for inspector-style
   * dense forms (label left, control + helper right).
   */
  orientation?: FieldOrientation;

  /**
   * Visual size — sets a `--zs-field-control-size` for descendants.
   * A contained `<Input>` that doesn't set its own `size` inherits
   * from here.
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

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

/* ─── styled wrappers around Base UI parts ───────────────────────────── */

// Base UI's `className` is `string | ((state) => string | undefined)`. We
// preserve that surface by composing our own static class with the
// caller's (string or callback). Callbacks become wrapping callbacks so
// our class always wins; strings concat.
function composeBaseClass<S>(
  ours: string,
  theirs: string | ((state: S) => string | undefined) | undefined,
): string | ((state: S) => string | undefined) {
  if (theirs == null) return ours;
  if (typeof theirs === "string") return classnames(ours, theirs);
  return (state: S) => classnames(ours, theirs(state));
}

type LabelProps = ComponentPropsWithoutRef<typeof BaseField.Label>;
const FieldLabel = forwardRef<HTMLLabelElement, LabelProps>(
  function FieldLabel({ className, ...rest }, ref) {
    return (
      <BaseField.Label
        // Cast: Base UI's ref is HTMLElement; we narrow to HTMLLabelElement
        // because Label is a native <label> by default.
        ref={ref as unknown as React.Ref<HTMLElement>}
        className={composeBaseClass("zs-field__label", className)}
        {...rest}
      />
    );
  },
);

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

type ErrorProps = ComponentPropsWithoutRef<typeof BaseField.Error>;
const FieldError = forwardRef<HTMLDivElement, ErrorProps>(
  function FieldError({ className, ...rest }, ref) {
    return (
      <BaseField.Error
        ref={ref}
        className={composeBaseClass("zs-field__error", className)}
        {...rest}
      />
    );
  },
);

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

/* ─── Field.Root ─────────────────────────────────────────────────────── */

function FieldRoot(
  {
    orientation = "vertical",
    size,
    required = false,
    className,
    children,
    ...rest
  }: FieldProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const ctxValue = useMemo<FieldContextValue>(
    () => ({ required, size, orientation }),
    [required, size, orientation],
  );

  return (
    <FieldContext.Provider value={ctxValue}>
      <BaseField.Root
        ref={ref}
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
};

export const Field = forwardRef<HTMLDivElement, FieldProps>(
  FieldRoot,
) as FieldComponent;
Field.displayName = "Field";
Field.Label = FieldLabel;
Field.Description = FieldDescription;
Field.Error = FieldError;
Field.Required = FieldRequired;
