import {
  cloneElement,
  forwardRef,
  isValidElement,
  useEffect,
  useImperativeHandle,
  useRef,
  type ButtonHTMLAttributes,
  type CSSProperties,
  type MutableRefObject,
  type ReactElement,
  type ReactNode,
  type Ref,
  type SyntheticEvent,
} from "react";

export type ButtonVariant = "filled" | "tinted" | "gray" | "plain";
export type ButtonIntent = "normal" | "destructive";
export type ButtonSize = "small" | "medium" | "large";

export interface ButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children"> {
  /**
   * Visual style — HIG button styles.
   * - `filled`: prominent, accent fill, white text. The "primary action" look.
   * - `tinted`: translucent accent-tinted fill, accent text. Secondary action.
   * - `gray`: neutral fill, label text. Tertiary action.
   * - `plain`: no chrome, accent text. Link-style.
   *
   * Defaults to `filled`.
   */
  variant?: ButtonVariant;

  /**
   * Semantic intent — HIG button role.
   * - `normal`: no special meaning.
   * - `destructive`: destructive action; OVERRIDES the accent palette
   *   with system-red regardless of variant (except `gray`, where only
   *   the label takes the red).
   *
   * Defaults to `normal`. Renamed from `role` to avoid shadowing the
   * native ARIA `role` attribute on every focusable element.
   *
   * Emitted as `data-intent="..."` for styling/testing hooks; not
   * consumed by assistive tech.
   */
  intent?: ButtonIntent;

  /** Size — small 32px, medium 40px (default), large 48px. */
  size?: ButtonSize;

  /**
   * Activity indicator. Per HIG, show this for actions that don't
   * instantly complete. While loading, the button is `aria-busy`,
   * forced `disabled`, label/slots are hidden via `opacity: 0` to
   * preserve width and the accessible name, and an absolutely-centered
   * spinner is shown.
   */
  loading?: boolean;

  /** Leading element (icon, etc.). */
  startSlot?: ReactNode;

  /** Trailing element. */
  endSlot?: ReactNode;

  /** Label content. */
  children?: ReactNode;

  /**
   * Render as the single child element rather than a `<button>`. Used
   * for anchors ("Learn more", "Open in browser") that should adopt
   * Button styling while keeping native anchor semantics.
   *
   * When `asChild` is set:
   * - The `<button>` wrapper is replaced with the child element.
   * - The `type="button"` default is dropped (anchors don't take it).
   * - Click semantics follow the child's native rules — for anchors,
   *   only Enter/click activate (no Space).
   * - During loading/disabled, `aria-disabled`/`aria-busy` are still
   *   applied to the child; the HTML `disabled` attribute is reserved
   *   for real `<button>` elements.
   */
  asChild?: boolean;
}

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

/* ─── inline Slot (no @radix-ui/react-slot dep) ──────────────────────── */
/*
 * Merges our props onto a single React-element child. Handlers compose
 * (child's handler runs first, ours after — child can preventDefault if
 * it wants), className concatenates, style shallow-merges, refs fan out.
 */
type SlotProps = Record<string, unknown> & {
  children?: ReactNode;
};

function setRef<T>(ref: Ref<T> | undefined, value: T | null) {
  if (typeof ref === "function") {
    ref(value);
  } else if (ref && typeof ref === "object") {
    (ref as MutableRefObject<T | null>).current = value;
  }
}

function composeRefs<T>(...refs: Array<Ref<T> | undefined>) {
  return (value: T | null) => {
    for (const ref of refs) setRef(ref, value);
  };
}

function mergeProps(
  ours: Record<string, unknown>,
  theirs: Record<string, unknown>,
): Record<string, unknown> {
  const merged: Record<string, unknown> = { ...theirs, ...ours };

  for (const key of Object.keys(ours)) {
    const oursValue = ours[key];
    const theirsValue = theirs[key];

    if (key === "className" && typeof oursValue === "string" && typeof theirsValue === "string") {
      merged[key] = classnames(theirsValue, oursValue);
    } else if (key === "style" && oursValue && theirsValue) {
      merged[key] = { ...(theirsValue as CSSProperties), ...(oursValue as CSSProperties) };
    } else if (/^on[A-Z]/.test(key) && typeof oursValue === "function" && typeof theirsValue === "function") {
      merged[key] = (event: SyntheticEvent) => {
        (theirsValue as (e: SyntheticEvent) => void)(event);
        if (!event.defaultPrevented) {
          (oursValue as (e: SyntheticEvent) => void)(event);
        }
      };
    }
  }

  return merged;
}

