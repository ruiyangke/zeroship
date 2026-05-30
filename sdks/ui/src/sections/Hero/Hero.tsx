/*
 * Hero — the top-of-page marketing band.
 *
 * The flagship page section: an eyebrow label, a headline, a supporting
 * description, a row of call-to-action buttons, and (optionally) a media
 * column. It is the first piece of the `sections/` layer — page-level
 * bands that compose the layout primitives + styled components into a
 * marketing/content surface.
 *
 * Like Card / EmptyState, Hero exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case in one element:
 *        <Hero
 *          eyebrow={<Badge>New</Badge>}
 *          title="Ship software without writing code"
 *          description="Describe what you want. AI builds it."
 *          actions={
 *            <>
 *              <Button>Get started</Button>
 *              <Button variant="gray">Learn more</Button>
 *            </>
 *          }
 *        />
 *
 *   2. Compound parts — full control over composition / ordering:
 *        <Hero>
 *          <Hero.Eyebrow><Badge>New</Badge></Hero.Eyebrow>
 *          <Hero.Title>Ship software without writing code</Hero.Title>
 *          <Hero.Description>Describe what you want.</Hero.Description>
 *          <Hero.Actions><Button>Get started</Button></Hero.Actions>
 *          <Hero.Media><img … /></Hero.Media>
 *        </Hero>
 *
 * The two surfaces are ADDITIVE — there is no suppression. Supplying both
 * a `title` prop AND a `<Hero.Title>` child renders TWO headings (only the
 * primary carries the section label id — see a11y). Every media source is
 * collected additively too: the `media` prop and EVERY `<Hero.Media>` child
 * land in the media column, in document order, none stranded in the text
 * flow. Compound parts may be Fragment-wrapped (`<><Hero.Media/></>`); the
 * root descends Fragments when normalizing. Pick one mode per band.
 *
 * Layout (dogfoods Wave-1 primitives — never re-rolls flexbox/grid):
 *   - The root is a `<section data-slot="hero">` with generous vertical
 *     padding; it paints NO heavy background by default so it drops into
 *     a page composably (the consumer supplies the page backdrop).
 *   - Inside sits a `Container` (the single width authority) holding the
 *     band body.
 *   - The text column is a vertical `Stack`: eyebrow → headline →
 *     description → actions, gap-governed.
 *   - `align="center"` (default) centers the text column on a readable
 *     max-width measure; `align="start"` start-aligns it.
 *   - When `media` (prop) or a `<Hero.Media>` child is present, the body
 *     becomes a two-column split (text + media); it collapses to a single
 *     stacked column below `--zs-bp-md`. Without media it is a single
 *     column. The two modes are driven by `data-layout` on the body.
 *
 * a11y:
 *   - The root `<section>` is `aria-labelledby` the headline. The ROOT owns
 *     the label id: it mints a stable `useId` and assigns it to exactly ONE
 *     primary headline — the `title` prop if present, else the first
 *     `<Hero.Title>` child (in document order, Fragments descended). The
 *     section is always labelled by that real primary heading, regardless of
 *     authoring mode, and never by a duplicate. A `<Hero.Title>` that is NOT
 *     the primary keeps its own minted id. With no headline at all the root
 *     omits `aria-labelledby` rather than emitting a dangling reference.
 *   - The headline renders as an `<h1>` by default (the hero is typically
 *     the page's primary heading); `asChild` on `Hero.Title` relevels it
 *     (e.g. to `<h2>`) so it matches the surrounding document outline.
 *   - The media wrapper is decorative (`aria-hidden`) by default — the
 *     headline carries the meaning. A consumer who passes a labelled,
 *     meaningful image opts back in with `aria-hidden={false}`.
 *   - actions are real `<Button>`s; the eyebrow may be a `Badge` or plain
 *     text. forced-colors keeps every text region on `CanvasText`.
 *
 * `data-slot` vocabulary mirrors Card / EmptyState so consumers can target
 * regions in CSS without leaning on the internal BEM class names:
 *   hero / hero-body / hero-text / hero-eyebrow / hero-title /
 *   hero-description / hero-actions / hero-media-column / hero-media.
 *
 * `hero-media-column` is the grid track that holds the media; every media
 * source (the `media` prop and each `<Hero.Media>` child) renders inside it
 * as a `hero-media` wrapper, stacked in document order.
 */
