/*
 * ScrollArea — custom-styled scrollbar wrapped around native scroll.
 *
 * Wraps Base UI's `ScrollArea` primitive. The mental model is: the
 * Viewport is the real overflow container (the browser scrolls it
 * normally — inertia, smooth scroll, scroll-snap, keyboard, accessible
 * scroll restoration all keep working); the Scrollbar + Thumb are
 * decorative chrome painted ON TOP of the native scroll and driven by
 * Base UI's overflow observers. We never reinvent inertia or
 * scroll-snap; we only restyle the bar.
 *
 *   <ScrollArea>                           // shorthand
 *     <p>Long content…</p>
 *   </ScrollArea>
 *
 *   <ScrollArea.Root>                      // compound
 *     <ScrollArea.Viewport>
 *       <ScrollArea.Content>…</ScrollArea.Content>
 *     </ScrollArea.Viewport>
 *     <ScrollArea.Scrollbar orientation="vertical">
 *       <ScrollArea.Thumb />
 *     </ScrollArea.Scrollbar>
 *     <ScrollArea.Corner />
 *   </ScrollArea.Root>
 *
 * Anatomy notes:
 *
 *   - Root: owns the visibility policy (`type`), the hide delay, and
 *     the orientation choice that drives which Scrollbar(s) the
 *     shorthand renders. Base UI's Root itself is dumb — it just emits
 *     `data-scrolling` / `data-has-overflow-x` / `data-has-overflow-y`
 *     state attributes; our shell uses those to drive visibility CSS.
 *
 *   - Viewport: `overflow: hidden` (Base UI's pattern). The real
 *     overflow happens INSIDE the Viewport on the Content wrapper —
 *     Base UI handles that detail; we just style the Viewport surface.
 *
 *   - Content: a block-display wrapper around the user's children. We
 *     keep it minimal so consumers can lay out children however they
 *     want (list, grid, prose, table).
 *
 *   - Scrollbar: the bar track. `orientation` selects vertical (the
 *     default) or horizontal. We position vertical bars at
 *     `inset-inline-end: 0` (logical-property cascade flips to the
 *     physical LEFT edge under RTL — the conventional mirror) and
 *     horizontal bars at `inset-block-end: 0`.
 *
 *   - Thumb: the draggable handle. Native pointer handling lives in
 *     Base UI; we just style it (pill, label-tinted, hover/active
 *     deepens via color-mix).
 *
 *   - Corner: the L-shaped intersection between vertical and horizontal
 *     scrollbars (only painted when both bars are visible).
 *
 * Visibility policy (`type` prop):
 *
 *   - `auto`   — bars appear while scrolling, fade after `scrollHideDelay`.
 *   - `always` — bars always visible (classic scroll-view).
 *   - `scroll` — bars only show during active scrolling; fade
 *                immediately when scrolling stops.
 *   - `hover`  — bars only show while the pointer hovers the Viewport
 *                (and during active scroll).
 *
 * The policy is encoded as `data-visibility` on the Root; CSS rules
 * scope to that attribute so the visibility behavior is purely
 * declarative — no JS timers in our layer (Base UI's `data-scrolling`
 * attribute drives the active-scroll signal; CSS `transition-delay`
 * implements the fade-out for `auto`).
 *
 * Reduced motion + forced colors + RTL: every painted rule has a
 * specificity-matched mirror under `@media (prefers-reduced-motion: reduce)`,
 * `@media (forced-colors: active)`, and `[dir="rtl"]` so the high-
 * contrast and right-to-left passes don't silently demote our
 * overrides.
 *
 * Anti-patterns this shell explicitly avoids:
 *
 *  - No `maxHeight` / `maxWidth` numeric props — the parent constrains
 *    the container. ScrollArea reads as a transparent enhancement of
 *    overflow, not a sizing primitive.
 *  - No `onScroll` prop on the Root — the consumer attaches it to the
 *    Viewport (`<ScrollArea.Viewport onScroll={…}>`) so the event
 *    fires on the actual scroll container.
 *  - No `barColor` / `barSize` numeric props — color via `--zs-*`
 *    tokens; size variants would be a future `variant="hairline" |
 *    "regular"` token discriminator, not raw pixel inputs.
 */
