/*
 * Select — popover-anchored dropdown from a fixed option list.
 *
 * Wraps Base UI's headless `select` primitive. The trigger reads as an
 * Input (same height / padding / focus-ring rhythm); the popup reads as
 * a Dialog (same `--zs-shadow-dialog`, same opaque-surface invariant).
 *
 *   <Select value={v} onValueChange={setV}>
 *     <Select.Item value="mango">Mango</Select.Item>
 *     <Select.Group label="Citrus">
 *       <Select.Item value="orange">Orange</Select.Item>
 *       <Select.Item value="lemon">Lemon</Select.Item>
 *     </Select.Group>
 *   </Select>
 *
 * Design principles encoded:
 *
 *   1. Select is the "long option list" surface — when ≤ 5 mutually
 *      exclusive choices fit on screen, Toggle.Group reads better; when
 *      freeform-required, Combobox does. Pure principle, no source check.
 *
 *   2. Trigger looks like an Input. Same `--zs-control-h-{sm,md,lg}`,
 *      same outline-vs-default border treatment, same Field cascade.
 *
 *   3. Popup uses Dialog's surface tokens. Same `--zs-shadow-dialog`,
 *      same opaque Crystal base. Reads as a member of the popover family
 *      (Dialog / AlertDialog / Select / Combobox / Autocomplete).
 *
 *   4. Multi-mode is a discriminated union, mirroring Toggle.Group's
 *      Slice-5 fix. `multiple={true}` flips both the value (T → T[]) and
 *      the onValueChange signature in one type-checked motion.
 *
 *   5. No `asChild` on items. Items are leaf option nodes; there is no
 *      compelling render-as case in the brief.
 *
 *   6. We mount Portal + Positioner + Popup + List + ScrollUp/Down
 *      arrows internally so consumers write the simple shape above. To
 *      reach internal layout knobs (different placement, sideOffset, …)
 *      use the `align` / `placement` / `sideOffset` props — these
 *      project onto Base UI's Positioner.
 *
 *   7. Required cascades from the enclosing Field. Base UI exposes
 *      `required` on `Select.Root` itself; we forward our prop OR the
 *      Field context value so `<Field required><Select … /></Field>`
 *      drives the hidden-input `required` attribute correctly.
 *
 *   8. Form submission: Base UI emits a hidden input keyed by `name`,
 *      with `required` mirrored from the prop. No work for us beyond
 *      passing the props through.
 */
import {
  createContext,
  forwardRef,
  useContext,
  useMemo,
  type ButtonHTMLAttributes,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Select as BaseSelect } from "@base-ui/react/select";
import { useFieldContext, type FieldSize } from "../Field";
import { classnames, composeBaseClass } from "../_classnames";

export type SelectSize = "sm" | "md" | "lg";
export type SelectVariant = "default" | "outline";
export type SelectAlign = "start" | "center" | "end";
export type SelectPlacement = "top" | "bottom";

/* ─── shared context — size/variant cascade from Root → Trigger/Item ── */

interface SelectContextValue {
  size: SelectSize;
  variant: SelectVariant;
  multiple: boolean;
}

const SelectContext = createContext<SelectContextValue | null>(null);

function useSelectContext(): SelectContextValue {
  return (
    useContext(SelectContext) ?? { size: "md", variant: "default", multiple: false }
  );
}

/* ─── props ────────────────────────────────────────────────────────── *
 *
 * Base UI's `Select.Root` is generic over `Value, Multiple extends boolean`.
 * The discriminated union below mirrors Toggle.Group's Slice-5 fix: each
 * branch redeclares `value` / `defaultValue` / `onValueChange` against
 * the scalar (T) or array (T[]) shape. TypeScript narrows to the right
 * branch based on the `multiple` literal, so a stock `<Select>` reads
 * scalar and a `<Select multiple>` reads array — no `as` casts at the
 * call site.
 */

type BaseRootShapeProps<Value, Multiple extends boolean | undefined> = Omit<
  Parameters<typeof BaseSelect.Root<Value, Multiple>>[0],
  | "value"
  | "defaultValue"
  | "onValueChange"
  | "multiple"
  | "render"