import {
  Children,
  cloneElement,
  createContext,
  Fragment,
  forwardRef,
  isValidElement,
  useContext,
  useId,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Stack } from "../../layouts/Stack";
import { Cluster } from "../../layouts/Cluster";
import type { SectionTone } from "../_tone";

/** Text alignment / column treatment for the hero band. */
export type HeroAlign = "center" | "start";

/* ─── context ─────────────────────────────────────────────────────────────
 *
 * Carries only the resolved `align` so compound parts (Actions) mirror the
 * band alignment. The heading label id is NOT shared here: the root owns it
 * and injects it onto the single primary headline directly (the `title` prop
 * via `<HeroTitle id={titleId}>`, or the first `<Hero.Title>` child via
 * `cloneElement`). A `Hero.Title` therefore resolves its own id
 * (`id ?? useId()`) and never reads a shared id from context — so two Titles
 * can never collide on the section label id. */
interface HeroContextValue {
  /**
   * Effective text-column alignment so parts mirror the column. This is
   * the band `align` collapsed to `start` whenever a media split is
   * active (a split balances the two columns, so the text column and its
   * CTA row read start-aligned regardless of the requested `align`).
   */
  align: HeroAlign;
}

const HeroContext = createContext<HeroContextValue | null>(null);

function useHeroContext(part: string): HeroContextValue {
  const ctx = useContext(HeroContext);
  if (ctx == null && process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      `Hero.${part} must be rendered inside <Hero>; it relies on the band ` +
        "context for its alignment.",
    );
  }
  return ctx ?? { align: "center" };
}

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><Hero.Media/></>`) are visible to the normalizer.
 * Preserves document order and keys. */
function flattenChildren(children: ReactNode): ReactNode[] {
  const out: ReactNode[] = [];
  Children.forEach(children, (child) => {
    if (isValidElement(child) && child.type === Fragment) {
      out.push(
        ...flattenChildren((child.props as { children?: ReactNode }).children),
      );
    } else {
      out.push(child);
    }
  });
  return out;
}

/* ─── props ─────────────────────────────────────────────────────────────── */

export interface HeroProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /**
   * Small label above the headline — a `Badge`, a `Tag`, or plain text.
   * Wrapped in a `Hero.Eyebrow` region. Ergonomic mode; for full control
   * use the compound `<Hero.Eyebrow>` part instead.
   */
  eyebrow?: ReactNode;

  /**
   * The headline (ergonomic mode). Renders as the band's `<h1>` (the
   * page's primary heading) and is the target of the section's
   * `aria-labelledby`. Optional because the headline may instead be
   * supplied via the compound `<Hero.Title>` part (e.g. to relevel it with
   * `<Hero.Title asChild>`). Use ONE mode: there is no suppression, so
   * passing both this prop AND a `<Hero.Title>` child renders two headings.
   * Every Hero needs exactly one headline — the section's `aria-labelledby`
   * points at it; omitting both (dev-warns) leaves a dangling label.
   */
  title?: ReactNode;

  /**
   * Supporting paragraph beneath the headline. Rendered as a muted `<p>`
   * on a comfortable reading measure.
   */
  description?: ReactNode;

  /**
   * Call-to-action row — typically a primary + secondary `<Button>`.
   * Rendered as a `Cluster` so the buttons wrap gracefully on narrow
   * widths. Aligned to match the band (`center` → centered, `start` →
   * start).
   */
  actions?: ReactNode;

  /**
   * Optional media (image / illustration / product shot). When set, the
   * band becomes a two-column split (text + media) that collapses to a
   * stacked single column below `--zs-bp-md`. Without media the band is a
   * single column. The wrapper is decorative (`aria-hidden`) by default;
   * pass a labelled image and `aria-hidden={false}` on a `<Hero.Media>`
   * part for a meaningful image.
   */
  media?: ReactNode;

  /**
   * Text alignment / column treatment.
   * - `center` (default): the text column is centered on a readable
   *   max-width measure. With media, the text column itself stays
   *   start-read but the split balances the two columns.
   * - `start`: the text column start-aligns.
   *
   * When `media` is present the layout is always a split; `align` then
   * governs the text column's internal alignment.
   */
  align?: HeroAlign;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `lg`. The hero is the only place a section sets its own
   * Container size; everything else inherits the page width.
   */
  size?: ContainerSize;

  /**
   * Full-bleed band tone — the shared page-rhythm system. The root stamps
   * `data-tone`; the band treatment lives in `sections/_section-tone.css`.
   * - `default` (default): transparent; inherits the page backdrop.
   * - `muted`: a subtle full-bleed surface fill so the band reads as its own
   *   panel.
   * - `accent`: an `--zs-accent` fill with the inner ink remapped to
   *   `--zs-accent-ink` — the bold contrast band.
   */
  tone?: SectionTone;

  /** Band contents — compound parts and/or arbitrary children. */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"hero"`. A composing section can
   * override it so consumers target the outer element via its own slot
   * vocabulary. Mirrors Card / Container.
   */
  "data-slot"?: string;
}

