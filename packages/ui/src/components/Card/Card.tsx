/*
 * Card — container surface. Decomposed subparts mirror the
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
 *     <Card.Content>…</Card.Content>
 *     <Card.Footer><Button>…</Button></Card.Footer>
 *   </Card>
 *
 * Aria semantics: Card subparts are layout-only — Header, Content,
 * Footer are <div>s with no role. The accessible heading hierarchy
 * comes from
 * Card.Title (an <h3> by default; `asChild` lets you swap to h2/h4 so
 * the level matches the surrounding document outline). This matches
 * shadcn/Chakra/Park UI and avoids problematic patterns like wrapping
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
 *
 * Subpart vocabulary: each subpart emits a `data-slot="card-<name>"`
 * attribute alongside its class. Aligns with the shadcn ecosystem
 * vocabulary and lets consumers target Card subparts by data-slot in
 * CSS without leaking our internal BEM class names. Visual-polish
 * item 9.
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
import { Slot } from "../_slot";
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

  /**
   * Card contents — typically a composition of `Card.Header`,
   * `Card.Media`, `Card.Content`, `Card.Footer`, etc. When `asChild` is
   * true, this MUST be a single React element (the render-as target).
   */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"card"`. A composing block
   * (e.g. `StatCard`) can override it so consumers can target the outer
   * element via the block's own slot vocabulary.
   */
  "data-slot"?: string;
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
    "data-slot": dataSlot = "card",
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

  // `data-slot` defaults to `"card"` but a composing block (e.g.
  // StatCard) can override the root slot so consumers can target the
  // outer element via its own slot vocabulary. Card's own stories pass
  // no `data-slot`, so the default `"card"` is unchanged.
  const dataProps = {
    "data-slot": dataSlot,
    "data-variant": variant,
    "data-size": size,
    "data-interactive": interactive ? "" : undefined,
  } as Record<string, string | undefined>;

  // `ownsActivation` is the precise predicate for "Card itself owns the
  // focusable/keyboard surface": interactive AND not asChild (asChild
  // delegates to the child's native semantics) AND a real `onClick`
  // handler is wired. Without `onClick`, applying `role="button"` +
  // `tabIndex=0` would ship a focusable fake control with no way to
  // activate it — a keyboard trap. The dev-mode console.warn at L176
  // surfaces this to consumers; here we degrade `interactive` to a
  // visual-only modifier (cursor, hover tint) instead.
  const ownsActivation =
    interactive && !asChild && typeof onClick === "function";

  // `aria-disabled` is observed via the rest spread so the CSS rule
  // .zs-card[data-interactive][aria-disabled="true"] (pointer-events
  // none, dimmed) is paired with JS-level suppression: pointer-events
  // blocks the mouse, but keyboard activation runs JS-side, so we must
  // also short-circuit `onClick` and the Enter/Space handler.
  const ariaDisabled =
    (rest as { "aria-disabled"?: boolean | "true" | "false" })["aria-disabled"];
  const isAriaDisabled = ariaDisabled === true || ariaDisabled === "true";

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
  // a "fake control" trap (review-fix items 1 + 20). When
  // `aria-disabled` is true we drop the activation entirely so keyboard
  // can't bypass the visual/pointer-events disabled state (wave-7 🔴 2).
  const handleKeyDown = ownsActivation
    ? (event: KeyboardEvent<HTMLDivElement>) => {
        if (typeof onKeyDown === "function") {
          onKeyDown(event);
          if (event.defaultPrevented) return;
        }
        if (isAriaDisabled) return;
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          // Synthesize a click — onClick is typed for the div, so the
          // KeyboardEvent stand-in is the closest thing to "the user
          // activated this control". React's synthetic-event base type
          // is compatible enough that the handler can read .currentTarget.
          onClick(event as unknown as MouseEvent<HTMLDivElement>);
        }
      }
    : onKeyDown;

  // Click suppression mirrors the keyboard path: aria-disabled cards
  // shouldn't activate from a click either. Pointer-events:none in CSS
  // already blocks most mouse clicks, but synthetic clicks (assistive
  // tech, programmatic .click()) bypass pointer-events and still reach
  // React's onClick — so guard at the handler level too.
  const handleClick = isAriaDisabled ? undefined : onClick;

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
        onClick={handleClick}
        onKeyDown={onKeyDown}
      >
        {children}
      </Slot>
    );
  }

  // Only own the focusable/keyboard surface when `ownsActivation` —
  // applying `role="button"` + `tabIndex=0` without an `onClick` would
  // create a focusable element with no activation path (wave-7 🔴 1).
  // Without `ownsActivation`, `interactive` degrades to a visual-only
  // hover/cursor modifier; the dev-mode warn at L176 tells consumers to
  // pass `onClick` or switch to `asChild` with a real link/button.
  const interactiveAriaProps = ownsActivation
    ? ({ role: "button" } as const)
    : undefined;

  return (
    <div
      {...rest}
      {...dataProps}
      {...interactiveAriaProps}
      ref={ref as Ref<HTMLDivElement>}
      className={composedClassName}
      tabIndex={ownsActivation ? 0 : undefined}
      onClick={handleClick}
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
export type CardContentProps = DivProps;
export type CardActionProps = DivProps;
export type CardDescriptionProps = ParagraphProps;

const CardHeader = forwardRef<HTMLDivElement, CardHeaderProps>(
  function CardHeader({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented `data-slot="card-header"` contract via `{...rest}`
    // (wave-7 🟢 6). ClassName stays composed via `classnames` so
    // consumer `className` augments rather than replaces the internal
    // class.
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="card-header"
        className={classnames("zs-card__header", className)}
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
      // Card root (review-fix item 8). Slot itself composes the child
      // ref via getElementRef internally, so we pass ONLY the
      // forwarded `ref` here — composing again would attach the child's
      // callback ref twice, firing twice per attach/detach
      // (wave-7 🟡 4).
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          data-slot="card-title"
          className={classnames("zs-card__title", className)}
        >
          {children}
        </Slot>
      );
    }
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented `data-slot="card-title"` contract (wave-7 🟢 6).
    return (
      <h3
        {...rest}
        ref={ref}
        data-slot="card-title"
        className={classnames("zs-card__title", className)}
      >
        {children}
      </h3>
    );
  },
);
CardTitle.displayName = "Card.Title";

const CardDescription = forwardRef<HTMLParagraphElement, CardDescriptionProps>(
  function CardDescription({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot (wave-7 🟢 6).
    return (
      <p
        {...rest}
        ref={ref}
        data-slot="card-description"
        className={classnames("zs-card__description", className)}
      />
    );
  },
);
CardDescription.displayName = "Card.Description";

const CardAction = forwardRef<HTMLDivElement, CardActionProps>(
  function CardAction({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot (wave-7 🟢 6).
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="card-action"
        className={classnames("zs-card__action", className)}
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
    // media override via the `rest` spread — JSX last-write-wins, so
    // putting `aria-hidden` BEFORE rest lets explicit
    // `aria-hidden={false}` from the consumer apply.
    //
    // `data-slot`/`data-side` are internal contract attrs — they sit
    // AFTER rest so callers cannot desync them via raw spread; the
    // documented surface for `data-side` is the `side` prop
    // (wave-7 🟢 6).
    const isDecorative = side === "fill";
    return (
      <div
        aria-hidden={isDecorative ? true : undefined}
        {...rest}
        ref={ref}
        data-slot="card-media"
        data-side={side}
        className={classnames("zs-card__media", className)}
      />
    );
  },
);
CardMedia.displayName = "Card.Media";

const CardContent = forwardRef<HTMLDivElement, CardContentProps>(
  function CardContent({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot (wave-7 🟢 6).
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="card-content"
        className={classnames("zs-card__content", className)}
      />
    );
  },
);
CardContent.displayName = "Card.Content";

export interface CardFooterProps extends DivProps {
  /** Justify-content of the button row. Default `end`. */
  align?: CardFooterAlign;
  /** Optional hairline divider — `"top"` adds a separator and extra
   *  top padding so the footer reads as a distinct band. */
  divider?: CardFooterDivider;
}

const CardFooter = forwardRef<HTMLDivElement, CardFooterProps>(
  function CardFooter({ align = "end", divider, className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot / data-align / data-divider
    // so callers cannot desync those contract attrs via raw spread —
    // the documented surface is the `align` and `divider` props
    // (wave-7 🟢 6).
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="card-footer"
        data-align={align}
        data-divider={divider}
        className={classnames("zs-card__footer", className)}
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
  Content: typeof CardContent;
  Footer: typeof CardFooter;
};

export const Card = CardRoot as CardComponent;
Card.Header = CardHeader;
Card.Title = CardTitle;
Card.Description = CardDescription;
Card.Action = CardAction;
Card.Media = CardMedia;
Card.Content = CardContent;
Card.Footer = CardFooter;
