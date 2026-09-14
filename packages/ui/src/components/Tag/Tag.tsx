/*
 * Tag — interactive chip. Generalizes the builder's Pill / FilterPill.
 *
 * Two MUTUALLY EXCLUSIVE modes, chosen to avoid a button-in-button
 * (which is invalid HTML and an AT nightmare):
 *
 *   1. Removable / static (default, or `removable`): the ROOT is a
 *      `<span>`. When `removable`, a trailing real
 *      `<button type="button" aria-label="Remove …">` is rendered; the
 *      glyph inside it is `aria-hidden`. The remove button fires
 *      `onRemove`. As an affordance, pressing Backspace or Delete while
 *      the remove button has focus also fires `onRemove`.
 *      Because the root is a span, the trailing button is the only
 *      interactive descendant — no nesting.
 *
 *   2. Filter toggle (`selected` / `onSelectedChange` provided): the
 *      ROOT ITSELF is a `<button type="button" aria-pressed={selected}>`
 *      that toggles `onSelectedChange(!selected)`. The selected state
 *      paints with `--zs-accent`. There is no remove button in this mode
 *      (a removable filter chip would nest a button inside a button).
 *
 * If BOTH `removable` and a selectable signal (`selected` /
 * `onSelectedChange`) are passed, the modes conflict. We resolve to
 * FILTER mode (the root is the interactive element) and dev-warn,
 * dropping `removable`. Precedence: filter wins because making the root
 * itself the button is the only way to keep a single interactive
 * element — honoring `removable` would force a button inside the filter
 * button.
 *
 * Accessible remove label: derived from the children when they are a
 * plain string (`Remove {text}`). When the children are not a string,
 * we cannot synthesize a meaningful label, so the consumer MUST pass
 * `removeLabel` (a dev-warn fires and a generic "Remove" fallback is
 * used otherwise).
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type Ref,
} from "react";
import { classnames } from "../_classnames";

export type TagSize = "sm" | "md";

export interface TagProps extends ComponentPropsWithoutRef<"span"> {
  /** Size — `sm` (caption type) or `md` (footnote type). Default `md`. */
  size?: TagSize;

  /**
   * Decorative leading element (an icon, a color swatch). Wrapped in an
   * `aria-hidden` span so it never pollutes the accessible name.
   */
  leadingIcon?: ReactNode;

  /**
   * Removable mode. Renders a trailing remove (×) `<button>`. The root
   * stays a `<span>` so the remove button is the only interactive
   * descendant. Mutually exclusive with the filter props
   * (`selected` / `onSelectedChange`) — passing both resolves to filter
   * mode with a dev-warn.
   */
  removable?: boolean;

  /** Fired when the remove button is activated, or Backspace/Delete is
   *  pressed while the remove button has focus. */
  onRemove?: () => void;

  /**
   * Explicit accessible label for the remove button. Required when the
   * children are not a plain string (we can only auto-derive
   * `Remove {text}` from string children). Ignored outside removable
   * mode.
   */
  removeLabel?: string;

  /**
   * Filter-toggle state. Providing `selected` (or `onSelectedChange`)
   * switches the root to a `<button aria-pressed>`. Mutually exclusive
   * with `removable`.
   *
   * Filter mode is CONTROLLED — Tag holds no internal selected state.
   * Pass `selected` and update it from `onSelectedChange`; otherwise the
   * chip fires `onSelectedChange(true)` forever and never visually
   * presses (`aria-pressed` stays false).
   */
  selected?: boolean;

  /**
   * Fired with the next selected state when the filter root is toggled.
   *
   * Filter mode is CONTROLLED — pass `selected` and update it inside this
   * handler. Providing `onSelectedChange` without a boolean `selected`
   * dev-warns (the chip can never press).
   */
  onSelectedChange?: (selected: boolean) => void;

  /** Tag label. */
  children?: ReactNode;
}

/** Trailing remove (×) glyph button. The glyph is aria-hidden; the
 *  accessible name comes from `aria-label`. */
function RemoveButton({
  label,
  size,
  onRemove,
}: {
  label: string;
  size: TagSize;
  onRemove?: () => void;
}) {
  return (
    <button
      type="button"
      data-slot="tag-remove"
      className="zs-tag__remove"
      aria-label={label}
      data-size={size}
      onClick={(event: ReactMouseEvent<HTMLButtonElement>) => {
        // Stop the click from bubbling to any consumer-attached handler
        // on the tag wrapper (e.g. a row click); removing is its own
        // intent.
        event.stopPropagation();
        onRemove?.();
      }}
      onKeyDown={(event: ReactKeyboardEvent<HTMLButtonElement>) => {
        // Backspace/Delete on the focused remove button also removes —
        // matches the tag-level affordance below so either focus target
        // honors the same keys.
        if (event.key === "Backspace" || event.key === "Delete") {
          event.preventDefault();
          event.stopPropagation();
          onRemove?.();
        }
      }}
    >
      <span aria-hidden="true" className="zs-tag__remove-glyph">
        {/* Multiplication sign — the canonical close glyph. */}
        ×
      </span>
    </button>
  );
}