/* ─── Hero root ─────────────────────────────────────────────────────────── */

const HeroRoot = forwardRef<HTMLElement, HeroProps>(function HeroRoot(
  {
    eyebrow,
    title,
    description,
    actions,
    media,
    align = "center",
    size = "lg",
    tone = "default",
    className,
    children,
    "data-slot": dataSlot = "hero",
    ...rest
  },
  ref,
) {
  // Stable id the section uses for aria-labelledby. The ROOT owns it and
  // injects it onto exactly one primary headline (see below); it is NOT
  // shared via context. useId is SSR-safe and collision-free across multiple
  // Heroes on a page.
  const titleId = useId();

  const composedClassName = classnames("zs-hero", className);

  // ── Recursive child normalization (single pass) ──────────────────────────
  // One walk over the flattened children (Fragments descended) does all the
  // dual-surface bookkeeping:
  //   - lift EVERY `<Hero.Media>` into the media column (additive — none are
  //     stranded in the text flow); a media column can't be produced by CSS
  //     alone, so each must be hoisted out of the text Stack;
  //   - designate the PRIMARY compound headline: the FIRST `<Hero.Title>`
  //     child, but only when there is no `title` prop (the prop wins by
  //     precedence). The root injects the section label id (`titleId`) onto
  //     that one element via cloneElement — overriding any custom id so the
  //     section is always labelled by the real primary headline (R3/R4);
  //   - keep every other child in document order for the text column.
  // The band becomes a two-column split whenever media is supplied in either
  // surface (the `media` prop OR any `<Hero.Media>` child).
  const liftedMedia: ReactElement[] = [];
  const restChildren: ReactNode[] = [];
  let sawTitleChild = false;
  let primaryTitleHadCustomId = false;
  flattenChildren(children).forEach((child, index) => {
    if (isValidElement(child)) {
      const childType = (child as ReactElement).type;
      if (childType === HeroMedia) {
        // Collect ALL media, keyed by flatten order so the reordered media
        // column never triggers a React key warning.
        liftedMedia.push(
          cloneElement(child as ReactElement, {
            key: (child as ReactElement).key ?? `hero-media-${index}`,
          }),
        );
        return;
      }
      if (childType === HeroTitle) {
        // The first `<Hero.Title>` child is the primary headline only when no
        // `title` prop is present; the root injects the label id onto it.
        // Non-primary titles render unchanged (their own minted id).
        if (!sawTitleChild && title == null) {
          const titleEl = child as ReactElement<{ id?: string }>;
          primaryTitleHadCustomId = titleEl.props.id != null;
          restChildren.push(cloneElement(titleEl, { id: titleId }));
          sawTitleChild = true;
          return;
        }
        sawTitleChild = true;
      }
    }
    restChildren.push(child);
  });

  // The primary headline owns the section label id, by precedence: the
  // `title` prop, else the first `<Hero.Title>` child.
  const hasHeadline = title != null || sawTitleChild;

  if (process.env.NODE_ENV !== "production") {
    // Every hero needs exactly one headline: the section's aria-labelledby
    // points at it. Warn when NEITHER the `title` prop NOR a compound
    // `<Hero.Title>` child is supplied — otherwise (R6) we omit the label
    // rather than dangle it.
    if (!hasHeadline) {
      // eslint-disable-next-line no-console
      console.warn(
        "Hero has no headline: pass the `title` prop or a <Hero.Title> child. " +
          "The section's aria-labelledby points at the headline; without one " +
          "the section is rendered without a label.",
      );
    }
    // R4: a custom id on the PRIMARY title is overridden with the section's
    // label id — the root owns it. Warn so the conflict is visible.
    if (primaryTitleHadCustomId) {
      // eslint-disable-next-line no-console
      console.warn(
        "Hero manages the heading id for the section label; the custom id on " +
          "the primary <Hero.Title> was ignored. Drop it, or use the `title` " +
          "prop. A custom id on a non-primary <Hero.Title> is honored.",
      );
    }
  }

  const mediaNodes: ReactNode[] = [];
  if (media != null) {
    mediaNodes.push(<HeroMedia key="hero-media-prop">{media}</HeroMedia>);
  }
  mediaNodes.push(...liftedMedia);
  const hasMedia = mediaNodes.length > 0;
  const dataLayout = hasMedia ? "split" : "single";
  // The text column / CTA row start-align under a media split (the split
  // already balances the two columns); otherwise they follow `align`.
  const effectiveAlign: HeroAlign = hasMedia ? "start" : align;

  // The text column: eyebrow → headline → description → actions. A vertical
  // Stack governs the gap; the `data-slot="hero-text"` relabels the Stack
  // so consumers target the column. Ergonomic-prop content renders first,
  // then any remaining compound children fall through after it inside the
  // same Stack. With a media split the text column always start-aligns
  // (the split balances the two columns); without media, `align="center"`
  // centers it.
  const textColumn = (
    <Stack
      data-slot="hero-text"
      className="zs-hero__text"
      gap={5}
      align={effectiveAlign === "center" ? "center" : "start"}
    >
      {eyebrow != null ? <HeroEyebrow>{eyebrow}</HeroEyebrow> : null}
      {title != null ? <HeroTitle id={titleId}>{title}</HeroTitle> : null}
      {description != null ? (
        <HeroDescription>{description}</HeroDescription>
      ) : null}
      {actions != null ? <HeroActions>{actions}</HeroActions> : null}
      {restChildren}
    </Stack>
  );

  const body = (
    <div
      data-slot="hero-body"
      data-layout={dataLayout}
      className="zs-hero__body"
    >
      {textColumn}
      {hasMedia ? (
        <div
          data-slot="hero-media-column"
          className="zs-hero__media-column"
        >
          {mediaNodes}
        </div>
      ) : null}
    </div>
  );

  return (
    <HeroContext.Provider value={{ align: effectiveAlign }}>
      <section
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot={dataSlot}
        data-align={align}
        data-layout={dataLayout}
        data-tone={tone}
        aria-labelledby={hasHeadline ? titleId : undefined}
        className={composedClassName}
      >
        <Container size={size} data-slot="hero-container">
          {body}
        </Container>
      </section>
    </HeroContext.Provider>
  );
});
HeroRoot.displayName = "Hero";

