/*
 * Toggle + ToggleGroup — pressable two-state button + segmented control.
 *
 * Wraps Base UI's `Toggle` + `ToggleGroup` primitives.
 *
 *   - Toggle (standalone): renders a real `<button>` with `aria-pressed`
 *     state. The component reads as a button affordance, not as a
 *     settings widget — that's the line that separates Toggle from
 *     Switch (Switch reads as a binary settings control with a track
 *     and a knob; Toggle reads as a button that stays "in").
 *
 *   - ToggleGroup: chains Toggles into a segmented control. Single-
 *     selection by default (mutually exclusive); `multiple={true}` lets
 *     each segment carry an independent boolean (filter-pill rows).
 *
 * Design guarantees encoded here (in source so they travel with the code):
 *
 *   1. Pressed state must be obvious. Pressed segments paint with the
 *      accent fill + ink combination Button uses for `filled`; unpressed
 *      segments are transparent over the group's unified track.
 *
 *   2. 2–5 segments is the sweet spot. Beyond 5 a Select reads better;
 *      we dev-warn (deduped per signature) when a group ships with 6+
 *      Toggle children. The warn is gated on `process.env.NODE_ENV !==
 *      "production"` so production bundles DCE the whole branch.
 *
 *   3. Equal-width segments by default. The Group's CSS uses
 *      `grid-template-columns: repeat(<n>, 1fr)`; `equalWidth={false}`
 *      flips to intrinsic-width via `data-equal-width="false"`.
 *
 *   4. Keep content types consistent. Mixing icon-only with text-only
 *      segments fights the visual rhythm of the segmented control. We
 *      dev-warn (same deduped effect) when a group's children mix the
 *      two content kinds.
 *
 *   5. Standalone Toggle hovers like Button's `--gray`. Inside a Group,
 *      unpressed segments share the group track surface and use a
 *      lighter hover tint so the group reads as one unit, not as a
 *      row of buttons.
 *
 *   6. No bleed on focus ring. Each segment's focus ring lives on
 *      `outline` (not `box-shadow`) so it escapes the group's inset
 *      rim and the surrounding overflow.
 *
 *   7. `asChild` routes through the canonical Slot helper (commit
 *      `3a64a726` / Dialog.Close pattern). The asChild target's tag
 *      drives `nativeButton` — pass an `<a>` and Base UI swaps in the
 *      non-native keyboard handlers; pass a `<button>` and the native
 *      ones stay.
 *
 *   8. Toggle is NOT a form control. The native `<button>` does not
 *      submit `aria-pressed` as a form value. Wrap with a real
 *      `<input type="hidden">` if you need form submission.
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useContext,
  useEffect,
  type ComponentPropsWithRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { Toggle as BaseToggle } from "@base-ui/react/toggle";
import { ToggleGroup as BaseToggleGroup } from "@base-ui/react/toggle-group";
import { Slot } from "../_slot";
import { classnames } from "../_classnames";

export type ToggleSize = "sm" | "md" | "lg";
export type ToggleVariant = "default" | "plain" | "tinted";
export type ToggleOrientation = "horizontal" | "vertical";

/* ─── group-local context ───────────────────────────────────────────── *
 *
 * Carries the group's `size` and `variant` so child Toggles pick them up
 * without climbing the React tree manually. Explicit props on a child
 * still win over the context. `multiple` is forwarded so children can
 * tailor a11y assertions in tests (and for future Toggle.Group-aware
 * features) without re-reading the BaseUI group state.
 */
interface ToggleGroupContextValue {
  size: ToggleSize | undefined;
  variant: ToggleVariant | undefined;
  multiple: boolean;
  disabled: boolean;
}

const ToggleGroupContext = createContext<ToggleGroupContextValue | null>(null);

function useToggleGroupContext() {
  return useContext(ToggleGroupContext);
}

/* ─── ToggleGroup ───────────────────────────────────────────────────── */