import {
  createContext,
  forwardRef,
  useContext,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { ScrollArea as BaseScrollArea } from "@base-ui/react/scroll-area";
import { classnames } from "../_classnames";

/**
 * Visibility policy for the scrollbar chrome.
 *
 * - `auto`: bars appear while scrolling and fade after
 *   `scrollHideDelay`. The default.
 * - `always`: bars are always visible (classic desktop scroll-view).
 * - `scroll`: bars only show during active scrolling; fade immediately
 *   when scrolling stops.
 * - `hover`: bars only show while the pointer is hovering the
 *   Viewport (and during active scroll).
 */
export type ScrollAreaType = "auto" | "always" | "scroll" | "hover";

/**
 * Which axes' Scrollbars the shorthand renders.
 *
 * - `vertical`: vertical bar only (the most common case — long lists,
 *   prose, sidebars).
 * - `horizontal`: horizontal bar only (wide tables, image strips).
 * - `both`: both bars plus a Corner where they meet.
 */
export type ScrollAreaOrientation = "vertical" | "horizontal" | "both";

/* ─── root-local context ────────────────────────────────────────────── *
 *
 * Carries the resolved visibility policy + orientation down to children
 * so the compound API picks up the same defaults without re-passing
 * props on every nested element. Explicit props on a child (e.g. a
 * `Scrollbar orientation="horizontal"`) still win — context is the
 * fallback. */
interface ScrollAreaContextValue {
  type: ScrollAreaType;
  orientation: ScrollAreaOrientation;
  scrollHideDelay: number;
}

const ScrollAreaContext = createContext<ScrollAreaContextValue | null>(null);

function useScrollAreaContext(): ScrollAreaContextValue {
  return (
    useContext(ScrollAreaContext) ?? {
      type: "auto",
      orientation: "vertical",
      scrollHideDelay: 600,
    }
  );
}

/* ─── ScrollArea.Root ───────────────────────────────────────────────── */

type BaseRootProps = ComponentPropsWithRef<typeof BaseScrollArea.Root>;

export interface ScrollAreaRootProps
  extends Omit<BaseRootProps, "render" | "className" | "onScroll"> {
  /**
   * Visibility policy for the scrollbar chrome. Drives `data-visibility`
   * on the Root which CSS scopes to. Native scroll behavior (inertia,
   * smooth-scroll, scroll-snap, keyboard) is unaffected by this prop
   * — only the bar chrome's appearance changes.
   *
   * @default "auto"
   */
  type?: ScrollAreaType;
  /**
   * Milliseconds the scrollbar stays visible after scrolling stops
   * before fading out. Only applies when `type="auto"` (the default).
   * Implemented as a CSS `transition-delay` on the bar's opacity
   * fade-out so no JS timer is involved.
   *
   * @default 600
   */
  scrollHideDelay?: number;
  /**
   * Which axes the shorthand `<ScrollArea>` renders Scrollbars for.
   * Ignored when consumers compose `Root` + `Viewport` + `Content` +
   * their own `Scrollbar`(s) directly — in compound usage the consumer
   * chooses the orientations explicitly.
   *
   * @default "vertical"
   */
  orientation?: ScrollAreaOrientation;
  /** Optional class hook on the Root container. */
  className?: string;
  /** Root subtree — Viewport + Scrollbar(s) + Corner. */
  children?: ReactNode;
}

function ScrollAreaRootInner(
  props: ScrollAreaRootProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const {
    type = "auto",
    scrollHideDelay = 600,
    orientation = "vertical",
    className,
    children,
    style,
    ...rest
  } = props;

  // Expose the hide-delay as a CSS custom property so the stylesheet
  // can read it as a `transition-delay` value. The token-only rule
  // forbids raw `px` in stylesheets, but a NUMERIC PROP from a
  // consumer is allowed to land as a CSS custom property — the
  // stylesheet still references `var(--zs-scrollarea-hide-delay)`,
  // never a hardcoded `600ms`.
  const rootStyle = {
    ...(style as React.CSSProperties | undefined),
    "--zs-scrollarea-hide-delay": `${scrollHideDelay}ms`,
  } as React.CSSProperties;

  return (
    <ScrollAreaContext.Provider value={{ type, orientation, scrollHideDelay }}>
      <BaseScrollArea.Root
        {...rest}
        ref={ref}
        style={rootStyle}
        className={classnames("zs-scrollarea", className)}
        data-visibility={type}
        data-orientation={orientation}
      >
        {children}
      </BaseScrollArea.Root>
    </ScrollAreaContext.Provider>
  );
}

/* ─── ScrollArea.Viewport ───────────────────────────────────────────── */

type BaseViewportProps = ComponentPropsWithRef<typeof BaseScrollArea.Viewport>;

export interface ScrollAreaViewportProps
  extends Omit<BaseViewportProps, "render" | "className"> {
  /** Optional class hook on the Viewport. */
  className?: string;
  /** Viewport children — typically a `ScrollArea.Content` wrapper. */
  children?: ReactNode;
}

const ScrollAreaViewport = forwardRef<HTMLDivElement, ScrollAreaViewportProps>(
  function ScrollAreaViewport({ className, ...rest }, ref) {
    return (
      <BaseScrollArea.Viewport
        {...rest}
        ref={ref}
        className={classnames("zs-scrollarea__viewport", className)}
      />
    );
  },
);
(ScrollAreaViewport as { displayName?: string }).displayName =
  "ScrollArea.Viewport";

/* ─── ScrollArea.Content ────────────────────────────────────────────── */

type BaseContentProps = ComponentPropsWithRef<typeof BaseScrollArea.Content>;

export interface ScrollAreaContentProps
  extends Omit<BaseContentProps, "render" | "className"> {
  /** Optional class hook on the Content wrapper. */
  className?: string;
  /** Content children — the actual scrollable payload. */
  children?: ReactNode;
}

const ScrollAreaContent = forwardRef<HTMLDivElement, ScrollAreaContentProps>(
  function ScrollAreaContent({ className, ...rest }, ref) {
    return (
      <BaseScrollArea.Content
        {...rest}
        ref={ref}
        className={classnames("zs-scrollarea__content", className)}
      />
    );
  },
);
(ScrollAreaContent as { displayName?: string }).displayName =
  "ScrollArea.Content";

/* ─── ScrollArea.Scrollbar ──────────────────────────────────────────── */

type BaseScrollbarProps = ComponentPropsWithRef<
  typeof BaseScrollArea.Scrollbar
>;

export interface ScrollAreaScrollbarProps
  extends Omit<BaseScrollbarProps, "render" | "className"> {
  /**
   * Which axis this Scrollbar controls. `vertical` (default) renders a
   * tall bar pinned to the inline-end edge; `horizontal` renders a wide
   * bar pinned to the block-end edge. Logical-property cascade gives
   * the vertical bar the conventional RTL mirror automatically — the
   * `inset-inline-end` anchor resolves to the physical LEFT edge under
   * `dir="rtl"`, no explicit override required.
   *
   * @default "vertical"
   */
  orientation?: "vertical" | "horizontal";
  /**
   * Keep the Scrollbar element in the DOM even when the Viewport has
   * no overflow on this axis. Useful when consumers want to reserve
   * gutter space for the bar so layout doesn't shift as content grows.
   *
   * @default false
   */
  keepMounted?: boolean;
  /** Optional class hook on the bar track. */
  className?: string;
  /** Bar children — typically a single `ScrollArea.Thumb`. */
  children?: ReactNode;
}

const ScrollAreaScrollbar = forwardRef<
  HTMLDivElement,
  ScrollAreaScrollbarProps
>(function ScrollAreaScrollbar(
  { orientation = "vertical", className, ...rest },
  ref,
) {
  return (
    <BaseScrollArea.Scrollbar
      {...rest}
      ref={ref}
      orientation={orientation}
      className={classnames(
        "zs-scrollarea__scrollbar",
        `zs-scrollarea__scrollbar--${orientation}`,
        className,
      )}
      data-orientation={orientation}
    />
  );
});
(ScrollAreaScrollbar as { displayName?: string }).displayName =
  "ScrollArea.Scrollbar";

/* ─── ScrollArea.Thumb ──────────────────────────────────────────────── */

type BaseThumbProps = ComponentPropsWithRef<typeof BaseScrollArea.Thumb>;

export interface ScrollAreaThumbProps
  extends Omit<BaseThumbProps, "render" | "className"> {
  /** Optional class hook on the draggable thumb. */
  className?: string;
}

const ScrollAreaThumb = forwardRef<HTMLDivElement, ScrollAreaThumbProps>(
  function ScrollAreaThumb({ className, ...rest }, ref) {
    return (
      <BaseScrollArea.Thumb
        {...rest}
        ref={ref}
        className={classnames("zs-scrollarea__thumb", className)}
      />
    );
  },
);
(ScrollAreaThumb as { displayName?: string }).displayName = "ScrollArea.Thumb";

/* ─── ScrollArea.Corner ─────────────────────────────────────────────── */

type BaseCornerProps = ComponentPropsWithRef<typeof BaseScrollArea.Corner>;

export interface ScrollAreaCornerProps
  extends Omit<BaseCornerProps, "render" | "className"> {
  /** Optional class hook on the corner square. */
  className?: string;
}

const ScrollAreaCorner = forwardRef<HTMLDivElement, ScrollAreaCornerProps>(
  function ScrollAreaCorner({ className, ...rest }, ref) {
    return (
      <BaseScrollArea.Corner
        {...rest}
        ref={ref}
        className={classnames("zs-scrollarea__corner", className)}
      />
    );
  },
);
(ScrollAreaCorner as { displayName?: string }).displayName =
  "ScrollArea.Corner";

/* ─── Shorthand <ScrollArea> ───────────────────────────────────────── *
 *
 * The shorthand collapses the standard tree:
 *   Root + Viewport + Content + conditional Scrollbar(s) + Corner.
 *
 * `orientation` chooses which Scrollbars get rendered:
 *   - `vertical`  → just the vertical bar.
 *   - `horizontal` → just the horizontal bar.
 *   - `both`      → both bars + a Corner.
 *
 * Consumers who need finer control (e.g. inline custom Thumb content,
 * attaching an onScroll to the Viewport, splitting the shorthand to
 * inject toolbar chrome) use the compound API directly. */

export interface ScrollAreaProps
  extends Omit<
    ScrollAreaRootProps,
    "type" | "scrollHideDelay" | "orientation" | "className" | "children"
  > {
  /**
   * Visibility policy for the scrollbar chrome. See `ScrollAreaType`
   * for the four policies.
   *
   * @default "auto"
   */
  type?: ScrollAreaType;
  /**
   * Milliseconds the scrollbar stays visible after scrolling stops
   * before fading out. Only applies when `type="auto"`.
   *
   * @default 600
   */
  scrollHideDelay?: number;
  /**
   * Which axes the shorthand renders Scrollbars for. See
   * `ScrollAreaOrientation` for the three modes.
   *
   * @default "vertical"
   */
  orientation?: ScrollAreaOrientation;
  /** Optional class hook on the Root container. */
  className?: string;
  /** Content rendered inside the Viewport's Content wrapper. */
  children: ReactNode;
}

/* ─── Compose the public namespace ──────────────────────────────────── *
 *
 * Public API surface = the shorthand `<ScrollArea>` plus the namespaced
 * compound parts (`ScrollArea.Root`, `.Viewport`, `.Content`,
 * `.Scrollbar`, `.Thumb`, `.Corner`). No bare exports for the
 * subcomponents so consumers see one obvious path. */

const ForwardedScrollAreaRoot = forwardRef<HTMLDivElement, ScrollAreaRootProps>(
  ScrollAreaRootInner,
);
(ForwardedScrollAreaRoot as { displayName?: string }).displayName =
  "ScrollArea.Root";

function ScrollAreaShorthand(
  props: ScrollAreaProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const {
    type = "auto",
    scrollHideDelay = 600,
    orientation = "vertical",
    className,
    children,
    ...rest
  } = props;

  const showVertical = orientation === "vertical" || orientation === "both";
  const showHorizontal = orientation === "horizontal" || orientation === "both";
  const showCorner = orientation === "both";
  // For `always` and `hover` policies the bar's visibility is decoupled
  // from overflow — keep the element mounted so the chrome is stable.
  // For `auto` and `scroll`, only render the bar element once there is
  // real overflow on the axis (Base UI's default behaviour).
  const keepBarMounted = type === "always" || type === "hover";

  return (
    <ForwardedScrollAreaRoot
      {...rest}
      ref={ref}
      type={type}
      scrollHideDelay={scrollHideDelay}
      orientation={orientation}
      className={className}
    >
      <ScrollAreaViewport>
        <ScrollAreaContent>{children}</ScrollAreaContent>
      </ScrollAreaViewport>
      {showVertical ? (
        <ScrollAreaScrollbar
          orientation="vertical"
          keepMounted={keepBarMounted}
        >
          <ScrollAreaThumb />
        </ScrollAreaScrollbar>
      ) : null}
      {showHorizontal ? (
        <ScrollAreaScrollbar
          orientation="horizontal"
          keepMounted={keepBarMounted}
        >
          <ScrollAreaThumb />
        </ScrollAreaScrollbar>
      ) : null}
      {showCorner ? <ScrollAreaCorner /> : null}
    </ForwardedScrollAreaRoot>
  );
}

const ForwardedScrollArea = forwardRef<HTMLDivElement, ScrollAreaProps>(
  ScrollAreaShorthand,
) as React.ForwardRefExoticComponent<
  ScrollAreaProps & React.RefAttributes<HTMLDivElement>
> & {
  Root: typeof ForwardedScrollAreaRoot;
  Viewport: typeof ScrollAreaViewport;
  Content: typeof ScrollAreaContent;
  Scrollbar: typeof ScrollAreaScrollbar;
  Thumb: typeof ScrollAreaThumb;
  Corner: typeof ScrollAreaCorner;
};

ForwardedScrollArea.displayName = "ScrollArea";
ForwardedScrollArea.Root = ForwardedScrollAreaRoot;
ForwardedScrollArea.Viewport = ScrollAreaViewport;
ForwardedScrollArea.Content = ScrollAreaContent;
ForwardedScrollArea.Scrollbar = ScrollAreaScrollbar;
ForwardedScrollArea.Thumb = ScrollAreaThumb;
ForwardedScrollArea.Corner = ScrollAreaCorner;

export const ScrollArea = ForwardedScrollArea;
