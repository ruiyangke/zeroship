/*
 * Autocomplete — text input with suggestion list.
 *
 * Wraps Base UI's headless `autocomplete` primitive. Pure freeform
 * completion: suggestions help, but the value the user types IS the
 * value. No managed selection list. Useful for emails, URLs, search
 * boxes — anywhere the suggestion is a hint, not a commit.
 *
 *   <Autocomplete items={domains} placeholder="email@example.com">
 *     {domains.map((d) => (
 *       <Autocomplete.Item key={d} value={d}>{d}</Autocomplete.Item>
 *     ))}
 *   </Autocomplete>
 *
 * Design principles encoded:
 *
 *   1. Single-only. Autocomplete is freeform completion, not selection;
 *      multi-mode doesn't apply (use Combobox for that).
 *
 *   2. `mode="list"` is the default — items filter to the input; the
 *      input value does NOT change on highlight. Consumers can opt into
 *      `mode="both"` or `"inline"` for inline-completion behavior via
 *      the standard Base UI prop (passed through via `...rest`).
 *
 *   3. The shared popup CSS is the Combobox popup CSS — both surfaces
 *      look the same and feel like a member of the popover family.
 *      Autocomplete.css carries only Autocomplete-specific bits; the
 *      shared rules live in Combobox.css already.
 *
 *   4. Required cascade from Field — same shape Select / Combobox use.
 */