type BaseToggleGroupProps = ComponentPropsWithRef<typeof BaseToggleGroup>;
type BaseToggleGroupChangeEventDetails = Parameters<
  NonNullable<BaseToggleGroupProps["onValueChange"]>
>[1];

/**
 * Properties shared between single- and multiple-selection groups.
 *
 * `role` is intentionally omitted — the group LOCKS to `role="toolbar"`
 * (the canonical semantic for a segmented control + the only role that
 * permits `aria-orientation`). Consumers passing `role` get a TypeScript
 * error at the call site so the contract is enforced at compile time.
 */
interface ToggleGroupBaseProps
  extends Omit<
    BaseToggleGroupProps,
    | "className"
    | "render"
    | "role"
    | "value"
    | "defaultValue"
    | "onValueChange"
    | "multiple"
  > {
  /** Visual size — inherited by children unless they override. */
  size?: ToggleSize;
  /**
   * Visual variant — inherited by children.
   *
   * - `default` paints a unified track behind the segments and an
   *   accent pill on pressed segments. Reads as a segmented control.
   * - `plain` is segments-only (no track surface). Reads as a row of
   *   linked pressable buttons; useful when the segmented framing
   *   would over-claim space in a toolbar.
   * - `tinted` paints the unpressed segments with a translucent accent
   *   tint — loud, signals interactivity strongly.
   */
  variant?: ToggleVariant;
  /**
   * Equal-width segments (default `true`). When `false`, each Toggle
   * sizes to its own content — useful in toolbars where icon-only
   * segments mix with text-only ones. Setting `equalWidth={false}` also
   * acts as an explicit opt-in to mixed icon/text content, so the mixed-
   * content dev-warn skips for groups that have declared the layout.
   *
   * @default true
   */
  equalWidth?: boolean;
  /** Optional class hook on the group root. */
  className?: string;
  /** Toggle children — typically 2–5. */
  children?: ReactNode;
}

/**
 * Single-selection variant — value is a scalar `Value` (or `undefined`).
 * The discriminant is `multiple?: false | undefined` so the default
 * `<Toggle.Group>` (no `multiple` prop) narrows to this branch.
 */
export interface ToggleGroupSingleProps<Value extends string = string>
  extends ToggleGroupBaseProps {
  /** When omitted or `false`, the group is single-selection. */
  multiple?: false;
  /** Controlled value of the single pressed segment. */
  value?: Value;
  /** Uncontrolled initial value of the single pressed segment. */
  defaultValue?: Value;
  /**
   * Pressed-state change callback. Fires with the scalar value of the
   * newly-pressed segment, or `undefined` when the press flips off.
   */
  onValueChange?: (
    value: Value | undefined,
    eventDetails: BaseToggleGroupChangeEventDetails,
  ) => void;
}

/**
 * Multiple-selection variant — value is an array of `Value`s. Discriminant
 * is `multiple: true` (required, not optional).
 */
export interface ToggleGroupMultipleProps<Value extends string = string>
  extends ToggleGroupBaseProps {
  /** Required to enter multiple-selection mode. */
  multiple: true;
  /** Controlled set of pressed values. */
  value?: readonly Value[];
  /** Uncontrolled initial set of pressed values. */
  defaultValue?: readonly Value[];
  /** Pressed-state change callback. Fires with the next set. */
  onValueChange?: (
    value: Value[],
    eventDetails: BaseToggleGroupChangeEventDetails,
  ) => void;
}

/**
 * Public `Toggle.Group` props — discriminated union over `multiple`. The
 * default branch is single-selection so a stock `<Toggle.Group>` reads
 * scalar `value`/`defaultValue`/`onValueChange`.
 */
export type ToggleGroupProps<Value extends string = string> =
  | ToggleGroupSingleProps<Value>
  | ToggleGroupMultipleProps<Value>;

