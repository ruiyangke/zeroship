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
 *   `CustomLegendPosition` story exercises this. Element-swap via
 *   Base UI's `render` is intentionally NOT exposed on the wrapper
 *   (`FieldsetProps` / `FieldsetLegendProps` `Omit<…,"render">`):
 *   the design-system layer owns the shape, and a consumer who
 *   needs a different container should compose with Card / a
 *   plain div instead.
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
  createContext,
  forwardRef,
  useContext,
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

  /**
   * Disables the entire group. The native `<fieldset disabled>`
   * attribute cascades to descendant form controls at the AT +
   * layout layers; the wrapper additionally propagates the signal
   * through a `FieldsetDisabledContext` so non-native Base UI
   * primitives (Checkbox / Switch / Radio / Toggle) grey out in
   * step. When `false` AND a wrapping Fieldset is disabled, the
   * outer disabled state still wins (nested cascade); pass
   * `disabled` explicitly only when you mean to disable THIS
   * fieldset. Default `false`.
   */
  disabled?: boolean;

  /**
   * Group children — typically a `Fieldset.Legend` plus one or
   * more `Field` / control rows. The Legend can sit anywhere
   * inside the fieldset; Base UI wires `aria-labelledby` via id,
   * not DOM order.
   */
  children?: React.ReactNode;
}

/* ─── Fieldset disabled context ───────────────────────────────────── *
 *
 * Native `<fieldset disabled>` cascades to descendant native form
 * controls at the AT + layout layers, but our Checkbox / Switch /
 * Radio / Toggle visible roots are non-native chrome (`<span>` /
 * `<button>`-shaped Base UI parts) whose `disabled` is driven by the
 * React prop, NOT by the native form-control cascade. Without this
 * context, a `<Fieldset disabled>` would render greyed-out chrome
 * for `<Input>` (via the underlying disabled native input) but leave
 * a Checkbox / Switch / Radio / Toggle visibly enabled — even though
 * the hidden form control underneath gets the native cascade.
 *
 * We expose a tiny boolean context the four selection primitives
 * consume alongside `useFieldDisabledContext()`. The cascade rule
 * inside each primitive becomes:
 *   `disabled = explicitProp ?? groupCtx?.disabled ?? fieldDisabled ?? fieldsetDisabled`
 * — explicit prop wins, then any wrapping group (RadioGroup /
 * ToggleGroup), then a wrapping Field, then a wrapping Fieldset.
 */
const FieldsetDisabledContext = createContext<boolean>(false);

/**
 * Read whether the nearest enclosing Fieldset is disabled. Returns
 * `false` outside of one. Selection primitives compose this with
 * the Field disabled context — see Fieldset.tsx file-header note.
 */
export function useFieldsetDisabledContext(): boolean {
  return useContext(FieldsetDisabledContext);
}

/* ─── styled passthroughs around Base UI parts ────────────────────── */

type LegendProps = ComponentPropsWithoutRef<typeof BaseFieldset.Legend>;

/**
 * Props for `Fieldset.Legend` — the visible group label.
 *
 * Renders Base UI's `<div>`-shaped Legend (the element shape is
 * fixed at the design-system layer; `render` is intentionally
 * omitted). Base UI auto-assigns the Legend an id and wires it
 * into the parent Fieldset's `aria-labelledby` via context, so
 * the Legend can sit anywhere inside the fieldset — first child,
 * between Fields, or at the bottom via `order` — and the
 * accessible name still survives.
 *
 * Accepts every HTML attribute applicable to a `<div>` (e.g.
 * `className`, `style`, `data-testid`, `id`); inherits Base UI's
 * own state attrs (`data-disabled`) when the wrapping Fieldset
 * is disabled.
 */
