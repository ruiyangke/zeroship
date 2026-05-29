/*
 * Skeleton — content placeholder shown while data loads.
 *
 * A purely decorative shimmer block that reserves layout space for
 * content that hasn't arrived yet. Three shapes:
 *
 *   - `text`   (default): a text-line bar. `lines > 1` renders N stacked
 *                          bars with the last bar shortened so the block
 *                          reads as a paragraph.
 *   - `rect`   : a rectangle (cards, thumbnails, media). `--zs-radius-2`.
 *   - `circle` : a square forced to a full-radius circle (avatars).
 *
 * `width` / `height` are free CSS lengths applied as logical sizes.
 *
 * Aria — `aria-hidden="true"`, no role. A Skeleton conveys NOTHING to
 * assistive tech: the page's status/Spinner (a `role="status"` region)
 * is what announces "loading". A skeleton that announced itself would
 * spam the SR with N meaningless placeholder nodes.
 *
 * Motion — the shimmer animation is DISABLED under
 * `prefers-reduced-motion: reduce` (the static fill stays painted, so
 * the placeholder is still visible — only the motion stops). See
 * Skeleton.css.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type CSSProperties,
} from "react";
import { classnames } from "../../components/_classnames";

export type SkeletonVariant = "text" | "rect" | "circle";

export interface SkeletonProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Placeholder shape.
   * - `text` (default): a text-line bar.
   * - `rect`: a rectangle with a small corner radius.
   * - `circle`: a full-radius circle (square aspect).
   */
  variant?: SkeletonVariant;

  /**
   * Number of stacked text bars (the `text` variant only). Default `1`.
   * With `lines > 1` the last bar is shortened so the block reads as a
   * paragraph. Ignored for `rect` / `circle`.
   */
  lines?: number;

  /** Explicit inline-size (a free CSS length, e.g. `"12rem"`, `"60%"`). */
  width?: string;

  /**
   * Explicit block-size (a free CSS length). Sizes the block / container
   * (for `lines > 1`, the lines container); per-line height derives from
   * the text line-height type token, not this value.
   */
  height?: string;
}

export const Skeleton = forwardRef<HTMLDivElement, SkeletonProps>(
  function Skeleton(
    { variant = "text", lines = 1, width, height, className, style, ...rest },
    ref,
  ) {
    // Sizes flow as logical CSS properties via inline style; only set a
    // dimension when the consumer supplied it so the CSS defaults win.
    const sizeStyle: CSSProperties = {
      ...(width != null ? { inlineSize: width } : null),
      ...(height != null ? { blockSize: height } : null),
      ...style,
    };

    // Multi-line text → a container of N bars. The container carries the
    // aria-hidden + data-slot and the computed size style so width/height
    // size the whole block; each bar sizes from the container (the last
    // bar is shortened in CSS so the block reads as a paragraph).
    if (variant === "text" && lines > 1) {
      return (
        <div
          {...rest}
          ref={ref}
          aria-hidden="true"
          data-slot="skeleton"
          data-variant="text"
          data-lines={lines}
          className={classnames("zs-skeleton-lines", className)}
          style={sizeStyle}
        >
          {Array.from({ length: lines }, (_, i) => (
            <span
              key={i}
              data-slot="skeleton-line"
              className="zs-skeleton zs-skeleton--text"
            />
          ))}
        </div>
      );
    }

    return (
      <div
        {...rest}
        ref={ref}
        aria-hidden="true"
        data-slot="skeleton"
        data-variant={variant}
        className={classnames("zs-skeleton", `zs-skeleton--${variant}`, className)}
        style={sizeStyle}
      />
    );
  },
);
Skeleton.displayName = "Skeleton";
