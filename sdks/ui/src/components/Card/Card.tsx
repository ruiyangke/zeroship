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
 * or <button> when the whole-card-clickable pattern is genuinely
 * desired — at which point THE CONSUMER decides about nested
 * interactives. (We never auto-wrap.)
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
  cloneElement,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { Slot, composeRefs } from "../_slot";

export type CardVariant = "surface" | "elevated" | "outline" | "ghost";
export type CardSize = "sm" | "md" | "lg";
export type CardMediaSide = "top" | "bottom" | "left" | "right" | "fill";
export type CardFooterAlign = "start" | "between" | "end";

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
   * even when not rendered as a button/anchor. Does NOT add an
   * `onClick` handler — the consumer wires that themselves. When
   * `interactive` is true we set `tabIndex={0}` so the card is
   * focusable, and emit `data-interactive` so the CSS hooks light up.
   */
  interactive?: boolean;

  /**
   * Render-as the single child element rather than a `<div>`. Used
   * for the whole-card-clickable pattern with an <a> or <button>:
   *
   *   <Card asChild><a href="/post/42">…children…</a></Card>
   *
   * When `asChild` is set, the child element's `children` keep their
   * place inside the card; we don't swap them out. The CONSUMER
   * decides about nested interactives — we don't auto-wrap.
   */
  asChild?: boolean;

  children?: ReactNode;
}

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
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

  if (asChild) {
    if (!isValidElement(children)) return null;
    return (
      <Slot
        {...rest}
        {...dataProps}
        ref={ref as Ref<unknown>}
        className={composedClassName}
        tabIndex={interactive ? 0 : undefined}
      >
        {children}
      </Slot>
    );
  }

  return (
    <div
      {...rest}
      {...dataProps}
      ref={ref as Ref<HTMLDivElement>}
      className={composedClassName}
      tabIndex={interactive ? 0 : undefined}
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

const CardHeader = forwardRef<HTMLDivElement, DivProps>(
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
      if (!isValidElement(children)) return null;
      const child = children as ReactElement<{ className?: string }> & {
        ref?: Ref<unknown>;
      };
      return cloneElement(child, {
        ...rest,
        ref: composeRefs(ref as Ref<unknown>, child.ref),
        className: classnames(
          "zs-card__title",
          child.props.className,
          className,
        ),
      } as Record<string, unknown>);
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

const CardDescription = forwardRef<HTMLParagraphElement, ParagraphProps>(
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

const CardAction = forwardRef<HTMLDivElement, DivProps>(
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
    return (
      <div
        ref={ref}
        className={classnames("zs-card__media", className)}
        data-side={side}
        {...rest}
      />
    );
  },
);
CardMedia.displayName = "Card.Media";

const CardBody = forwardRef<HTMLDivElement, DivProps>(
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
}

const CardFooter = forwardRef<HTMLDivElement, CardFooterProps>(
  function CardFooter({ align = "end", className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-card__footer", className)}
        data-align={align}
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
