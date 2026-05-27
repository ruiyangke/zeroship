/*
 * Card — HIG "Boxes" container. Decomposed subparts mirror the
 * shadcn / Chakra / Park UI / Fluent convergence noted in the slice 3
 * library-survey report. Monolithic Cards (Radix Themes, Mantine,
 * Geist) inevitably need escape hatches anyway, so we decompose from
 * the start:
 *
 *   <Card>
 *     <Card.Header>
 *       <Card.Title>Title</Card.Title>
 *       <Card.Description>Subtitle</Card.Description>
 *       <Card.Action><Button>…</Button></Card.Action>
 *     </Card.Header>
 *     <Card.Body>…</Card.Body>
 *     <Card.Footer><Button>…</Button></Card.Footer>
 *   </Card>
 *
 * Aria semantics: Card subparts are layout-only — Header, Body, Footer
 * are <div>s with no role. The accessible heading hierarchy comes from
 * Card.Title (an <h3> by default; `asChild` lets you swap to h2/h4 so
 * the level matches the surrounding document outline). This matches
 * shadcn/Chakra/Park UI and avoids HIG-violation patterns like wrapping
 * the entire card in an <a> (anti-pattern #2 from the survey).
 *
 * The Card root accepts `asChild` so the consumer can render-as an <a>
 * when the whole-card-clickable pattern is genuinely desired — at
 * which point THE CONSUMER decides about nested interactives. (We
 * never auto-wrap.)
 *
 * Render-as targets: prefer an `<a href>` for whole-card-clickable.
 * `<button>` is INVALID HTML when the card contains block-level
 * descendants (Header/Title/Body emit div/h3/p), so we dev-warn when
 * an asChild button is detected (item 6 from the slice-3 review-fix
 * brief).
 *
 * Anti-patterns we explicitly avoid (15-item survey list, items 1-15 in
 * the brief):
 *  - 1: NO `::before` / `::after` overlays on the card surface
 *    (axe-glass rule — pseudos break the color-contrast walk).
 *  - 2: NO wrapping the card root in an <a> by default; `asChild` is
 *    opt-in.
 *  - 14: Card padding ALWAYS comes from --zs-card-padding-* tokens.
 *  - 15: Card.Media side="top" pulls a negative margin out to the
 *    card edge; the Card root has `overflow: hidden` so the media
 *    clips to the card's border-radius (no negative-margin overflow
 *    trick without clip).
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type Ref,
} from "react";
import { Slot, composeRefs, getElementRef } from "../_slot";
import { classnames } from "../_classnames";

export type CardVariant = "surface" | "elevated" | "outline" | "ghost";
export type CardSize = "sm" | "md" | "lg";
/**
 * Edge-bleed Media slot position. `"top"` and `"bottom"` pull a
 * negative margin out to the card edge; `"fill"` is an absolutely
 * positioned decorative background layer (defaults to
 * `aria-hidden="true"` — item 16).
 *
 * `"left"` and `"right"` were exposed in the original surface but
 * never implemented (the Card root is flex-column, so margin-inline
 * couldn't produce a horizontal layout). Removed pre-launch (item 7
 * from the slice-3 review-fix brief). A real horizontal Media layout
 * needs a row-orientation Card variant that the grid-based root can
 * support; deferred to a future slice.
 */
export type CardMediaSide = "top" | "bottom" | "fill";
export type CardFooterAlign = "start" | "between" | "end";
/**
 * Optional divider modifier for Footer.
 * - `"top"`: render a hairline separator above the footer with extra
 *   top padding so the button row reads as a distinct affordance band.
 */
export type CardFooterDivider = "top";

export interface CardProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Visual style.
   * - `surface`: opaque background; no border; no shadow. Default.
   * - `elevated`: opaque + small layered shadow (--zs-shadow-2).
   * - `outline`: opaque + 1px inset separator.
   * - `ghost`: TRANSPARENT — the only variant that opts out of the
   *   opaque-base rule. Meant for nesting inside an already-opaque
   *   surface (e.g., grouping rows inside a surface Card). Don't use
   *   ghost as a top-level card; axe's color-contrast walk will
   *   report incompletes against the page background.
   */
  variant?: CardVariant;

  /** Sizing — sm / md / lg. Default `md`. */
  size?: CardSize;

  /**
   * Apply interactive states (hover, focus-visible ring, active press)
   * even when not rendered as a button/anchor.
   *
   * When `interactive` is true AND `asChild` is false, the Card:
   *   - sets `tabIndex={0}` so it is focusable,
   *   - sets `role="button"` so AT announces it as a control,
   *   - wires `onKeyDown` to forward Enter/Space to `onClick`.
   *
   * If the consumer didn't pass `onClick`, `interactive` becomes a
   * no-op visual modifier and a dev-mode console.warn fires —
   * keyboard users can't activate a "fake control". For
   * whole-card-clickable, prefer `asChild` with a real anchor.
   */
  interactive?: boolean;

  /**
   * Render-as the single child element rather than a `<div>`. Used
   * for the whole-card-clickable pattern with an `<a>`:
   *
   *   <Card asChild><a href="/post/42">…children…</a></Card>
   *
   * Prefer an anchor — `<button>` is INVALID HTML when Card subparts
   * render block content. The asChild child's children keep their
   * place inside the card; we don't swap them out. The CONSUMER
   * decides about nested interactives — we don't auto-wrap.
   */
  asChild?: boolean;

  children?: ReactNode;
}