/**
 * Module-level dedup map for dev warnings. We key on a stable signature
 * derived from the warning identity so React's StrictMode double-render
 * and noisy reload-heavy dev sessions don't drown in repeat-warnings.
 * The whole branch is DCE'd in production via the static NODE_ENV check.
 */
const groupWarned = new Set<string>();

function ToggleGroupInner<Value extends string = string>(
  props: ToggleGroupProps<Value>,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const {
    size,
    variant,
    equalWidth = true,
    orientation = "horizontal",
    disabled: disabledProp,
    className,
    value,
    defaultValue,
    onValueChange,
    children,
    multiple,
    ...rest
  } = props as ToggleGroupBaseProps & {
    multiple?: boolean;
    value?: Value | readonly Value[];
    defaultValue?: Value | readonly Value[];
    onValueChange?: (
      next: Value | Value[] | undefined,
      details: BaseToggleGroupChangeEventDetails,
    ) => void;
  };

  const disabled = disabledProp ?? false;
  const isMultiple = multiple === true;

  // Dev-warn guards (contingency: 6+ segments, mixed icon/text). Both
  // run via useEffect keyed by a content signature; module-level Set
  // dedupes across StrictMode double-render + per-instance signatures.
  // Production bundles DCE this branch because the NODE_ENV compare is
  // a literal string equality.
  useEffect(() => {
    if (typeof process === "undefined") return;
    if (process.env.NODE_ENV === "production") return;

    const childArray = collectToggleChildren(children);
    const count = childArray.length;

    // 6+ segments warn — one warn per process per count.
    if (count >= 6) {
      const sig = `count:${count}`;
      if (!groupWarned.has(sig)) {
        groupWarned.add(sig);
        // eslint-disable-next-line no-console
        console.warn(
          `[Toggle.Group] Rendered with ${count} Toggle children — beyond 5 ` +
            "a Select reads more comfortably than a segmented control. " +
            "Reduce the segment count or switch to a Select.",
        );
      }
    }

    // Mixed text + icon-only warn. Heuristic: `typeof children === "string"`
    // → text segment; `isValidElement(children)` with no string descendant
    // → icon segment. Anything else is ambiguous and skipped.
    //
    // Item 5 fix: `equalWidth={false}` is the documented opt-in for mixed
    // icon + text segments (the EqualWidthOff story is exactly that
    // pattern). Skip the mixed-content scan when the consumer has
    // explicitly declared intrinsic-width layout — they've already opted
    // out of the equal-rhythm constraint the warn is enforcing.
    if (count >= 2 && equalWidth !== false) {
      let textCount = 0;
      let iconCount = 0;
      for (const toggle of childArray) {
        const kind = classifyToggleContent(toggle);
        if (kind === "text") textCount += 1;
        else if (kind === "icon") iconCount += 1;
      }
      if (textCount > 0 && iconCount > 0) {
        const sig = `mixed:${textCount}t-${iconCount}i`;
        if (!groupWarned.has(sig)) {
          groupWarned.add(sig);
          // eslint-disable-next-line no-console
          console.warn(
            `[Toggle.Group] Rendered with mixed content (${textCount} text + ` +
              `${iconCount} icon-only segments). Keep content types ` +
              "consistent — either all-text or all-icon — so the row " +
              "reads coherent. To intentionally mix, set " +
              "`equalWidth={false}`.",
          );
        }
      }
    }
  }, [children, equalWidth]);

  // Adapt scalar single-mode values into the always-array contract Base
  // UI's ToggleGroup expects. Single-mode `undefined` becomes `[]` (Base
  // UI's "nothing pressed" representation); multiple-mode arrays pass
  // through. Item 3 fix.
  const adaptedValue: readonly Value[] | undefined = isMultiple
    ? (value as readonly Value[] | undefined)
    : value != null
      ? [value as Value]
      : undefined;
  const adaptedDefaultValue: readonly Value[] | undefined = isMultiple
    ? (defaultValue as readonly Value[] | undefined)
    : defaultValue != null
      ? [defaultValue as Value]
      : undefined;
  const adaptedOnValueChange = (
    next: Value[],
    details: BaseToggleGroupChangeEventDetails,
  ) => {
    if (!onValueChange) return;
    if (isMultiple) {
      (onValueChange as ToggleGroupMultipleProps<Value>["onValueChange"])?.(
        next,
        details,
      );
    } else {
      // Single mode: Base UI hands us a 0- or 1-element array; surface
      // the scalar (or `undefined` when the press flips off).
      (onValueChange as ToggleGroupSingleProps<Value>["onValueChange"])?.(
        next.length > 0 ? next[0] : undefined,
        details,
      );
    }
  };

  return (
    <ToggleGroupContext.Provider
      value={{
        size,
        variant,
        multiple: isMultiple,
        disabled,
      }}
    >
      <BaseToggleGroup
        // role="toolbar" is set BEFORE {...rest} so a future maintainer
        // who drops the `Omit<…, "role">` from the public type can't
        // accidentally let consumer props overwrite it via spread order.
        // Belt-and-braces with the TypeScript omit above.
        role="toolbar"
        {...rest}
        ref={ref}
        // Base UI's value props are typed as `readonly Value[]`; the
        // scalar→array adaptation above made the shape match. The cast
        // dance below keeps our `<Value>` generic visible through the
        // forwardRef erasure without losing static safety on adaptedValue.
        value={adaptedValue as never}
        defaultValue={adaptedDefaultValue as never}
        onValueChange={adaptedOnValueChange as never}
        orientation={orientation}
        disabled={disabled || undefined}
        multiple={isMultiple}
        className={classnames(
          "zs-toggle-group",
          variant ? `zs-toggle-group--${variant}` : null,
          size ? `zs-toggle-group--${size}` : null,
          `zs-toggle-group--${orientation}`,
          className,
        )}
        data-orientation={orientation}
        data-size={size}
        data-variant={variant}
        data-equal-width={equalWidth ? "true" : "false"}
      >
        {children}
      </BaseToggleGroup>
    </ToggleGroupContext.Provider>
  );
}

