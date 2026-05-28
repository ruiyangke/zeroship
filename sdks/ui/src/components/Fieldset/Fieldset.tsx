/*
 * Fieldset — namespace component for grouping related Fields under a
 * shared visible label.
 *
 *   <Fieldset>
 *     <Fieldset.Legend>Mailing address</Fieldset.Legend>
 *     <Field>…</Field>
 *     <Field>…</Field>
 *   </Fieldset>
 *
 * Renders a native `<fieldset>` whose `aria-labelledby` is auto-wired
 * to the Legend's id (Base UI wires this through a Root↔Legend
 * context, so the Legend can sit anywhere inside the fieldset — not
 * necessarily as the first child).
 *
 * Why a wrapper over Base UI directly:
 *
 *   1. Visual rhythm — `size` (sm/md/lg) maps to the Field cadence
 *      so a Fieldset's inline padding stays in step with the rest of
 *      the form rows. Selection primitives consuming `size` from a
 *      Field context don't get hit by a Fieldset (Fieldset is a
 *      grouping shell, NOT a Field), so the cascade stays clean.
 *
 *   2. Disabled cascade — the native `<fieldset disabled>` attribute
 *      cascades to descendant form controls at the AT layer AND at
 *      the layout layer (browsers natively skip disabled fieldsets
 *      when computing form values + focus chains). We forward
 *      `disabled` through and add `data-disabled` so our CSS can
 *      grey the Legend in step with the descendants. No extra
 *      JavaScript wiring needed; verified by the
 *      `Fieldset disabled cascades to nested input` aria-wiring
 *      assertion.
 *
 *   3. Surface narrowing — we drop Base UI's `render` prop. The
 *      design-system layer owns the element shape; if a consumer
 *      wants a non-fieldset container they should compose with
 *      Card / a plain div instead of swapping the element type
 *      from under our CSS.
 *
 * Legend element note (brief contingency):
 *   HTML `<legend>` must be the FIRST child of `<fieldset>` and
 *   carries native browser styling (forced flow inside the rim).
 *   Base UI renders the Legend as a `<div>` and binds it via
 *   `aria-labelledby` so the AT label survives any DOM position.
 *   We keep the Base UI `<div>` shape so consumers can place the
 *   Legend anywhere (top, between fields, bottom) — the
 *   `CustomLegendPosition` story exercises this. If a consumer
 *   needs a real `<legend>` they can pass `render={<legend />}`
 *   through the Base UI surface, but the default `<div>` is the
 *   right call for a flexible design system.
 *
 * Aria contract: every aria detail (`aria-labelledby` on the
 * fieldset, `aria-disabled` cascade onto descendant controls when
 * the fieldset is disabled) is Base UI / native HTML's job. We
 * never overwrite the wired attrs — see the spread order
 * discipline below.
 *
 * ----------------------------------------------------------------------
 * `composeBaseClass` invariant:
 * ----------------------------------------------------------------------
 * The Base UI `className` prop accepts `string | ((state) => string |
 * undefined)`. Every styled passthrough uses `composeBaseClass` so our
 * static class always wins while preserving consumer strings or
 * callbacks. Field / Dialog / Card all share this shape.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
} from "react";
import { Fieldset as BaseFieldset } from "@base-ui/react/fieldset";
import { classnames, composeBaseClass } from "../_classnames";

export type FieldsetSize = "sm" | "md" | "lg";

type BaseFieldsetRootProps = ComponentPropsWithRef<typeof BaseFieldset.Root>;

export interface FieldsetProps extends Omit<BaseFieldsetRootProps, "render"> {
  /**
   * Inline padding + gap cadence — matches the Field rhythm so a
   * Fieldset of `size="sm"` sits next to a `size="sm"` Field without
   * a vertical hiccup. Default `md`.
   */
  size?: FieldsetSize;

  /** Class hook for the root `<fieldset>`. */
  className?: string;
}

/* ─── styled passthroughs around Base UI parts ────────────────────── */

type LegendProps = ComponentPropsWithoutRef<typeof BaseFieldset.Legend>;
const FieldsetLegend = forwardRef<HTMLDivElement, LegendProps>(
  function FieldsetLegend({ className, ...rest }, ref) {
    return (
      <BaseFieldset.Legend
        ref={ref}
        className={composeBaseClass(
          "zs-fieldset__legend",
          className,
        )}
        {...rest}
      />
    );
  },
);
FieldsetLegend.displayName = "Fieldset.Legend";

/* ─── Fieldset.Root ───────────────────────────────────────────────── */

function FieldsetRoot(
  {
    size = "md",
    disabled = false,
    className,
    children,
    ...rest
  }: FieldsetProps,
  ref: React.ForwardedRef<HTMLFieldSetElement>,
) {
  return (
    <BaseFieldset.Root
      // Base UI's Fieldset.Root ref is typed `HTMLElement`; we
      // narrow at the namespace export to the natural
      // `HTMLFieldSetElement` so consumers forwarding refs land on
      // the expected element type.
      ref={ref as React.Ref<HTMLElement>}
      disabled={disabled}
      className={composeBaseClass(
        classnames(
          "zs-fieldset",
          size !== "md" ? `zs-fieldset--${size}` : null,
        ),
        className,
      )}
      data-size={size}
      data-disabled={disabled ? "" : undefined}
      {...rest}
    >
      {children}
    </BaseFieldset.Root>
  );
}

type FieldsetComponent = React.ForwardRefExoticComponent<
  Omit<FieldsetProps, "ref"> & React.RefAttributes<HTMLFieldSetElement>
> & {
  Legend: typeof FieldsetLegend;
};

export const Fieldset = forwardRef<HTMLFieldSetElement, FieldsetProps>(
  FieldsetRoot,
) as FieldsetComponent;
Fieldset.displayName = "Fieldset";
Fieldset.Legend = FieldsetLegend;