>;

interface SelectBaseProps<Value> extends BaseRootShapeProps<Value, false> {
  /** Size — sm 32 / md 40 (default) / lg 48 — matches Input. */
  size?: SelectSize;
  /** Visual variant — `default` filled / `outline` border-only. Mirrors Input. */
  variant?: SelectVariant;
  /** Placeholder shown when no value is selected. */
  placeholder?: string;
  /** Align the popup to the trigger's `start` / `center` / `end`. */
  align?: SelectAlign;
  /**
   * Vertical placement of the popup. `bottom` (default) lets Base UI
   * auto-flip if there isn't room; pass `top` to anchor above the
   * trigger.
   */
  placement?: SelectPlacement;
  /** Pixel offset between trigger and popup. */
  sideOffset?: number;
  /** Optional class hook on the Trigger element. */
  className?: string;
  children?: ReactNode;
}

/** Single-selection branch — default when `multiple` is unset or false.
 *
 * Note: `onValueChange` receives `Value | null` because Base UI emits
 * `null` on clear-paths and when the currently-selected item disappears
 * from the options array. Wrapping this as `Value` would be unsound.
 */
export interface SelectSingleProps<Value> extends SelectBaseProps<Value> {
  multiple?: false;
  value?: Value | null;
  defaultValue?: Value | null;
  onValueChange?: (value: Value | null, eventDetails: unknown) => void;
}

/** Multiple-selection branch — value is `Value[]`. */
export interface SelectMultipleProps<Value> extends SelectBaseProps<Value> {
  multiple: true;
  value?: readonly Value[] | null;
  defaultValue?: readonly Value[] | null;
  onValueChange?: (value: Value[], eventDetails: unknown) => void;
}

/*
 * `SelectProps` is the union (single | multiple). Useful as a typed bag,
 * but call-site inference must go through the overload signatures below —
 * the union alone doesn't help TypeScript narrow `Value` from the
 * `value` prop's shape (it locks Value before reading `multiple`, which
 * is why `useState<string[]>` + `value={value}` was inferring
 * `Value=string[]` instead of `Value=string`).
 */
export type SelectProps<Value = string> =
  | SelectSingleProps<Value>
  | SelectMultipleProps<Value>;

/* ─── Root + Trigger composition ──────────────────────────────────────
 *
 * One <Select> render encapsulates the full Trigger / Portal / Positioner
 * / Popup / List composition. The children prop is the list of <Select.Item>
 * / <Select.Group> elements that paint the dropdown content. This is the
 * Dialog-style "all-in-one" Root pattern (vs decomposed Trigger/Portal/etc.)
 * — every story in the brief uses the same shape, so the simple surface
 * pays its rent over and over.
 */
