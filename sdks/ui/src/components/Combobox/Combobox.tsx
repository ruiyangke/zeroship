/*
 * Combobox — typeahead + selection hybrid.
 *
 * Wraps Base UI's headless `combobox` primitive. The text input IS the
 * trigger; type to filter the list, ↓/↑ navigate, Enter commits. Multi-
 * mode adds chips inside the input row.
 *
 *   <Combobox items={fruits}>
 *     {fruits.map((f) => (
 *       <Combobox.Item key={f.value} value={f.value}>
 *         {f.label}
 *       </Combobox.Item>
 *     ))}
 *   </Combobox>
 *
 * Design principles encoded:
 *
 *   1. Combobox is "text input + filtered list + commit". The Input IS
 *      the Trigger. Don't ship an asChild on items — they're leaf nodes.
 *
 *   2. Default filter is Base UI's substring-includes — fast, predictable,
 *      "no surprises". Consumers can swap in `filter={fn}` to customize
 *      (Base UI exposes the prop directly via `...rest`).
 *
 *   3. Multi-mode renders chips INSIDE the input row via Base UI's
 *      Chips/Chip/ChipRemove primitives.
 *
 *   4. Same popup surface as Select / Dialog. `--zs-shadow-dialog`,
 *      opaque Crystal base, popup cap at min(50vh, 28rem).
 *
 *   5. Required cascade from Field — same shape Select uses. The
 *      Combobox emits a hidden submission input itself; we just need to
 *      forward `required`.
 *
 *   6. No Backdrop. Combobox is a popover, not a modal.
 */
import {
  createContext,
  forwardRef,
  useContext,
  useMemo,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Combobox as BaseCombobox } from "@base-ui/react/combobox";
import { useFieldContext, type FieldSize } from "../Field";
import { classnames, composeBaseClass } from "../_classnames";

export type ComboboxSize = "sm" | "md" | "lg";
export type ComboboxVariant = "default" | "outline";
export type ComboboxAlign = "start" | "center" | "end";
export type ComboboxPlacement = "top" | "bottom";

/* ─── shared context ───────────────────────────────────────────────── */

interface ComboboxContextValue {
  size: ComboboxSize;
  variant: ComboboxVariant;
  multiple: boolean;
}

const ComboboxContext = createContext<ComboboxContextValue | null>(null);

function useComboboxContext(): ComboboxContextValue {
  return (
    useContext(ComboboxContext) ?? {
      size: "md",
      variant: "default",
      multiple: false,
    }
  );
}

/* ─── props ────────────────────────────────────────────────────────── */

type BaseComboboxRootShape<Value, Multiple extends boolean | undefined> = Omit<
  Parameters<typeof BaseCombobox.Root<Value, Multiple>>[0],
  | "value"
  | "defaultValue"
  | "onValueChange"
  | "multiple"
  | "render"
  | "children"
>;

interface ComboboxBaseProps<Value>
  extends BaseComboboxRootShape<Value, false> {
  /** Size — sm 32 / md 40 / lg 48. */
  size?: ComboboxSize;
  /** Visual variant. */
  variant?: ComboboxVariant;
  /** Placeholder for the empty input. */
  placeholder?: string;
  /** Popup alignment to the trigger. */
  align?: ComboboxAlign;
  /** Popup vertical placement. */
  placement?: ComboboxPlacement;
  /** Pixel offset between trigger and popup. */
  sideOffset?: number;
  /** Class hook on the input row. */
  className?: string;
  /**
   * Children — either a static set of <Combobox.Item> nodes (no
   * filtering) OR a render-prop `(item) => ReactNode` that Base UI
   * threads through its substring-includes filter against the `items`
   * prop on Root. The render-prop shape is what makes the brief's
   * "typing filters the list" assertion pass — without it Base UI
   * keeps every static child mounted.
   */
  children?: ReactNode | ((item: Value, index: number) => ReactNode);
  /**
   * Optional empty-state — rendered as a sibling of the list inside
   * the popup, so it can announce regardless of whether children is a
   * static block or a render-prop. Same shape as `<Combobox.Empty>`
   * (Base UI auto-mounts persistently for live-region announcements).
   */
  empty?: ReactNode;
  /**
   * Multi-mode chip label resolver. Escape hatch for callers whose
   * `Value` is a non-string shape (e.g. `{id, name}`); the default
   * walks the `items` array to find a `label` for a matching value, or
   * falls back to `String(v)` when no match is found. Provide
   * `getChipLabel` when you need typed access to the value (e.g.
   * `(v) => v.name`).
   */
  getChipLabel?: (value: Value) => ReactNode;
}