/* ─── Card root ──────────────────────────────────────────────────────── */

const CardRoot = forwardRef<HTMLElement, CardProps>(function CardRoot(
  {
    variant = "surface",
    size = "md",
    interactive = false,
    asChild = false,
    className,
    children,
    onClick,
    onKeyDown,
    ...rest
  },
  ref,
) {
  const composedClassName = classnames(
    "zs-card",
    `zs-card--${variant}`,
    size !== "md" ? `zs-card--${size}` : null,
    className,
  );

  const dataProps = {
    "data-variant": variant,
    "data-size": size,
    "data-interactive": interactive ? "" : undefined,
  } as Record<string, string | undefined>;

  // Dev-mode validation surfaces guidance the AI agent / consumer can
  // act on. Gated to non-production via `process.env.NODE_ENV` —
  // bundlers (Vite, Webpack, Rollup, tsup-via-downstream) replace this
  // identifier so the whole branch DCEs out of production builds.
  // Slice-3 review-fix items 1 + 6.
  if (process.env.NODE_ENV !== "production") {
    if (interactive && !asChild && typeof onClick !== "function") {
      // eslint-disable-next-line no-console
      console.warn(
        "Card interactive=true but no onClick handler; keyboard users can't activate it. " +
          "Pass onClick, or use asChild with a real link/button if you don't want onClick.",
      );
    }
    if (asChild && isValidElement(children)) {
      const childType = (children as { type?: unknown }).type;
      if (childType === "button") {
        // eslint-disable-next-line no-console
        console.warn(
          "Card asChild=<button> is invalid HTML because Card subparts render block content " +
            "(Header/Title/Body emit div/h3/p). Use an <a href> for whole-card-clickable, " +
            "or place a Button outside the card.",
        );
      }
    }
  }

  // Keyboard activation: when we own a non-native interactive div, Enter
  // and Space MUST fire onClick — otherwise tabIndex=0 + focus ring is
  // a "fake control" trap (review-fix items 1 + 20).
  const handleKeyDown =
    interactive && !asChild && typeof onClick === "function"
      ? (event: KeyboardEvent<HTMLDivElement>) => {
          if (typeof onKeyDown === "function") {
            onKeyDown(event);
            if (event.defaultPrevented) return;
          }
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            // Synthesize a click — onClick is typed for the div, so the
            // KeyboardEvent stand-in is the closest thing to "the user
            // activated this control". React's synthetic-event base type
            // is compatible enough that the handler can read .currentTarget.
            onClick(
              event as unknown as MouseEvent<HTMLDivElement>,
            );
          }
        }
      : onKeyDown;

  if (asChild) {
    if (!isValidElement(children)) {
      if (process.env.NODE_ENV !== "production") {
        console.error(
          "Card asChild expects a single React element child; received " +
            typeof children +
            "; rendering nothing.",
        );
      }
      return null;
    }
    // The child element brings its own focusability semantics: anchors
    // via href, buttons inherently. We DO NOT inject tabIndex here —
    // if the consumer asChilds a non-focusable element, that's their
    // bug to fix (review-fix item 10).
    return (
      <Slot
        {...rest}
        {...dataProps}
        ref={ref as Ref<unknown>}
        className={composedClassName}
        onClick={onClick}
        onKeyDown={onKeyDown}
      >
        {children}
      </Slot>
    );
  }

  const interactiveAriaProps = interactive
    ? ({ role: "button" } as const)
    : undefined;

  return (
    <div
      {...rest}
      {...dataProps}
      {...interactiveAriaProps}
      ref={ref as Ref<HTMLDivElement>}
      className={composedClassName}
      tabIndex={interactive ? 0 : undefined}
      onClick={onClick}
      onKeyDown={handleKeyDown}
    >
      {children}
    </div>
  );
});
CardRoot.displayName = "Card";

