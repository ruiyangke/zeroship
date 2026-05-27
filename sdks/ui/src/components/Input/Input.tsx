/*
 * Input — HIG-anchored single-line text field.
 *
 * Two surfaces in one component (informed by the slice-2 DS survey):
 *
 *   1. DECOMPOSED (canonical) — render inside a <Field> and let Base UI
 *      Field handle label / description / error / aria wiring:
 *
 *        <Field>
 *          <Field.Label>Email</Field.Label>
 *          <Input type="email" />
 *          <Field.Description>We'll never share.</Field.Description>
 *          <Field.Error match="typeMismatch">Enter a valid email.</Field.Error>
 *        </Field>
 *
 *   2. COMBINED (shorthand) — pass `label` / `description` / `error` to
 *      one-off the inline Field. Sugar; the resulting tree is identical
 *      to the decomposed form.
 *
 *        <Input label="Email" description="We'll never share."
 *               error="Invalid" type="email" />
 *
 * Why both: the survey called out shadcn / Mantine / Chakra v3 / Fluent
 * v9 all converging on a decomposed-with-combined-sugar shape. The
 * sugar pays its rent at call sites that don't need fine control over
 * subpart ordering — but the decomposed form is the canonical write.
 *
 * Anti-patterns we explicitly avoid (from the survey):
 *  - `type` overloaded with status (Geist `type="error"`). We keep
 *    `type` HTML-sacred and put status on `invalid`.
 *  - `addonBefore` / `addonAfter` outside-attached strips inside the
 *    same prop. `startSlot` / `endSlot` here live INSIDE the bordered
 *    shell; the outside-attached pattern is a future `InputGroup`.
 *  - `loadingPosition`. A spinner in `endSlot` covers the same ground.
 *  - `slots` / `slotProps` triple-prop API (MUI Base). One layer only.
 *  - Floating label / outlined-notch (MUI). HIG doesn't float labels.
 *
 * Aria contract — DO NOT BREAK:
 *   The consumer's native input props (value/defaultValue/onChange/
 *   type/placeholder/name/autoComplete/etc.) are forwarded to
 *   `<BaseField.Control>` FIRST. Base UI merges them with its
 *   auto-wired aria attributes (id, aria-describedby, aria-invalid,
 *   aria-labelledby) and re-emits the combined set via the `render`
 *   callback as `controlProps`. The inner <input> spreads
 *   `{...controlProps}` so Base UI's wiring always wins; we then layer
 *   our shell-only overrides (className, aria-invalid for the
 *   styling-only `invalid` flag, aria-required for AT-script
 *   visibility). This is the INVERSE of Button.tsx, where our
 *   internal props beat the caller — here, Base UI's wiring is
 *   what we protect.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type CSSProperties,
  type ReactNode,
  type Ref,
} from "react";
import { Field as BaseField } from "@base-ui/react/field";
import { Field, useFieldContext } from "../Field";

export type InputSize = "sm" | "md" | "lg";
export type InputVariant = "outline" | "filled" | "plain";

export interface InputProps
  extends Omit<
    ComponentPropsWithoutRef<"input">,
    "size" | "prefix" | "children"
  > {
  /** Visual size — sm 32 / md 40 / lg 48. Defaults to `md`. */
  size?: InputSize;

  /** Visual variant. Defaults to `outline`. */
  variant?: InputVariant;

  /** Leading adornment INSIDE the bordered shell (icon, prefix). */
  startSlot?: ReactNode;

  /** Trailing adornment INSIDE the bordered shell (icon, button, …). */
  endSlot?: ReactNode;

  /**
   * Force the invalid visual state independent of Field validation.
   * Useful when an external library (server validation, Standard
   * Schema) owns the error truth. Mirrors `aria-invalid` onto the
   * native input.
   */
  invalid?: boolean;

  /**
   * Combined-shorthand label. When set (along with `description` /
   * `error`), the component renders an inline <Field> wrapper around
   * itself for the one-off case. Inside an existing <Field>, prefer
   * <Field.Label> instead.
   */
  label?: ReactNode;

  /** Combined-shorthand description. */
  description?: ReactNode;

  /**
   * Combined-shorthand error. A ReactNode renders as the error message;
   * `true` flips the invalid styling without text. Setting any error
   * (truthy) forces `invalid` on the inline Field.
   */
  error?: ReactNode | boolean;

  /** Escape hatch for the bordered-shell wrapper element. */
  wrapperClassName?: string;
  wrapperProps?: ComponentPropsWithoutRef<"div">;
  wrapperStyle?: CSSProperties;
}

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

/* ─── inner shell — the styled bordered box w/ slots + control ──────── */

type ControlRenderProps = ComponentPropsWithRef<"input">;

interface InnerProps extends InputProps {
  /** Internal: the `aria-invalid` source from the outer combined wrapper. */
  forcedInvalid?: boolean;
}