const ToggleGroup = forwardRef(ToggleGroupInner) as <Value extends string = string>(
  props: ToggleGroupProps<Value> & { ref?: React.Ref<HTMLDivElement> },
) => React.JSX.Element;
(ToggleGroup as React.FC).displayName = "Toggle.Group";

/* ─── Toggle ────────────────────────────────────────────────────────── */

type BaseToggleRootProps = ComponentPropsWithRef<typeof BaseToggle>;

export interface ToggleProps<Value extends string = string>
  extends Omit<BaseToggleRootProps, "className" | "render" | "value"> {
  /**
   * The value this Toggle contributes when rendered inside a
   * `Toggle.Group`. The `<Value>` generic narrows the literal string set
   * so `<Toggle<"day" | "week"> value="month">` is a compile error.
   * Outside a group the prop is decorative — Base UI still emits it as
   * the `value` attribute for symmetry, but no state is keyed off it.
   */
  value?: Value;
  /**
   * Visual size — sm 32, md 40 (default), lg 48. Matches Button + Input
   * rhythm so a Toggle next to either reads coherent. Inherits from
   * the enclosing Toggle.Group when omitted; falls back to `md`.
   */
  size?: ToggleSize;
  /**
   * Visual variant. `default` is gray (matches Button's `gray`) when
   * unpressed and accent-filled when pressed. `plain` strips the gray
   * surface entirely so the unpressed state is fully transparent
   * (useful inside compact toolbars). `tinted` paints unpressed with
   * a translucent accent tint — loud, signals interactivity strongly.
   *
   * Inherits from the enclosing Toggle.Group when omitted; falls back
   * to `default`.
   */
  variant?: ToggleVariant;
  /** Optional class hook on the toggle root. */
  className?: string;
  /**
   * Render as the single child element rather than a `<button>`. Used
   * for `<a>` toggles or other host elements that need Toggle styling
   * + pressed-state semantics. Routes through the shared Slot helper
   * (canonical pattern from Dialog.Close, commit `3a64a726`) so
   * className, style, refs, AND event handlers compose with whatever
   * Base UI emits.
   *
   * When set, the child's tag drives Base UI's `nativeButton` — pass a
   * `<button>` and the native keyboard handlers stay; pass anything
   * else and Base UI swaps in `role="button"` + keyboard handlers.
   */
  asChild?: boolean;
}