export interface ComboboxSingleProps<Value> extends ComboboxBaseProps<Value> {
  multiple?: false;
  value?: Value | null;
  defaultValue?: Value | null;
  onValueChange?: (value: Value | null, eventDetails: unknown) => void;
}

export interface ComboboxMultipleProps<Value> extends ComboboxBaseProps<Value> {
  multiple: true;
  value?: readonly Value[] | null;
  defaultValue?: readonly Value[] | null;
  onValueChange?: (value: Value[], eventDetails: unknown) => void;
}

export type ComboboxProps<Value = string> =
  | ComboboxSingleProps<Value>
  | ComboboxMultipleProps<Value>;

/* ─── Root ────────────────────────────────────────────────────────── *
 *
 * Single render encapsulates Root → InputGroup → Chips? → Input →
 * Portal → Positioner → Popup → (Empty | List). Children are dropped
 * into the List as the option set. */
function ComboboxRoot<Value = string>(props: ComboboxProps<Value>) {
  const {
    size: sizeProp,
    variant = "default",
    placeholder,
    align = "start",
    placement = "bottom",
    sideOffset = 6,
    className,
    children,
    empty,
    multiple,
    value,
    defaultValue,
    onValueChange,
    disabled: disabledProp,
    required: requiredProp,
    getChipLabel,
    /*
     * Sift `data-testid` off the Root rest so it lands on the
     * InputGroup (the visible DOM host) — Combobox.Root is a context-only
     * node and won't forward to a real element.
     *
     * Sift `aria-label` / `aria-labelledby` / `aria-describedby` so they
     * land on the focusable `<input>` (NOT the InputGroup). The
     * InputGroup is a `<div role="group">`; aria-label on a group is
     * legal but doesn't accessibly-name the focusable control. Stamping
     * aria-* on the Input is what screen readers announce on focus.
     */
    "data-testid": dataTestid,
    "aria-label": ariaLabel,
    "aria-labelledby": ariaLabelledBy,
    "aria-describedby": ariaDescribedBy,
    ...rootRest
  } = props as ComboboxBaseProps<Value> & {
    multiple?: boolean;
    value?: Value | readonly Value[] | null;
    defaultValue?: Value | readonly Value[] | null;
    onValueChange?: (next: Value | Value[] | null, details: unknown) => void;
    disabled?: boolean;
    required?: boolean;
    getChipLabel?: (v: Value) => ReactNode;
    "data-testid"?: string;
    "aria-label"?: string;
    "aria-labelledby"?: string;
    "aria-describedby"?: string;
  };

  /*
   * Default chip-label resolver. When the consumer didn't pass
   * `getChipLabel`, we walk the `items` array (Base UI's `items` prop is
   * Root-level, lives on `rootRest`) looking for a matching value. Each
   * candidate may be `Value` directly OR a `{value, label}` pair (Base
   * UI's grouped/labelled shape). Fallback: `String(v)`, same as
   * Slice-6's first-pass behavior — kept as a last resort, never as the
   * default for typed value shapes.
   */
  const items = (rootRest as { items?: readonly unknown[] }).items;
  const resolveChipLabel = (v: Value): ReactNode => {
    if (getChipLabel) return getChipLabel(v);
    if (Array.isArray(items)) {
      for (const candidate of items) {
        if (candidate === v) return String(v);
        if (
          candidate != null &&
          typeof candidate === "object" &&
          "value" in candidate &&
          (candidate as { value: unknown }).value === v
        ) {
          const labelled = candidate as { value: unknown; label?: ReactNode };
          if (labelled.label != null) return labelled.label;
          return String(v);
        }
      }
    }
    return String(v);
  };

  const fieldCtx = useFieldContext();
  const fieldSize: FieldSize | undefined = fieldCtx?.size;
  const size: ComboboxSize =
    sizeProp ?? (fieldSize as ComboboxSize | undefined) ?? "md";
  const disabled = disabledProp ?? fieldCtx?.disabled ?? false;
  const required = requiredProp ?? fieldCtx?.required ?? false;
  const isMultiple = multiple === true;

  const ctxValue = useMemo<ComboboxContextValue>(
    () => ({ size, variant, multiple: isMultiple }),
    [size, variant, isMultiple],
  );

  const rootProps = {
    ...rootRest,
    multiple: multiple as never,
    value: value as never,
    defaultValue: defaultValue as never,
    onValueChange: onValueChange as never,
    disabled: disabled || undefined,
    required: required || undefined,
  };

  return (
    <ComboboxContext.Provider value={ctxValue}>
      <BaseCombobox.Root {...rootProps}>
        <BaseCombobox.InputGroup
          data-testid={dataTestid}
          className={classnames(
            "zs-combobox-input-group",
            `zs-combobox-input-group--${variant}`,
            `zs-combobox-input-group--${size}`,
            className,
          )}
          data-variant={variant}
          data-size={size}
        >
          {isMultiple ? (
            <BaseCombobox.Chips className="zs-combobox-chips">
              <BaseCombobox.Value>
                {(values: unknown) => (
                  <>
                    {Array.isArray(values)
                      ? values.map((v, i) => (
                          <ComboboxChip key={String(v) + i}>
                            {resolveChipLabel(v as Value)}
                          </ComboboxChip>
                        ))
                      : null}
                    <BaseCombobox.Input
                      className="zs-combobox-input"
                      placeholder={placeholder}
                      aria-label={ariaLabel}
                      aria-labelledby={ariaLabelledBy}
                      aria-describedby={ariaDescribedBy}
                    />
                  </>
                )}
              </BaseCombobox.Value>
            </BaseCombobox.Chips>
          ) : (
            <BaseCombobox.Input
              className="zs-combobox-input"
              placeholder={placeholder}
              aria-label={ariaLabel}
              aria-labelledby={ariaLabelledBy}
              aria-describedby={ariaDescribedBy}
            />
          )}
          <BaseCombobox.Icon
            className="zs-combobox-input-group__icon"
            aria-hidden="true"
          >
            <ChevronDownGlyph />
          </BaseCombobox.Icon>
        </BaseCombobox.InputGroup>
        <BaseCombobox.Portal>
          <BaseCombobox.Positioner
            className="zs-combobox-positioner"
            align={align}
            side={placement}
            sideOffset={sideOffset}
          >
            <BaseCombobox.Popup
              className={classnames(
                "zs-combobox-popup",
                `zs-combobox-popup--${size}`,
              )}
              data-size={size}
            >
              <BaseCombobox.List className="zs-combobox-list">
                {children as ReactNode}
              </BaseCombobox.List>
              {empty}
            </BaseCombobox.Popup>
          </BaseCombobox.Positioner>
        </BaseCombobox.Portal>
      </BaseCombobox.Root>
    </ComboboxContext.Provider>
  );
}
ComboboxRoot.displayName = "Combobox";

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