function SelectRoot<Value = string>(props: SelectProps<Value>) {
  const {
    size: sizeProp,
    variant = "default",
    placeholder,
    align = "start",
    placement = "bottom",
    sideOffset = 6,
    className,
    children,
    multiple,
    value,
    defaultValue,
    onValueChange,
    disabled: disabledProp,
    required: requiredProp,
    /*
     * Default the wrapper to popover-mode (`modal: false`). Base UI's
     * `Select.Root` defaults `modal: true`, which mounts an internal
     * backdrop AND scroll-locks the page. The Slice 6 brief specifies
     * popover-feel for Select (NO Backdrop, NO scroll lock), so the
     * wrapper flips the default. Consumers can override by passing
     * `modal={true}` explicitly; preserved here via `modalProp`.
     */
    modal: modalProp,
    /*
     * Sift native HTML-attribute / story-test props off of `...rootRest`
     * so they land on the visible Trigger rather than on the headless
     * Root (which doesn't render any DOM). Without this split, attrs
     * like `data-testid` evaporate at runtime — Base UI's SelectRoot
     * is a context-only node that does NOT spread props onto a host
     * element. Same shape Dialog/AlertDialog use for forwarding consumer
     * attrs onto their Trigger; symmetric here.
     */
    "aria-label": ariaLabel,
    "aria-labelledby": ariaLabelledBy,
    "aria-describedby": ariaDescribedBy,
    "data-testid": dataTestid,
    ...rootRest
  } = props as SelectBaseProps<Value> & {
    multiple?: boolean;
    value?: Value | readonly Value[] | null;
    defaultValue?: Value | readonly Value[] | null;
    onValueChange?: (next: Value | Value[] | null, details: unknown) => void;
    disabled?: boolean;
    required?: boolean;
    modal?: boolean;
    "aria-label"?: string;
    "aria-labelledby"?: string;
    "aria-describedby"?: string;
    "data-testid"?: string;
  };

  const fieldCtx = useFieldContext();
  const fieldSize: FieldSize | undefined = fieldCtx?.size;
  const size: SelectSize = sizeProp ?? (fieldSize as SelectSize | undefined) ?? "md";
  const disabled = disabledProp ?? fieldCtx?.disabled ?? false;
  const required = requiredProp ?? fieldCtx?.required ?? false;

  const ctxValue = useMemo<SelectContextValue>(
    () => ({ size, variant, multiple: multiple === true }),
    [size, variant, multiple],
  );

  // Base UI's Root is strongly typed across the Multiple generic.
  // The overload signatures above keep the public surface honest; the
  // cast below funnels into the runtime call site once we've narrowed.
  const rootProps = {
    ...rootRest,
    multiple: multiple as never,
    value: value as never,
    defaultValue: defaultValue as never,
    onValueChange: onValueChange as never,
    disabled: disabled || undefined,
    required: required || undefined,
    // Popover-feel: default modal=false; consumer explicit override wins.
    modal: modalProp ?? false,
  };

  /*
   * Wave-8 review-fix 🔴 #1: build the aria-* prop bag CONDITIONALLY.
   *
   * Pre-fix the wrapper unconditionally stamped
   *
   *     aria-label={…}
   *     aria-labelledby={ariaLabelledBy}
   *     aria-describedby={ariaDescribedBy}
   *
   * on `<BaseSelect.Trigger>`. When the consumer didn't pass any of
   * those (e.g. inside `<Field><Field.Label>…</Field.Label>
   * <Field.Description>…</Field.Description></Field>`), Base UI's
   * `mergeProps` treated the explicit `undefined`s as overrides and
   * clobbered the ids the Field bridge had auto-wired through
   * `resolveAriaLabelledBy` (`aria-labelledby`) and
   * `validation.getValidationProps` (`aria-describedby`). The focused
   * trigger then had no label / no description.
   *
   * Post-fix we only spread keys that are actually defined, so Base
   * UI's Field wiring shines through. When the consumer DOES pass an
   * explicit `aria-describedby`, we union it with Base UI's auto-wired
   * one via the trigger `render` callback so external help-text ids
   * don't replace Field.Description / Field.Error ids.
   *
   * Accessible-name fallback (axe `button-name`): when there is no
   * explicit `aria-label`, no explicit `aria-labelledby`, AND no Field
   * wrapping us, fall back to `aria-label={placeholder}` so an empty
   * standalone Select still announces something. Inside a Field, skip
   * the fallback entirely — the Field's `aria-labelledby` already
   * names the trigger and a stale duplicate `aria-label` that drifts
   * when the placeholder changes is dead weight (same shape Combobox
   * uses).
   */
  const ariaLabelFallback: string | undefined =
    ariaLabel ??
    (fieldCtx || ariaLabelledBy ? undefined : placeholder);
  const triggerAriaProps: {
    "aria-label"?: string;
    "aria-labelledby"?: string;
  } = {};
  if (ariaLabelFallback !== undefined)
    triggerAriaProps["aria-label"] = ariaLabelFallback;
  if (ariaLabelledBy !== undefined)
    triggerAriaProps["aria-labelledby"] = ariaLabelledBy;

  return (
    <SelectContext.Provider value={ctxValue}>
      <BaseSelect.Root {...rootProps}>
        <BaseSelect.Trigger
          {...triggerAriaProps}
          data-testid={dataTestid}
          className={classnames(
            "zs-select-trigger",
            `zs-select-trigger--${variant}`,
            `zs-select-trigger--${size}`,
            className,
          )}
          data-variant={variant}
          data-size={size}
          /*
           * Union caller's `aria-describedby` with Base UI's auto-wired
           * value. Base UI's `SelectTrigger` runs `mergeProps(triggerProps,
           * {…}, validation.getValidationProps, elementProps, getButtonProps)`
           * — `validation.getValidationProps` is what stamps the Field
           * description / error ids onto the button. If we pass
           * `aria-describedby` through `elementProps`, mergeProps clobbers
           * the Field-auto-wired value. Routing through `render` lets us
           * read Base UI's resolved `aria-describedby` AFTER the validation
           * merge and union it with the consumer's id(s).
           */
          render={(triggerProps) => {
            const baseDescribedBy =
              (triggerProps as { "aria-describedby"?: string })[
                "aria-describedby"
              ];
            const mergedDescribedBy =
              [baseDescribedBy, ariaDescribedBy].filter(Boolean).join(" ") ||
              undefined;
            return (
              <button
                type="button"
                {...(triggerProps as ButtonHTMLAttributes<HTMLButtonElement>)}
                aria-describedby={mergedDescribedBy}
              />
            );
          }}
        >
          <BaseSelect.Value
            className="zs-select-trigger__value"
            placeholder={placeholder}
          />
          <BaseSelect.Icon
            className="zs-select-trigger__icon"
            aria-hidden="true"
          >
            <ChevronDownGlyph />
          </BaseSelect.Icon>
        </BaseSelect.Trigger>
        <BaseSelect.Portal>
          <BaseSelect.Positioner
            className="zs-select-positioner"
            align={align}
            side={placement}
            sideOffset={sideOffset}
            /*
             * Popover-feel: disable Base UI's default
             * `alignItemWithTrigger` (overlap-trigger so the selected
             * item's text aligns with the trigger value). That mode also
             * forces scroll-lock on the body even when `modal=false`
             * (the positioner uses `useAnchoredPopupScrollLock` gated by
             * `alignItemWithTriggerActive || modal`). We want the popup
             * to anchor BELOW the trigger like a Combobox / Dialog, so
             * we turn this off here. Consumers who want the overlap-mode
             * behavior should reach for a different primitive (or
             * Slice 6 will add an opt-in once a real use-case appears).
             */
            alignItemWithTrigger={false}
          >
            <BaseSelect.Popup
              className={classnames(
                "zs-select-popup",
                `zs-select-popup--${size}`,
              )}
              data-size={size}
            >
              <BaseSelect.List className="zs-select-list">
                {children}
              </BaseSelect.List>
            </BaseSelect.Popup>
          </BaseSelect.Positioner>
        </BaseSelect.Portal>
      </BaseSelect.Root>
    </SelectContext.Provider>
  );
}
SelectRoot.displayName = "Select";

function ChevronDownGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M4 6l4 4 4-4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── Item ─────────────────────────────────────────────────────────── */

type BaseItemProps = ComponentPropsWithoutRef<typeof BaseSelect.Item>;
export interface SelectItemProps extends BaseItemProps {
  /** Stable testid hook for stories / aria-wiring. */
  "data-testid"?: string;
}

/* Render: the chip carries our `zs-select-item` class; an indicator
 * pseudo-row reserves space for the checkmark whether or not the item
 * is selected, so single-select rows align with multi-select rows. */
const SelectItem = forwardRef<HTMLElement, SelectItemProps>(function SelectItem(
  { className, children, ...rest },
  ref,
) {
  const { size } = useSelectContext();
  return (
    <BaseSelect.Item
      ref={ref as Ref<HTMLDivElement>}
      className={composeBaseClass("zs-select-item", className) as string}
      data-size={size}
      {...rest}
    >
      <BaseSelect.ItemIndicator
        className="zs-select-item__indicator"
        keepMounted
      >
        <CheckGlyph />
      </BaseSelect.ItemIndicator>
      <BaseSelect.ItemText className="zs-select-item__text">
        {children}
      </BaseSelect.ItemText>
    </BaseSelect.Item>
  );
});
SelectItem.displayName = "Select.Item";

function CheckGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M3.5 8.5l3 3 6-6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── Group + GroupLabel ─────────────────────────────────────────── *
 *
 * Two shapes:
 *   - `<Select.Group label="…">…</Select.Group>` (sugar) renders the
 *     auto-bound GroupLabel above the items.
 *   - `<Select.Group><Select.GroupLabel>…</Select.GroupLabel>…</Select.Group>`
 *     decomposed — same composeable shape Base UI ships.
 */

type BaseGroupProps = ComponentPropsWithoutRef<typeof BaseSelect.Group>;
export interface SelectGroupProps extends BaseGroupProps {
  /**
   * Shorthand: when set, renders an auto `<Select.GroupLabel>` as the
   * first child. Use the decomposed form when the label needs custom
   * markup (icons, badges).
   */
  label?: ReactNode;
}

const SelectGroup = forwardRef<HTMLDivElement, SelectGroupProps>(
  function SelectGroup({ label, className, children, ...rest }, ref) {
    return (
      <BaseSelect.Group
        ref={ref}
        className={composeBaseClass("zs-select-group", className)}
        {...rest}
      >
        {label != null ? <SelectGroupLabel>{label}</SelectGroupLabel> : null}
        {children}
      </BaseSelect.Group>
    );
  },
);
SelectGroup.displayName = "Select.Group";

type BaseGroupLabelProps = ComponentPropsWithoutRef<typeof BaseSelect.GroupLabel>;
export type SelectGroupLabelProps = BaseGroupLabelProps;

const SelectGroupLabel = forwardRef<HTMLDivElement, SelectGroupLabelProps>(
  function SelectGroupLabel({ className, ...rest }, ref) {
    return (
      <BaseSelect.GroupLabel
        ref={ref}
        className={composeBaseClass("zs-select-group-label", className)}
        {...rest}
      />
    );
  },
);
SelectGroupLabel.displayName = "Select.GroupLabel";

/* ─── Separator ────────────────────────────────────────────────────── */

type BaseSeparatorProps = ComponentPropsWithoutRef<typeof BaseSelect.Separator>;
export type SelectSeparatorProps = BaseSeparatorProps;

const SelectSeparator = forwardRef<HTMLDivElement, SelectSeparatorProps>(
  function SelectSeparator({ className, ...rest }, ref) {
    return (
      <BaseSelect.Separator
        ref={ref}
        className={composeBaseClass("zs-select-separator", className)}
        {...rest}
      />
    );
  },
);
SelectSeparator.displayName = "Select.Separator";

/* ─── namespace export ─────────────────────────────────────────────── *
 *
 * Use call-signature OVERLOADS, not a single union signature, so
 * inference picks the correct branch from the `value` prop's shape +
 * `multiple` discriminant. With a single union signature TypeScript
 * locks `Value` from the first prop it sees (typically `value`) before
 * reading `multiple`, which causes the multi-mode story shape
 * `useState<string[]>` + `<Select multiple value={value}>` to infer
 * `Value=string[]` instead of `Value=string`. Overloads let TS try the
 * Single branch first, fail when `multiple: true` is present, then try
 * the Multiple branch with Value-from-array narrowing intact. */
export interface SelectComponent {
  <Value = string>(props: SelectSingleProps<Value>): React.JSX.Element;
  <Value = string>(props: SelectMultipleProps<Value>): React.JSX.Element;
  Item: typeof SelectItem;
  Group: typeof SelectGroup;
  GroupLabel: typeof SelectGroupLabel;
  Separator: typeof SelectSeparator;
  displayName?: string;
}

const ForwardedSelect = SelectRoot as unknown as SelectComponent;
ForwardedSelect.Item = SelectItem;
ForwardedSelect.Group = SelectGroup;
ForwardedSelect.GroupLabel = SelectGroupLabel;
ForwardedSelect.Separator = SelectSeparator;
ForwardedSelect.displayName = "Select";

export const Select = ForwardedSelect;