/* ─── Subparts ───────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;
type HeadingProps = ComponentPropsWithoutRef<"h3">;
type ParagraphProps = ComponentPropsWithoutRef<"p">;

export type CardHeaderProps = DivProps;
export type CardBodyProps = DivProps;
export type CardActionProps = DivProps;
export type CardDescriptionProps = ParagraphProps;

const CardHeader = forwardRef<HTMLDivElement, CardHeaderProps>(
  function CardHeader({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-card__header", className)}
        {...rest}
      />
    );
  },
);
CardHeader.displayName = "Card.Header";

export interface CardTitleProps extends HeadingProps {
  /** Render-as the single child element (e.g., to swap the heading
   *  level so it matches the surrounding outline). */
  asChild?: boolean;
}

const CardTitle = forwardRef<HTMLHeadingElement, CardTitleProps>(
  function CardTitle({ asChild = false, className, children, ...rest }, ref) {
    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          console.error(
            "Card.Title asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      // Route asChild through Slot so className composition, style
      // shallow-merge, event composition with defaultPrevented short-
      // circuit, and React-19 ref access all behave identically to
      // Card root (review-fix item 8). composeRefs is wired into Slot;
      // we still pass our ref so a parent forwarding into Card.Title
      // lands at the rendered element.
      return (
        <Slot
          {...rest}
          ref={composeRefs(ref as Ref<unknown>, getElementRef(children))}
          className={classnames("zs-card__title", className)}
        >
          {children}
        </Slot>
      );
    }
    return (
      <h3
        ref={ref}
        className={classnames("zs-card__title", className)}
        {...rest}
      >
        {children}
      </h3>
    );
  },
);
CardTitle.displayName = "Card.Title";

const CardDescription = forwardRef<HTMLParagraphElement, CardDescriptionProps>(
  function CardDescription({ className, ...rest }, ref) {
    return (
      <p
        ref={ref}
        className={classnames("zs-card__description", className)}
        {...rest}
      />
    );
  },
);
CardDescription.displayName = "Card.Description";

const CardAction = forwardRef<HTMLDivElement, CardActionProps>(
  function CardAction({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-card__action", className)}
        {...rest}
      />
    );
  },
);
CardAction.displayName = "Card.Action";

export interface CardMediaProps extends DivProps {
  /** Edge-bleed media slot position. Default `top`. */
  side?: CardMediaSide;
}

const CardMedia = forwardRef<HTMLDivElement, CardMediaProps>(
  function CardMedia({ side = "top", className, ...rest }, ref) {
    // `side="fill"` is a decorative background layer — default it to
    // aria-hidden so AT doesn't double-announce the card surface
    // (review-fix item 16). Consumers wanting a meaningful fill-mode
    // media override via the `rest` spread (which runs last, so
    // {...rest} wins over the hard-coded value here? No — JSX spread
    // semantics are last-write-wins; we put the spread LAST below so
    // explicit aria-hidden={false} from the consumer applies).
    const isDecorative = side === "fill";
    return (
      <div
        ref={ref}
        aria-hidden={isDecorative ? true : undefined}
        {...rest}
        className={classnames("zs-card__media", className)}
        data-side={side}
      />
    );
  },
);
CardMedia.displayName = "Card.Media";

const CardBody = forwardRef<HTMLDivElement, CardBodyProps>(
  function CardBody({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-card__body", className)}
        {...rest}
      />
    );
  },
);
CardBody.displayName = "Card.Body";

export interface CardFooterProps extends DivProps {
  /** Justify-content of the button row. Default `end` (HIG-standard). */
  align?: CardFooterAlign;
  /** Optional hairline divider — `"top"` adds a separator and extra
   *  top padding so the footer reads as a distinct band. */
  divider?: CardFooterDivider;
}

const CardFooter = forwardRef<HTMLDivElement, CardFooterProps>(
  function CardFooter({ align = "end", divider, className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-card__footer", className)}
        data-align={align}
        data-divider={divider}
        {...rest}
      />
    );
  },
);
CardFooter.displayName = "Card.Footer";

/* ─── public Card namespace ──────────────────────────────────────────── */

type CardComponent = typeof CardRoot & {
  Header: typeof CardHeader;
  Title: typeof CardTitle;
  Description: typeof CardDescription;
  Action: typeof CardAction;
  Media: typeof CardMedia;
  Body: typeof CardBody;
  Footer: typeof CardFooter;
};

export const Card = CardRoot as CardComponent;
Card.Header = CardHeader;
Card.Title = CardTitle;
Card.Description = CardDescription;
Card.Action = CardAction;
Card.Media = CardMedia;
Card.Body = CardBody;
Card.Footer = CardFooter;