function Slot({ children, ...props }: SlotProps) {
  if (!isValidElement(children)) return null;
  const child = children as ReactElement<Record<string, unknown>> & {
    ref?: Ref<unknown>;
  };
  const merged = mergeProps(
    props as Record<string, unknown>,
    child.props as Record<string, unknown>,
  );
  if ((props as { ref?: Ref<unknown> }).ref || child.ref) {
    merged.ref = composeRefs(
      (props as { ref?: Ref<unknown> }).ref,
      child.ref,
    );
  }
  return cloneElement(child, merged);
}

/**
 * Inline spinner SVG. Honors prefers-reduced-motion via Button.css.
 * `stroke-width="2"` is an SVG viewBox unit (not a CSS px), so it is
 * outside the no-raw-px rule.
 */
function Spinner() {
  return (
    <span className="zs-button__spinner" aria-hidden="true">
      <svg viewBox="0 0 16 16" focusable="false">
        <circle cx="8" cy="8" r="6" />
      </svg>
    </span>
  );
}

export const Button = forwardRef<HTMLElement, ButtonProps>(function Button(
  {
    variant = "filled",
    intent = "normal",
    size = "medium",
    loading = false,
    startSlot,
    endSlot,
    children,
    className,
    disabled,
    type,
    asChild = false,
    "aria-label": ariaLabel,
    ...rest
  },
  ref,
) {
  const composedClassName = classnames(
    "zs-button",
    `zs-button--${variant}`,
    `zs-button--${size}`,
    intent === "destructive" && "zs-button--destructive",
    className,
  );

  const isBusy = loading === true;
  const isDisabled = disabled === true || isBusy;

  // Dev-mode accessible-name guard: warn once per mount if the button
  // has no visible text and no aria-label / aria-labelledby. Helps catch
  // icon-only buttons missing a name.
  const localRef = useRef<HTMLElement | null>(null);
  useImperativeHandle(ref, () => localRef.current as HTMLElement, []);
  useEffect(() => {
    if (typeof process === "undefined" || process.env?.NODE_ENV === "production") return;
    const node = localRef.current;
    if (!node) return;
    const text = node.textContent?.trim() ?? "";
    const labelled =
      node.getAttribute("aria-label")?.trim() ||
      node.getAttribute("aria-labelledby")?.trim();
    if (!text && !labelled) {
      // eslint-disable-next-line no-console
      console.warn(
        "[Button] Rendered without an accessible name. Provide visible text, " +
          "`aria-label`, or `aria-labelledby` so screen readers can announce it.",
      );
    }
  }, []);

  const buildInner = (labelContent: ReactNode) => (
    <span className="zs-button__inner">
      {startSlot != null ? <span className="zs-button__start">{startSlot}</span> : null}
      <span className="zs-button__label">{labelContent}</span>
      {endSlot != null ? <span className="zs-button__end">{endSlot}</span> : null}
    </span>
  );

  if (asChild) {
    // Slot path: render the single child element (e.g., an <a>) with
    // Button styling. The child's own children become the label content
    // so we don't nest anchors. No HTML `disabled` (anchors don't
    // support it); express state via `aria-disabled` + `aria-busy`.
    // No default `type` (anchors don't take it).
    if (!isValidElement(children)) return null;
    const onlyChild = children as ReactElement<{ children?: ReactNode }>;
    const labelContent = onlyChild.props.children;
    const wrappedChild = cloneElement(
      onlyChild,
      undefined,
      <>
        {buildInner(labelContent)}
        {isBusy ? <Spinner /> : null}
      </>,
    );
    return (
      <Slot
        {...rest}
        ref={localRef as Ref<unknown>}
        className={composedClassName}
        data-variant={variant}
        data-intent={intent}
        data-size={size}
        aria-busy={isBusy || undefined}
        aria-disabled={isDisabled || undefined}
        aria-label={ariaLabel}
      >
        {wrappedChild}
      </Slot>
    );
  }

  const inner = buildInner(children);

  return (
    // Spreading {...rest} FIRST means our controlled attributes
    // (data-variant, aria-busy, disabled, etc.) win over any caller
    // overrides — same shape as Radix's component composition.
    //
    // Note on disabled-during-loading: we keep the HTML `disabled`
    // attribute set when `aria-busy` is true. Trade-off: HTML disabled
    // is correct because the button cannot be activated; some screen
    // readers stop announcing state on disabled buttons, but
    // `aria-busy="true"` is announced separately and is the proper
    // signal for "work in progress". `aria-disabled` + activation
    // guards would let focus persist on the busy button — desirable
    // for some flows; revisit when a real use case forces it.
    <button
      {...rest}
      ref={localRef as Ref<HTMLButtonElement>}
      type={type ?? "button"}
      className={composedClassName}
      data-variant={variant}
      data-intent={intent}
      data-size={size}
      aria-busy={isBusy || undefined}
      aria-label={ariaLabel}
      disabled={isDisabled}
    >
      {inner}
      {isBusy ? <Spinner /> : null}
    </button>
  );
});

Button.displayName = "Button";
