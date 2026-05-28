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
import { Slot, composeRefs } from "../_slot";
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

export interface ToggleGroupProps<Value extends string = string>
  extends Omit<
    BaseToggleGroupProps,
    "className" | "render" | "value" | "defaultValue" | "onValueChange"
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
   * segments mix with text-only ones.
   *
   * @default true
   */
  equalWidth?: boolean;
  /** Optional class hook on the group root. */
  className?: string;
  /** Controlled value(s) of pressed segments. */
  value?: readonly Value[];
  /** Uncontrolled initial value(s). */
  defaultValue?: readonly Value[];
  /** Pressed-state change callback. */
  onValueChange?: (value: Value[], eventDetails: unknown) => void;
  /** Toggle children — typically 2–5. */
  children?: ReactNode;
}

/**
 * Module-level dedup map for dev warnings. We key on a stable signature
 * derived from the warning identity so React's StrictMode double-render
 * and noisy reload-heavy dev sessions don't drown in repeat-warnings.
 * The whole branch is DCE'd in production via the static NODE_ENV check.
 */
const groupWarned = new Set<string>();

function ToggleGroupInner<Value extends string = string>(
  {
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
  }: ToggleGroupProps<Value>,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const disabled = disabledProp ?? false;

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
    if (count >= 2) {
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
              "reads coherent.",
          );
        }
      }
    }
  }, [children]);

  return (
    <ToggleGroupContext.Provider
      value={{
        size,
        variant,
        multiple: multiple === true,
        disabled,
      }}
    >
      <BaseToggleGroup
        {...rest}
        ref={ref}
        // Base UI's value props are untyped (`unknown` at the public
        // surface). The `as never` keeps our `<Value>` generic visible
        // to TypeScript while threading through cleanly — no `any`.
        value={value as never}
        defaultValue={defaultValue as never}
        onValueChange={onValueChange as never}
        orientation={orientation}
        disabled={disabled || undefined}
        multiple={multiple}
        // Base UI defaults `role="group"` which doesn't permit
        // `aria-orientation` (axe `aria-allowed-attr`). The semantic
        // fit for a segmented control is `role="toolbar"` (a row of
        // pressable buttons that move via arrow keys) — that role
        // DOES allow `aria-orientation`. Consumer can still override
        // via `rest.role` since `{...rest}` is spread first.
        role="toolbar"
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
  extends Omit<BaseToggleRootProps, "className" | "render"> {
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
    <BaseToggle
      {...(rest as BaseToggleRootProps)}
      ref={ref as unknown as Ref<HTMLButtonElement>}
      disabled={disabled || undefined}
      nativeButton={nativeButton}
      className={composedClassName}
      data-size={size}
      data-variant={variant}
      render={(baseProps) => {
        const basePropsRef = (baseProps as { ref?: Ref<unknown> }).ref;

        if (asChild) {
          if (!isValidElement(children)) {
            // Render-prop must return a ReactElement; dev-error above
            // already flagged the misuse.
            return <></>;
          }
          // Slot handles className / style / event composition AND ref
          // fan-out for the React 19 `element.props.ref` location.
          return (
            <Slot
              {...baseProps}
              ref={composeRefs(
                ref as Ref<unknown>,
                basePropsRef,
              )}
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
            ref={composeRefs(
              ref as Ref<HTMLButtonElement>,
              basePropsRef as Ref<HTMLButtonElement>,
            ) as unknown as Ref<HTMLButtonElement>}
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