/* ─── Eyebrow — small label above the headline ───────────────────────────── */

export type HeroEyebrowProps = ComponentPropsWithoutRef<"div">;

const HeroEyebrow = forwardRef<HTMLDivElement, HeroEyebrowProps>(
  function HeroEyebrow({ className, ...rest }, ref) {
    // Rest spread BEFORE the internal data-slot so callers cannot overwrite
    // the documented contract attr via `{...rest}`.
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="hero-eyebrow"
        className={classnames("zs-hero__eyebrow", className)}
      />
    );
  },
);
HeroEyebrow.displayName = "Hero.Eyebrow";

/* ─── Title — <h1> by default; asChild relevels; root owns the label id ──── */

export interface HeroTitleProps extends ComponentPropsWithoutRef<"h1"> {
  /**
   * Render-as the single child element rather than an `<h1>` — use to
   * relevel the heading (e.g. `<h2>`) so it matches the surrounding
   * document outline. Routed through `Slot` (React-19-safe refs).
   */
  asChild?: boolean;
}

const HeroTitle = forwardRef<HTMLHeadingElement, HeroTitleProps>(
  function HeroTitle({ asChild = false, className, children, id, ...rest }, ref) {
    // Read context only to dev-warn when rendered outside <Hero>.
    useHeroContext("Title");
    // The Title owns its own id. When this Title is the section's PRIMARY
    // headline the root injects `id={titleId}` via cloneElement (so this prop
    // already carries the section label id); otherwise this is a non-primary
    // title and its own minted `useId` keeps it unique. The Title NEVER reads
    // a shared label id from context — that is what made two Titles collide.
    const fallbackId = useId();
    const resolvedId = id ?? fallbackId;
    const composedClassName = classnames("zs-hero__title", className);

    if (asChild) {
      // A Fragment passes `isValidElement` but cannot carry the id / className
      // / heading semantics — Slot would clone it and the label would dangle.
      // Reject it the same way as a non-element child.
      if (!isValidElement(children) || children.type === Fragment) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.warn(
            "Hero.Title asChild expects a single concrete element child (not a " +
              "Fragment); received " +
              (isValidElement(children) ? "a Fragment" : typeof children) +
              "; rendering nothing.",
          );
        }
        return null;
      }
      return (
        <Slot
          {...rest}
          id={resolvedId}
          ref={ref as Ref<unknown>}
          data-slot="hero-title"
          className={composedClassName}
        >
          {children}
        </Slot>
      );
    }
    return (
      <h1
        {...rest}
        id={resolvedId}
        ref={ref}
        data-slot="hero-title"
        className={composedClassName}
      >
        {children}
      </h1>
    );
  },
);
HeroTitle.displayName = "Hero.Title";