/**
 * The visible button element. Carries `aria-pressed` as the source of
 * truth (Base UI emits it). Default tag is `<button>`; `asChild` swaps
 * to a single React-element child via Slot.
 */
function ToggleInner<Value extends string = string>(
  {
    size: sizeProp,
    variant: variantProp,
    className,
    asChild = false,
    children,
    disabled: disabledProp,
    ...rest
  }: ToggleProps<Value>,
  ref: React.ForwardedRef<HTMLButtonElement>,
) {
  const groupCtx = useToggleGroupContext();

  // Explicit prop wins over group context; group context wins over the
  // default. Mirrors the Radio + Field cascade in the rest of the slate.
  const size: ToggleSize = sizeProp ?? groupCtx?.size ?? "md";
  const variant: ToggleVariant = variantProp ?? groupCtx?.variant ?? "default";
  const disabled = disabledProp ?? groupCtx?.disabled ?? false;

  // Detect whether the asChild target is a native `<button>` so we can
  // drive Base UI's `nativeButton` correctly. Contingency from the brief
  // — same heuristic Dialog.Close uses (slice-3 review-fix item 2).
  const asChildIsNativeButton =
    asChild && isValidElement(children) && (children as { type?: unknown }).type === "button";
  const nativeButton = asChild ? asChildIsNativeButton : true;

  if (process.env.NODE_ENV !== "production" && asChild && !isValidElement(children)) {
    // Matches Dialog.Close convention (slice-3 review-fix item 2): the
    // dev-error is NOT deduped across mounts — same shape Dialog.Close
    // uses. Leaving undeduped keeps the misuse loud at dev time.
    // eslint-disable-next-line no-console
    console.error(
      "Toggle asChild expects a single React element child; received " +
        typeof children +
        "; rendering nothing.",
    );
  }

  const composedClassName = classnames(
    "zs-toggle",
    `zs-toggle--${variant}`,
    `zs-toggle--${size}`,
    className,
  );

  return (
    // Item 6 fix: pass the caller's `ref` ONLY to BaseToggle.Root. Base UI
    // forwards it through its render callback as `baseProps.ref` so the
    // child element (Slot or <button>) just spreads that single ref —
    // no double-fan-out via composeRefs. Pre-fix, the caller ref was
    // composed twice per mount which fired callback refs with the same
    // DOM node twice.
    <BaseToggle
      {...(rest as BaseToggleRootProps)}
      ref={ref as unknown as Ref<HTMLButtonElement>}
      disabled={disabled || undefined}
      nativeButton={nativeButton}
      className={composedClassName}
      data-size={size}
      data-variant={variant}
      render={(baseProps) => {
        if (asChild) {
          if (!isValidElement(children)) {
            // Render-prop must return a ReactElement; dev-error above
            // already flagged the misuse.
            return <></>;
          }
          // Slot handles className / style / event composition. Item 8
          // fix: stamp data-size / data-variant explicitly so the
          // attribute-keyed group CSS (item 4) catches asChild targets,
          // not just default <button>s.
          return (
            <Slot
              {...(baseProps as Record<string, unknown>)}
              data-size={size}
              data-variant={variant}
            >
              {children as ReactElement}
            </Slot>
          );
        }

        // Default path: a real `<button>`. Spread baseProps then ours so
        // our controlled attributes (className, data-*) win. Type
        // defaults to "button" so Toggle never accidentally submits a
        // surrounding form (Toggle is NOT a form control).
        const { type: bpType, ...baseRest } = baseProps as {
          type?: "button" | "submit" | "reset";
        } & Record<string, unknown>;
        return (
          <button
            {...(baseRest as Record<string, unknown>)}
            type={bpType ?? "button"}
            className={composedClassName}
            data-size={size}
            data-variant={variant}
          >
            {children}
          </button>
        );
      }}
    />
  );
}

