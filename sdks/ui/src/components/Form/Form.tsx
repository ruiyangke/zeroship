/*
 * Form — consolidated submit + validation surface around Base UI's
 * `<Form>`. Wraps a native `<form>` element, coordinates `Field.Root`
 * validation across descendants, exposes:
 *
 *   - `onFormSubmit(formValues, details)` — collected-values callback
 *     fired only after every Field's validation reports valid.
 *   - `errors` — server-side validation errors keyed by Field name.
 *   - `validationMode` — when each Field validates (`onSubmit` default,
 *     `onBlur`, `onChange`). Individual `<Field validationMode>` wins.
 *   - `actionsRef` — imperative `.validate()` to programmatically
 *     trigger validation (e.g. before a fetch).
 *
 * Why a wrapper over Base UI directly:
 *
 *   1. Surface narrowing — we drop `render` (the design-system layer
 *      owns the element shape; consumers don't get to swap the form
 *      out from underneath us). Slot composition would only
 *      complicate the field-collection contract Base UI's onSubmit
 *      walks.
 *   2. A `variant` axis — `default` keeps the form layout-only (no
 *      surface chrome) so it can sit inside a Card / Dialog already
 *      carrying surface. `variant="card"` opts in to an opaque
 *      surface + Card-style rim so the form CAN stand alone as a
 *      page-level container without nesting in a Card.
 *   3. A consistent `data-variant` attribute so consumer CSS can
 *      target the surfaced vs surface-less shells without poking
 *      at our internal class names.
 *
 * Anti-patterns we explicitly avoid:
 *   - No `onSuccess`/`onError` props on Form. The `onFormSubmit`
 *     callback fires only when every Field validates; server-side
 *     errors come back through the `errors` prop (a Base UI
 *     pattern) which routes them onto the right Field.
 *   - No `initialValues` prop. Forms are tree-driven — each Field
 *     owns its initial value (via the control's `defaultValue` or
 *     a controlled `value`). The Form wrapper is value-agnostic.
 *   - No "submit on Enter" toggle. Native form semantics already
 *     wire this; an Input inside a Form bound to a `type="submit"`
 *     Button submits on Enter without any wiring.
 *
 * Aria contract: every aria detail (`aria-invalid` on Fields,
 * `aria-describedby` chaining, focus restoration to the first
 * invalid field) is Base UI's job — we never overwrite. Base UI's
 * own submit handler focuses the first invalid field's control
 * after validation fails.
 *
 * ----------------------------------------------------------------------
 * `composeBaseClass` invariant:
 * ----------------------------------------------------------------------
 * The Base UI `className` prop accepts `string | ((state) => string |
 * undefined)`. The styled wrapper below composes via `composeBaseClass`
 * so our static class always wins while preserving consumer strings or
 * callbacks. Same shape Dialog / Field / Card use.
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Form as BaseForm } from "@base-ui/react/form";
import { composeBaseClass } from "../_classnames";

export type FormVariant = "default" | "card";

type BaseFormElementProps = ComponentPropsWithRef<typeof BaseForm>;

export interface FormProps<
  FormValues extends Record<string, unknown> = Record<string, unknown>,
> extends Omit<
    BaseFormElementProps,
    "render" | "className" | "onFormSubmit"
  > {
  /**
   * Validation errors returned from a server / form action. Keys MUST
   * match the `name` attribute on the corresponding `<Field>` /
   * control. Values may be a single string or an array of strings.
   * Setting / clearing an entry re-renders the matching Field with
   * `aria-invalid` and the error subtree.
   */
  errors?: BaseFormElementProps["errors"];

  /**
   * When each Field validates. `onSubmit` (default) waits for submit
   * then re-validates on every change; `onBlur` validates on focus
   * loss; `onChange` validates on every value change. An individual
   * `<Field validationMode>` wins for that Field.
   */
  validationMode?: BaseFormElementProps["validationMode"];

  /**
   * Imperative `actionsRef.current.validate(fieldName?)` — pass a
   * field name to validate just one, or call with no argument to
   * validate every Field. Useful before kicking off a manual `fetch`
   * outside the submit flow.
   *
   * Typed via the Base UI form actions shape so consumers can
   * `useRef<FormActions>(null)` against it directly.
   */
  actionsRef?: BaseFormElementProps["actionsRef"];

  /**
   * Fires only after every Field reports valid. Receives the
   * collected `{name: value}` map and a Base UI event details record
   * (with `event.preventDefault()` already called by Base UI's
   * internal handler — DO NOT call it again).
   *
   * Generic parameter is the formValues shape; defaults to a string-
   * keyed record so consumers without a typed shape still get a
   * usable callback signature.
   */
  onFormSubmit?: (
    formValues: FormValues,
    eventDetails: Parameters<
      NonNullable<BaseFormElementProps["onFormSubmit"]>
    >[1],
  ) => void;

  /**
   * Visual variant.
   * - `default` (no surface chrome) — for forms living inside an
   *   already-surfaced container (a Card, a Dialog body).
   * - `card` — opaque surface + Card-style rim. Use when the form
   *   IS the page-level surface and a Card wrapper would double
   *   the chrome.
   */
  variant?: FormVariant;

  /** Class hook for the root `<form>`. */
  className?: string;

  children?: ReactNode;
}

function FormRoot<
  FormValues extends Record<string, unknown> = Record<string, unknown>,
>(
  {
    variant = "default",
    className,
    onFormSubmit,
    ...rest
  }: FormProps<FormValues>,
  ref: React.ForwardedRef<HTMLFormElement>,
) {
  // `onFormSubmit` is typed against the consumer's generic FormValues
  // shape; Base UI types it as `Record<string, any>`. The values
  // contract is identical (each Field collects via its `name` attr) —
  // the cast is purely the generic-narrowing handshake.
  const baseOnFormSubmit = onFormSubmit as
    | BaseFormElementProps["onFormSubmit"]
    | undefined;

  return (
    <BaseForm
      ref={ref}
      onFormSubmit={baseOnFormSubmit}
      className={composeBaseClass<{}>(
        variant === "card" ? "zs-form zs-form--card" : "zs-form",
        className,
      )}
      data-variant={variant}
      {...rest}
    />
  );
}

type FormComponent = <
  FormValues extends Record<string, unknown> = Record<string, unknown>,
>(
  props: FormProps<FormValues> & React.RefAttributes<HTMLFormElement>,
) => React.ReactElement | null;

// `forwardRef` doesn't preserve a generic parameter; the outer cast
// re-narrows the callable so consumers can write
// `<Form<MyShape> onFormSubmit={…}>` and get inferred-value types in
// the callback. Same shape Base UI's own export uses internally.
export const Form = forwardRef(FormRoot) as FormComponent & {
  displayName?: string;
};
(Form as { displayName?: string }).displayName = "Form";