function ChipRemoveGlyph() {
  return (
    <svg viewBox="0 0 12 12" aria-hidden="true" focusable="false">
      <path
        d="M3 3l6 6M9 3l-6 6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
    </svg>
  );
}

/* ─── Item ─────────────────────────────────────────────────────────── */

type BaseItemProps = ComponentPropsWithoutRef<typeof BaseCombobox.Item>;
export interface ComboboxItemProps extends BaseItemProps {
  "data-testid"?: string;
}

const ComboboxItem = forwardRef<HTMLDivElement, ComboboxItemProps>(
  function ComboboxItem({ className, children, ...rest }, ref) {
    const { size } = useComboboxContext();
    return (
      <BaseCombobox.Item
        ref={ref}
        className={composeBaseClass("zs-combobox-item", className)}
        data-size={size}
        {...rest}
      >
        <BaseCombobox.ItemIndicator
          className="zs-combobox-item__indicator"
          keepMounted
        >
          <CheckGlyph />
        </BaseCombobox.ItemIndicator>
        <span className="zs-combobox-item__text">{children}</span>
      </BaseCombobox.Item>
    );
  },
);
ComboboxItem.displayName = "Combobox.Item";

/* ─── Empty ───────────────────────────────────────────────────────── */

type BaseEmptyProps = ComponentPropsWithoutRef<typeof BaseCombobox.Empty>;
export type ComboboxEmptyProps = BaseEmptyProps;

