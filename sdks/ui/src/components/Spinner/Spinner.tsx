/*
 * Spinner — standalone activity indicator.
 *
 * A rotating ring for "work in progress" surfaces (page loads, inline
 * fetches, button-adjacent waits). Distinct from the Button's internal
 * spinner: this one is a self-contained status region with an
 * accessible name.
 *
 * Aria — the root is `role="status"` (a polite live region). The visible
 * `label` (default "Loading") is rendered inside a
 * `<span class="zs-visually-hidden">` so screen readers announce it
 * while sighted users see only the ring. The ring element itself is
 * `aria-hidden` so it isn't double-announced. `aria-live="polite"` is
 * implied by `role="status"`; we leave the implicit default.
 *
 * Motion — the rotation is DISABLED under
 * `prefers-reduced-motion: reduce`. We stop the spin entirely and leave
 * the static ring/arc painted: a non-animated busy indicator is an
 * acceptable (and accessible) reduced-motion fallback, and the
 * `role="status"` label still conveys "loading" to AT. See Spinner.css.
 */
import { forwardRef, useId, type ComponentPropsWithoutRef } from "react";
import { classnames } from "../_classnames";

export type SpinnerSize = "sm" | "md" | "lg";

export interface SpinnerProps extends ComponentPropsWithoutRef<"span"> {
  /** Size — `sm` / `md` (default) / `lg`. */
  size?: SpinnerSize;

  /**
   * Accessible name announced by screen readers. Rendered
   * visually-hidden (not on screen). Default `"Loading"`.
   */
  label?: string;
}

export const Spinner = forwardRef<HTMLSpanElement, SpinnerProps>(
  function Spinner(
    {
      size = "md",
      label = "Loading",
      className,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      ...rest
    },
    ref,
  ) {
    // `role="status"` is NOT a name-from-content role, so the inner
    // visually-hidden text alone does not become the region's accessible
    // name. When the consumer names the region themselves (via
    // `aria-label` or `aria-labelledby`) THEIR value wins and we emit no
    // internal label span. Otherwise we wire a generated `aria-labelledby`
    // to a visually-hidden span carrying the default `label` so screen
    // readers (and getByRole({ name })) resolve the name off-screen.
    const labelId = useId();
    const consumerNamed = ariaLabel != null || ariaLabelledBy != null;
    return (
      <span
        {...rest}
        {...(ariaLabel != null ? { "aria-label": ariaLabel } : null)}
        aria-labelledby={consumerNamed ? ariaLabelledBy : labelId}
        ref={ref}
        role="status"
        data-slot="spinner"
        data-size={size}
        className={classnames("zs-spinner", `zs-spinner--${size}`, className)}
      >
        {/* The visible ring. Decorative — the consumer-supplied name (or
            the visually-hidden label below) carries the accessible name. */}
        <span className="zs-spinner__ring" aria-hidden="true" />
        {consumerNamed ? null : (
          <span id={labelId} className="zs-visually-hidden">
            {label}
          </span>
        )}
      </span>
    );
  },
);
Spinner.displayName = "Spinner";