const InputInner = forwardRef<HTMLInputElement, InnerProps>(function InputInner(
  {
    size: sizeProp,
    variant = "outline",
    startSlot,
    endSlot,
    invalid,
    forcedInvalid,
    className,
    wrapperClassName,
    wrapperProps,
    wrapperStyle,
    required: requiredProp,
    ...rest
  },
  ref,
) {
  const fieldCtx = useFieldContext();
  // Inherit size from the enclosing Field if the consumer didn't set one;
  // fall back to medium.
  const size: InputSize = sizeProp ?? fieldCtx?.size ?? "md";
  // Inherit required from the enclosing Field when not set explicitly. The
  // HTML attribute on <input> is what drives `aria-required`, so we mirror.
  const required = requiredProp ?? fieldCtx?.required ?? false;
  const isInvalid = invalid === true || forcedInvalid === true;

  return (
    <BaseField.Control
      // Pass every native input prop (value/defaultValue/onChange/
      // type/placeholder/autoComplete/name/etc.) through to Base UI's
      // FieldControl so its controlled/uncontrolled state machine and
      // ValidityState wiring see them. Base UI then re-emits them via
      // the `render` callback's `controlProps`.
      {...(rest as Record<string, unknown>)}
      // The control is the source of truth for value/onChange/etc.
      // `render` projects our styled shell while preserving every aria
      // attribute Base UI builds (id / aria-describedby / aria-invalid /
      // aria-required / etc.).
      required={required}
      render={(controlProps: ControlRenderProps, state) => {
        // state: { disabled, touched, dirty, valid, filled, focused }
        const dataInvalid =
          state.valid === false || isInvalid ? "" : undefined;
        const dataDisabled = state.disabled ? "" : undefined;
        const dataFocused = state.focused ? "" : undefined;
        const dataFilled = state.filled ? "" : undefined;
        const dataReadonly =
          (controlProps as { readOnly?: boolean }).readOnly ? "" : undefined;

        return (
          <div
            {...wrapperProps}
            style={{ ...wrapperStyle, ...(wrapperProps?.style ?? {}) }}
            className={classnames(
              "zs-input",
              `zs-input--${variant}`,
              `zs-input--${size}`,
              wrapperClassName,
            )}
            data-variant={variant}
            data-size={size}
            data-invalid={dataInvalid}
            data-disabled={dataDisabled}
            data-readonly={dataReadonly}
            data-focused={dataFocused}
            data-filled={dataFilled}
          >
            {startSlot != null ? (
              <span
                className="zs-input__slot zs-input__slot--start"
                aria-hidden="true"
              >
                {startSlot}
              </span>
            ) : null}
            {/*
             * Base UI's `controlProps` already merges the consumer's
             * native input props (we forwarded them via `{...rest}` on
             * `<BaseField.Control>` above) WITH the auto-wired aria
             * attributes (id, aria-describedby, aria-invalid, aria-labelledby).
             *
             * Spread `controlProps` first; our shell-specific className
             * comes after via the dedicated prop. The aria-invalid and
             * aria-required overrides at the bottom are belt-and-braces:
             * `invalid` is a styling-only flag that doesn't go through
             * Field's ValidityState (so controlProps won't carry it);
             * `required` exposes the attribute that browsers populate
             * into the ax tree but don't expose as an attribute, so AT
             * scripts (and axe) can see it directly.
             */}
            <input
              {...controlProps}
              ref={ref as Ref<HTMLInputElement>}
              className={classnames(
                "zs-input__control",
                className,
                (controlProps as { className?: string }).className,
              )}
              aria-invalid={
                isInvalid || controlProps["aria-invalid"] || undefined
              }
              aria-required={required || undefined}
            />
            {endSlot != null ? (
              <span className="zs-input__slot zs-input__slot--end">
                {endSlot}
              </span>
            ) : null}
          </div>
        );
      }}
    />
  );
});

/* ─── public Input ──────────────────────────────────────────────────── */

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  props,
  ref,
) {
  const { label, description, error, ...rest } = props;
  const hasCombined =
    label != null || description != null || error != null;

  if (!hasCombined) {
    return <InputInner {...rest} ref={ref} />;
  }

  const errorIsTruthy = error !== undefined && error !== null && error !== false;
  const errorMessage =
    error !== true && error !== false && error != null ? error : null;
  // The inline Field's `invalid` is whatever the caller explicitly
  // requested or whatever the `error` payload implies. We feed it into
  // BaseField.Root so Field state matches the shorthand intent.
  const inlineInvalid = props.invalid === true || errorIsTruthy;

  return (
    <Field
      invalid={inlineInvalid || undefined}
      required={props.required}
    >
      {label != null ? <Field.Label>{label}</Field.Label> : null}
      <InputInner {...rest} ref={ref} forcedInvalid={inlineInvalid} />
      {description != null ? (
        <Field.Description>{description}</Field.Description>
      ) : null}
      {errorMessage != null ? (
        // `match` defaults to a falsy ValidityState lookup; passing the
        // boolean `true` lets us drive visibility from the prop instead
        // of native validity.
        <Field.Error match>{errorMessage}</Field.Error>
      ) : null}
    </Field>
  );
});
Input.displayName = "Input";