const ComboboxEmpty = forwardRef<HTMLDivElement, ComboboxEmptyProps>(
  function ComboboxEmpty({ className, ...rest }, ref) {
    return (
      <BaseCombobox.Empty
        ref={ref}
        className={composeBaseClass("zs-combobox-empty", className)}
        {...rest}
      />
    );
  },
);
ComboboxEmpty.displayName = "Combobox.Empty";

/* ─── Chip + ChipRemove ──────────────────────────────────────────── *
 *
 * The Chip renders a single removable token inside multi-mode triggers.
 * We pair Base UI's Chip + ChipRemove so the chip carries its own
 * remove button (X) with the canonical aria-label.
 *
 * NOTE: removal is keyed by the chip's index in the composite-list —
 * NOT by a `value` prop. Base UI's `<Combobox.Chip>` accepts only the
 * native `<div>` props plus internal state; passing `value` leaks onto
 * the DOM as a stray `value="…"` attribute, which is invalid HTML on a
 * div and reads as a contract drift. The chip's parent must render
 * chips in the same order Base UI emits values from `<Combobox.Value>`.
 */

type BaseChipProps = ComponentPropsWithoutRef<typeof BaseCombobox.Chip>;
export interface ComboboxChipProps extends BaseChipProps {
  /**
   * Aria-label for the remove button. Defaults to `"Remove"`. Localized
   * consumers override per-instance.
   */
  removeLabel?: string;
}

const ComboboxChip = forwardRef<HTMLDivElement, ComboboxChipProps>(
  function ComboboxChip(
    { className, children, removeLabel = "Remove", ...rest },
    ref,
  ) {
    return (
      <BaseCombobox.Chip
        ref={ref}
        className={composeBaseClass("zs-combobox-chip", className)}
        {...rest}
      >
        <span className="zs-combobox-chip__label">{children}</span>
        <BaseCombobox.ChipRemove
          className="zs-combobox-chip__remove"
          aria-label={removeLabel}
        >
          <ChipRemoveGlyph />
        </BaseCombobox.ChipRemove>
      </BaseCombobox.Chip>
    );
  },
);
ComboboxChip.displayName = "Combobox.Chip";

/* ─── namespace export ─────────────────────────────────────────────── */

type ComboboxComponent = (<Value = string>(
  props: ComboboxProps<Value>,
) => React.JSX.Element) & {
  Item: typeof ComboboxItem;
  Empty: typeof ComboboxEmpty;
  Chip: typeof ComboboxChip;
  displayName?: string;
};

const ForwardedCombobox = ComboboxRoot as unknown as ComboboxComponent;
ForwardedCombobox.Item = ComboboxItem;
ForwardedCombobox.Empty = ComboboxEmpty;
ForwardedCombobox.Chip = ComboboxChip;
ForwardedCombobox.displayName = "Combobox";

export const Combobox = ForwardedCombobox;