export const Tag = forwardRef<HTMLElement, TagProps>(function Tag(
  {
    size = "md",
    leadingIcon,
    removable = false,
    onRemove,
    removeLabel,
    selected,
    onSelectedChange,
    className,
    children,
    onKeyDown,
    onClick,
    ...rest
  },
  ref,
) {
  // Filter mode is signaled by EITHER the controlled `selected` value or
  // an `onSelectedChange` handler being present. We detect presence (not
  // truthiness) so a controlled `selected={false}` filter still enters
  // filter mode.
  const isFilter = selected !== undefined || onSelectedChange !== undefined;
  // Resolve the both-modes conflict toward filter (root-is-button is the
  // only single-interactive-element shape).
  const conflict = isFilter && removable;
  const showRemove = removable && !isFilter;

  if (process.env.NODE_ENV !== "production" && conflict) {
    // eslint-disable-next-line no-console
    console.warn(
      "Tag received both `removable` and a filter prop (`selected` / " +
        "`onSelectedChange`). These modes are mutually exclusive — a " +
        "removable filter chip would nest a button inside a button. " +
        "Resolving to filter mode and ignoring `removable`.",
    );
  }

  if (
    process.env.NODE_ENV !== "production" &&
    onSelectedChange !== undefined &&
    typeof selected !== "boolean"
  ) {
    // eslint-disable-next-line no-console
    console.warn(
      "Tag filter mode is controlled: `onSelectedChange` was provided " +
        "without a boolean `selected`. The chip will fire " +
        "`onSelectedChange(true)` on every activation and never visually " +
        "press (`aria-pressed` stays false). Pass `selected` and update it " +
        "inside `onSelectedChange`.",
    );
  }

  // Derive the remove button's accessible name. Prefer an explicit
  // `removeLabel`; else `Remove {text}` when children is a string; else
  // a generic fallback + dev-warn (we can't introspect arbitrary nodes).
  const childIsString = typeof children === "string";
  let resolvedRemoveLabel = removeLabel;
  if (showRemove && resolvedRemoveLabel == null) {
    if (childIsString) {
      resolvedRemoveLabel = `Remove ${children}`;
    } else {
      resolvedRemoveLabel = "Remove";
      if (process.env.NODE_ENV !== "production") {
        // eslint-disable-next-line no-console
        console.warn(
          "Tag is removable but its children are not a plain string, so " +
            "the remove button's accessible name cannot be derived. Pass " +
            "`removeLabel` (e.g. removeLabel=\"Remove React\") so screen " +
            "readers announce what is being removed. Falling back to " +
            '"Remove".',
        );
      }
    }
  }

  const composedClassName = classnames(
    "zs-tag",
    `zs-tag--${size}`,
    isFilter && "zs-tag--filter",
    showRemove && "zs-tag--removable",
    className,
  );

  const inner = (
    <>
      {leadingIcon != null ? (
        <span aria-hidden="true" className="zs-tag__icon">
          {leadingIcon}
        </span>
      ) : null}
      <span className="zs-tag__label">{children}</span>
    </>
  );

  /* ─── Filter mode: the root IS a <button aria-pressed>. ─────────────── */
  if (isFilter) {
    const isSelected = selected === true;
    return (
      <button
        {...rest}
        ref={ref as Ref<HTMLButtonElement>}
        type="button"
        data-slot="tag"
        data-size={size}
        data-selected={isSelected ? "" : undefined}
        className={composedClassName}
        aria-pressed={isSelected}
        onClick={(event: ReactMouseEvent<HTMLButtonElement>) => {
          (onClick as ((e: ReactMouseEvent<HTMLButtonElement>) => void) | undefined)?.(
            event,
          );
          if (event.defaultPrevented) return;
          onSelectedChange?.(!isSelected);
        }}
        onKeyDown={
          onKeyDown as
            | ((e: ReactKeyboardEvent<HTMLButtonElement>) => void)
            | undefined
        }
      >
        {inner}
      </button>
    );
  }

  /* ─── Removable / static mode: the root is a <span>. ────────────────── */
  // The Backspace/Delete remove affordance lives entirely on the remove
  // <button> (see RemoveButton). The root span has no tabIndex so it can
  // never hold focus — a span-level keydown branch would be unreachable,
  // so we only forward the consumer's own onKeyDown.

  return (
    <span
      {...rest}
      ref={ref as Ref<HTMLSpanElement>}
      data-slot="tag"
      data-size={size}
      className={composedClassName}
      onClick={onClick as ((e: ReactMouseEvent<HTMLSpanElement>) => void) | undefined}
      onKeyDown={
        onKeyDown as
          | ((e: ReactKeyboardEvent<HTMLSpanElement>) => void)
          | undefined
      }
    >
      {inner}
      {showRemove ? (
        <RemoveButton
          label={resolvedRemoveLabel ?? "Remove"}
          size={size}
          onRemove={onRemove}
        />
      ) : null}
    </span>
  );
});

Tag.displayName = "Tag";