/* ─── Description — muted <p> ─────────────────────────────────────────────── */

export type HeroDescriptionProps = ComponentPropsWithoutRef<"p">;

const HeroDescription = forwardRef<HTMLParagraphElement, HeroDescriptionProps>(
  function HeroDescription({ className, ...rest }, ref) {
    return (
      <p
        {...rest}
        ref={ref}
        data-slot="hero-description"
        className={classnames("zs-hero__description", className)}
      />
    );
  },
);
HeroDescription.displayName = "Hero.Description";

/* ─── Actions — a Cluster of CTA buttons, aligned to the band ─────────────── */

export type HeroActionsProps = ComponentPropsWithoutRef<"div">;

const HeroActions = forwardRef<HTMLDivElement, HeroActionsProps>(
  function HeroActions({ className, children, ...rest }, ref) {
    const { align } = useHeroContext("Actions");
    // Compose the Cluster primitive (wrapping inline group). It mirrors the
    // band alignment so a centered hero has centered CTAs. Cluster honors a
    // consumer data-slot (defaulting to "cluster"); we relabel it to the
    // band's own vocabulary and compose the BEM hook via classnames.
    return (
      <Cluster
        {...rest}
        ref={ref}
        gap={3}
        justify={align === "center" ? "center" : "start"}
        data-slot="hero-actions"
        className={classnames("zs-hero__actions", className)}
      >
        {children}
      </Cluster>
    );
  },
);
HeroActions.displayName = "Hero.Actions";

/* ─── Media — optional, decorative by default ─────────────────────────────── */

export type HeroMediaProps = ComponentPropsWithoutRef<"div">;

const HeroMedia = forwardRef<HTMLDivElement, HeroMediaProps>(
  function HeroMedia({ className, ...rest }, ref) {
    // Decorative by default — the headline carries the meaning, so a
    // product shot / illustration shouldn't double-announce. `aria-hidden`
    // sits BEFORE rest so a consumer with a meaningful, labelled image opts
    // back in with `aria-hidden={false}`.
    return (
      <div
        aria-hidden="true"
        {...rest}
        ref={ref}
        data-slot="hero-media"
        className={classnames("zs-hero__media", className)}
      />
    );
  },
);
HeroMedia.displayName = "Hero.Media";

/* ─── public Hero namespace ──────────────────────────────────────────────── */

type HeroComponent = typeof HeroRoot & {
  Eyebrow: typeof HeroEyebrow;
  Title: typeof HeroTitle;
  Description: typeof HeroDescription;
  Actions: typeof HeroActions;
  Media: typeof HeroMedia;
};

export const Hero = HeroRoot as HeroComponent;
Hero.Eyebrow = HeroEyebrow;
Hero.Title = HeroTitle;
Hero.Description = HeroDescription;
Hero.Actions = HeroActions;
Hero.Media = HeroMedia;