import {
  createContext,
  forwardRef,
  useContext,
  useMemo,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import {
  Autocomplete as BaseAutocomplete,
  type AutocompleteRootProps as BaseAutocompleteRootProps,
} from "@base-ui/react/autocomplete";
import { useFieldContext, type FieldSize } from "../Field";
import { classnames, composeBaseClass } from "../_classnames";

export type AutocompleteSize = "sm" | "md" | "lg";
export type AutocompleteVariant = "default" | "outline";
export type AutocompleteAlign = "start" | "center" | "end";
export type AutocompletePlacement = "top" | "bottom";

interface AutocompleteContextValue {
  size: AutocompleteSize;
  variant: AutocompleteVariant;
}
const AutocompleteContext = createContext<AutocompleteContextValue | null>(null);
function useAutocompleteContext(): AutocompleteContextValue {
  return useContext(AutocompleteContext) ?? { size: "md", variant: "default" };
}

/* ─── props ────────────────────────────────────────────────────────── */

type BaseAutocompleteRootShape<Value> = Omit<
  BaseAutocompleteRootProps<Value>,
  "render" | "children"
>;

export interface AutocompleteProps<Value extends string = string>
  extends BaseAutocompleteRootShape<Value> {
  size?: AutocompleteSize;
  variant?: AutocompleteVariant;
  placeholder?: string;
  align?: AutocompleteAlign;
  placement?: AutocompletePlacement;
  sideOffset?: number;
  className?: string;
  /**
   * Render-prop children: `(item) => ReactNode` threads suggestions
   * through Base UI's substring-includes filter against `items`. A
   * static ReactNode is also accepted (no filtering).
   */
  children?: ReactNode | ((item: Value, index: number) => ReactNode);
}

/* ─── Root ────────────────────────────────────────────────────────── */

function AutocompleteRoot<Value extends string = string>(
  props: AutocompleteProps<Value>,
) {
  const {
    size: sizeProp,
    variant = "default",
    placeholder,
    align = "start",
    placement = "bottom",
    sideOffset = 6,
    className,
    children,
    disabled: disabledProp,
    required: requiredProp,
    /*
     * Sift native HTML attrs / test hooks off Root rest so they land on
     * the InputGroup. Same rationale as Select/Combobox — Autocomplete.Root
     * is a context-only node.
     */
    "data-testid": dataTestid,
    "aria-label": ariaLabel,
    "aria-labelledby": ariaLabelledBy,
    "aria-describedby": ariaDescribedBy,
    ...rootRest
  } = props as AutocompleteProps<Value> & {
    disabled?: boolean;
    required?: boolean;
    "data-testid"?: string;
    "aria-label"?: string;
    "aria-labelledby"?: string;
    "aria-describedby"?: string;
  };

  const fieldCtx = useFieldContext();
  const fieldSize: FieldSize | undefined = fieldCtx?.size;
  const size: AutocompleteSize =
    sizeProp ?? (fieldSize as AutocompleteSize | undefined) ?? "md";
  const disabled = disabledProp ?? fieldCtx?.disabled ?? false;
  const required = requiredProp ?? fieldCtx?.required ?? false;

  const ctxValue = useMemo<AutocompleteContextValue>(
    () => ({ size, variant }),
    [size, variant],
  );

  return (
    <AutocompleteContext.Provider value={ctxValue}>
      {/*
       * Base UI's `Autocomplete.Root` is double-overloaded over the
       * `items` prop shape (flat array vs grouped). We cast through
       * `unknown` at the boundary so the type-checker doesn't try to
       * pick one overload — both are valid for our consumer surface
       * (the discriminant is whether items[i] has an `items` sub-array,
       * which Base UI sniffs at runtime). The Autocomplete generic
       * still constrains the public Value type.
       */}
      <BaseAutocomplete.Root
        {...(rootRest as unknown as Record<string, unknown>)}
        disabled={disabled || undefined}
        required={required || undefined}
      >
        <BaseAutocomplete.InputGroup
          data-testid={dataTestid}
          aria-label={ariaLabel}
          aria-labelledby={ariaLabelledBy}
          aria-describedby={ariaDescribedBy}
          className={classnames(
            "zs-combobox-input-group",
            "zs-autocomplete-input-group",
            `zs-combobox-input-group--${variant}`,
            `zs-combobox-input-group--${size}`,
            className,
          )}
          data-variant={variant}
          data-size={size}
        >
          <BaseAutocomplete.Input
            className="zs-combobox-input"
            placeholder={placeholder}
          />
          <BaseAutocomplete.Icon
            className="zs-combobox-input-group__icon"
            aria-hidden="true"
          >
            <ChevronDownGlyph />
          </BaseAutocomplete.Icon>
        </BaseAutocomplete.InputGroup>
        <BaseAutocomplete.Portal>
          <BaseAutocomplete.Positioner
            className="zs-combobox-positioner"
            align={align}
            side={placement}
            sideOffset={sideOffset}
          >
            <BaseAutocomplete.Popup
              className={classnames(
                "zs-combobox-popup",
                "zs-autocomplete-popup",
                `zs-combobox-popup--${size}`,
              )}
              data-size={size}
            >
              <BaseAutocomplete.List className="zs-combobox-list">
                {children}
              </BaseAutocomplete.List>
            </BaseAutocomplete.Popup>
          </BaseAutocomplete.Positioner>
        </BaseAutocomplete.Portal>
      </BaseAutocomplete.Root>
    </AutocompleteContext.Provider>
  );
}
AutocompleteRoot.displayName = "Autocomplete";

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

/* ─── Item ─────────────────────────────────────────────────────────── *
 *
 * Autocomplete items have no "selected" notion — Autocomplete is
 * suggestion-only. We render the highlight surface but skip the
 * checkmark gutter Select/Combobox use. */

type BaseItemProps = ComponentPropsWithoutRef<typeof BaseAutocomplete.Item>;
export interface AutocompleteItemProps extends BaseItemProps {
  "data-testid"?: string;
}

const AutocompleteItem = forwardRef<HTMLDivElement, AutocompleteItemProps>(
  function AutocompleteItem({ className, children, ...rest }, ref) {
    const { size } = useAutocompleteContext();
    return (
      <BaseAutocomplete.Item
        ref={ref}
        className={composeBaseClass(
          "zs-autocomplete-item zs-combobox-item",
          className,
        )}
        data-size={size}
        {...rest}
      >
        <span className="zs-autocomplete-item__text">{children}</span>
      </BaseAutocomplete.Item>
    );
  },
);
AutocompleteItem.displayName = "Autocomplete.Item";

/* ─── namespace export ─────────────────────────────────────────────── */

type AutocompleteComponent = (<Value extends string = string>(
  props: AutocompleteProps<Value>,
) => React.JSX.Element) & {
  Item: typeof AutocompleteItem;
  displayName?: string;
};

const ForwardedAutocomplete =
  AutocompleteRoot as unknown as AutocompleteComponent;
ForwardedAutocomplete.Item = AutocompleteItem;
ForwardedAutocomplete.displayName = "Autocomplete";

export const Autocomplete = ForwardedAutocomplete;