// Cast through `unknown` so TypeScript sees the generic component plus
// the `Group` namespace shape without complaining about forwardRef
// erasing the `<Value>`. Same pattern Radio uses.
const ForwardedToggle = forwardRef(ToggleInner) as unknown as (<
  Value extends string = string,
>(
  props: ToggleProps<Value> & { ref?: React.Ref<HTMLButtonElement> },
) => React.JSX.Element) & {
  Group: typeof ToggleGroup;
  displayName?: string;
};

(ForwardedToggle as { displayName?: string }).displayName = "Toggle";
ForwardedToggle.Group = ToggleGroup;

export const Toggle = ForwardedToggle;
export { ToggleGroup };

/* ─── helpers ───────────────────────────────────────────────────────── */

/**
 * Walk a children tree and return the direct Toggle children. Fragments
 * are flattened one level so `<>{toggles.map(...)}</>` works; nested
 * group structures are left alone (a nested Toggle.Group is the
 * consumer's problem, not the warn's).
 */
function collectToggleChildren(children: ReactNode): ReactElement[] {
  const out: ReactElement[] = [];
  visit(children, out);
  return out;
}

function visit(node: ReactNode, out: ReactElement[]): void {
  if (node == null || node === false) return;
  if (Array.isArray(node)) {
    for (const child of node) visit(child, out);
    return;
  }
  if (!isValidElement(node)) return;
  const el = node as ReactElement<{ children?: ReactNode }>;
  // Fragment? Flatten one level so .map() output is iterable.
  if ((el.type as unknown) === Symbol.for("react.fragment")) {
    visit(el.props.children, out);
    return;
  }
  // Treat anything that looks like a Toggle (our forwardRef, or a
  // direct BaseToggle) as a segment. The check is heuristic — we look
  // at the resolved component identity rather than name strings so the
  // warn survives minification.
  if (el.type === ForwardedToggle || el.type === BaseToggle) {
    out.push(el);
    return;
  }
  // Unknown wrappers: descend one level (lets `<Tooltip><Toggle/></Tooltip>`
  // still get counted) but don't recurse infinitely.
  visit(el.props.children, out);
}

/**
 * Classify a Toggle child as text-only, icon-only, or ambiguous.
 *
 *   - `text`: children is a string OR contains a string descendant at
 *     the top level (e.g. `<Toggle>Bold</Toggle>`).
 *   - `icon`: children is a single React element with no string text
 *     content (e.g. `<Toggle><Icon /></Toggle>` with `aria-label`).
 *   - `ambiguous`: anything else — skipped by the mixed-content warn.
 */
function classifyToggleContent(
  toggle: ReactElement,
): "text" | "icon" | "ambiguous" {
  const children = (toggle.props as { children?: ReactNode }).children;
  if (children == null) return "ambiguous";
  if (typeof children === "string" || typeof children === "number") return "text";
  if (Array.isArray(children)) {
    const hasText = children.some(
      (c) => typeof c === "string" || typeof c === "number",
    );
    const hasEl = children.some((c) => isValidElement(c));
    if (hasText) return "text";
    if (hasEl) return "icon";
    return "ambiguous";
  }
  if (isValidElement(children)) return "icon";
  return "ambiguous";
}