export type FieldsetLegendProps = Omit<LegendProps, "render">;
const FieldsetLegend = forwardRef<HTMLDivElement, FieldsetLegendProps>(
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
    /*
     * Sift `aria-label` / `aria-labelledby` / `aria-describedby`
     * out of `...rest` so they do NOT spread onto BaseFieldset.Root
     * with potentially-`undefined` values. Base UI auto-wires
     * `aria-labelledby` to the Legend's id via RootContext; if a
     * consumer renders `<Fieldset aria-labelledby={undefined}>`,
     * a bare `{...rest}` spread lands `aria-labelledby={undefined}`
     * AFTER Base UI's merged props and CLOBBERS the wired id. Same
     * footgun a defined `aria-labelledby={"some-id"}` raises —
     * silently replacing the Legend binding without composing.
     *
     * Mirror of the Combobox / Select pattern: sift the aria props,
     * spread `...rest` (which now excludes them), then re-apply
     * each aria attr ONLY when the caller actually passed a defined
     * value. Undefined values stay out of the DOM and the
     * Legend-id binding survives untouched.
     */
    "aria-label": ariaLabel,
    "aria-labelledby": ariaLabelledBy,
    "aria-describedby": ariaDescribedBy,
    ...rest
  }: FieldsetProps,
  ref: React.ForwardedRef<HTMLFieldSetElement>,
) {
  // Inherit a wrapping Fieldset's disabled state — when a
  // `<Fieldset disabled>` wraps a non-disabled inner Fieldset, the
  // inner one should still report disabled to its selection
  // descendants AND to Base UI's Root (so its `data-disabled` chip
  // and the descendant native-cascade chain both pick up the
  // ancestor signal). Native cascade also handles the DOM at the
  // outer fieldset level; this `effectiveDisabled` mirrors the
  // signal into the inner Root + context so the styling /
  // metadata never disagree with the browser-cascaded behavior.
  const parentFieldsetDisabled = useContext(FieldsetDisabledContext);
  const effectiveDisabled = disabled || parentFieldsetDisabled;

  // Only apply each aria attr when defined (mirror of Combobox /
  // Select sift pattern — preserves Base UI's auto-wired ids when
  // the caller passes nothing).
  const ariaProps: {
    "aria-label"?: string;
    "aria-labelledby"?: string;
    "aria-describedby"?: string;
  } = {};
  if (ariaLabel !== undefined) ariaProps["aria-label"] = ariaLabel;
  if (ariaLabelledBy !== undefined)
    ariaProps["aria-labelledby"] = ariaLabelledBy;
  if (ariaDescribedBy !== undefined)
    ariaProps["aria-describedby"] = ariaDescribedBy;

  return (
    <FieldsetDisabledContext.Provider value={effectiveDisabled}>
      <BaseFieldset.Root
        // Base UI's Fieldset.Root ref is typed `HTMLElement`; we
        // narrow at the namespace export to the natural
        // `HTMLFieldSetElement` so consumers forwarding refs land on
        // the expected element type.
        ref={ref as React.Ref<HTMLElement>}
        disabled={effectiveDisabled}
        // Base UI's FieldsetRoot consumes `disabled` purely as context
        // + a `data-disabled` state attribute — it does NOT emit the
        // native `disabled` attribute on the `<fieldset>`. Without the
        // native attribute the browser-level cascade never fires, so a
        // plain non-Base-UI descendant (e.g. our `<Button>`) inside a
        // `<Fieldset disabled>` stays fully interactive — a real
        // accessibility bug, and a contradiction of this file's own
        // "native `<fieldset disabled>` cascade" contract. We use Base
        // UI's `render` prop internally (still omitted from the public
        // wrapper surface) to emit a native `<fieldset>` that carries
        // the native `disabled` attribute when disabled, restoring the
        // browser cascade to every interactive descendant — native
        // controls AND non-native chrome alike. Base UI's own merged
        // props (aria-labelledby, data-disabled, refs, className) are
        // spread first so we never clobber the wired attrs. See the
        // `DisabledCascade` story regression assertion (the Button
        // toBeDisabled() check fails pre-fix).
        render={(props, state) => (
          <fieldset {...props} disabled={state.disabled || undefined} />
        )}
        className={composeBaseClass(
          classnames(
            "zs-fieldset",
            size !== "md" ? `zs-fieldset--${size}` : null,
          ),
          className,
        )}
        data-size={size}
        data-disabled={effectiveDisabled ? "" : undefined}
        {...rest}
        {...ariaProps}
      >
        {children}
      </BaseFieldset.Root>
    </FieldsetDisabledContext.Provider>
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
