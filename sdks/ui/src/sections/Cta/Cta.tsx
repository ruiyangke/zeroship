/*
 * Cta — a focused call-to-action band.
 *
 * A single conversion moment: an optional eyebrow label, an `<h2>` title, a
 * short supporting description, and a row of action `<Button>`s. The fourth
 * piece of the `sections/` layer (after Hero, PricingTable, FeatureGrid):
 * page-level bands that compose the layout primitives + styled components
 * into a marketing/content surface.
 *
 * Unlike its sibling bands, Cta is PROPS-ONLY — it carries no repeating list,
 * so there is no dual surface / compound part / Fragment-walk. The 80% case is
 * one element:
 *
 *     <Cta
 *       eyebrow="Ready when you are"
 *       title="Ship your idea today"
 *       description="Describe what you want. AI builds it. We host it."
 *       actions={
 *         <>
 *           <Button>Get started</Button>
 *           <Button variant="gray">Talk to sales</Button>
 *         </>
 *       }
 *     />
 *
 * `children` falls through after the actions row inside the same Stack for the
 * rare case a consumer wants extra inline content (a fine-print note, a second
 * cluster); it is NOT a compound-part channel.
 *
 * Layout (dogfoods the layout primitives — never re-rolls flex/grid):
 *   - The root is a `<section data-slot="cta">` with generous vertical
 *     padding. The `plain` variant (default) paints NO heavy background so it
 *     drops into a page composably; the `tinted` variant wraps the body in a
 *     rounded accent-tint panel (a contained CTA card) via the house
 *     `color-mix` idiom.
 *   - Inside sits a `Container` (the single inline-width authority) holding a
 *     vertical `Stack`: eyebrow → `<h2>` → description → actions `Cluster`.
 *   - `align="center"` (default) centers the column on a readable measure;
 *     `align="start"` start-aligns it. The actions Cluster mirrors `align`.
 *
 * a11y:
 *   - The title is an `<h2>` (the band sits under a page `<h1>`); the
 *     `<section>` is `aria-labelledby` it ONLY when the title renders — the
 *     attr is gated on the heading actually rendering, so an absent, `false`,
 *     or empty (`""`) `title` never leaves an empty heading nor a dangling
 *     reference (the Hero R6 / PricingTable / FeatureGrid lesson). A CTA
 *     normally always carries a title, but the guard holds regardless.
 *   - actions are real `<Button>`s; the eyebrow may be a `Badge` or plain
 *     text. forced-colors keeps every text region on `CanvasText` and gives
 *     the tinted panel a visible system edge; the reduced-motion parity block
 *     is present.
 *
 * `data-slot` vocabulary (mirrors Hero / FeatureGrid so consumers target
 * regions in CSS without leaning on the internal BEM class names):
 *   cta             — the <section> root (overridable)
 *   cta-panel       — the body wrapper (tinted panel under variant="tinted")
 *   cta-eyebrow     — small label above the title
 *   cta-title       — the section <h2>
 *   cta-description — muted supporting <p>
 *   cta-actions     — the CTA Cluster
 */
import {
  forwardRef,
  useId,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { classnames } from "../../components/_classnames";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Stack } from "../../layouts/Stack";
import { Cluster } from "../../layouts/Cluster";
import type { SectionTone } from "../_tone";

/** Text + actions alignment for the CTA band. */
export type CtaAlign = "center" | "start";

/** Surface treatment for the CTA band. */
export type CtaVariant = "plain" | "tinted";

/* ─── props ───────────────────────────────────────────────────────────── */

export interface CtaProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /**
   * Optional small label above the title (eyebrow). Plain text or a
   * `Badge`/node.
   */
  eyebrow?: ReactNode;

  /**
   * The call-to-action headline. Renders as the band's `<h2>` AND becomes the
   * section's `aria-labelledby` target — the label attr is gated on the
   * heading actually rendering, so an absent, `false`, or empty (`""`) `title`
   * (e.g. the `showTitle && "…"` idiom) never leaves an empty heading nor a
   * dangling reference.
   */
  title?: ReactNode;

  /** Optional supporting paragraph under the title (muted). */
  description?: ReactNode;

  /**
   * Call-to-action row — typically a primary + secondary `<Button>`. Rendered
   * as a `Cluster` so the buttons wrap gracefully on narrow widths; aligned to
   * match the band (`center` → centered, `start` → start).
   */
  actions?: ReactNode;

  /**
   * Text + actions alignment. `center` (default) centers the column on a
   * readable measure; `start` start-aligns it.
   */
  align?: CtaAlign;

  /**
   * Surface treatment.
   * - `plain` (default): transparent — the band drops into a page composably
   *   (the consumer paints the page backdrop).
   * - `tinted`: a subtle accent-tint panel with radius + padding (a contained
   *   CTA card), via the house `color-mix` idiom.
   */
  variant?: CtaVariant;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `md` — a CTA reads tightest on a narrower measure than the wider
   * content bands.
   */
  size?: ContainerSize;

  /**
   * Full-bleed band tone — the shared page-rhythm system. The root stamps
   * `data-tone`; the band treatment lives in `sections/_section-tone.css`.
   * - `default` (default): transparent; inherits the page backdrop.
   * - `muted`: a subtle full-bleed surface fill so the band reads as its own
   *   panel.
   * - `accent`: an `--zs-accent` fill with the inner ink remapped to
   *   `--zs-accent-ink` — the bold contrast band, a CTA's loudest close.
   *
   * Distinct from `variant`: `variant="tinted"` paints a CONTAINED tint card
   * on the readable measure, whereas `tone="accent"` paints the WHOLE band.
   * For a punchy closing CTA prefer `tone="accent"` with the default `plain`
   * variant.
   */
  tone?: SectionTone;

  /**
   * Extra inline content rendered after the actions row inside the same Stack
   * (e.g. a fine-print note). NOT a compound-part channel — Cta is props-only.
   */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"cta"`. A composing section can
   * override it so consumers target the outer element via its own slot
   * vocabulary. Mirrors Card / Hero / FeatureGrid / Container.
   */
  "data-slot"?: string;
}

/* ─── root ────────────────────────────────────────────────────────────── */

export const Cta = forwardRef<HTMLElement, CtaProps>(function Cta(
  {
    eyebrow,
    title,
    description,
    actions,
    align = "center",
    variant = "plain",
    size = "md",
    tone = "default",
    className,
    children,
    "data-slot": dataSlot = "cta",
    ...rest
  },
  ref,
) {
  const composedClassName = classnames("zs-cta", className);

  // Stable id the section uses for aria-labelledby when a `title` renders.
  // useId is SSR-safe + collision-free across multiple Ctas on a page.
  const titleId = useId();

  // The section is labelled ONLY when a real `title` heading renders — never a
  // dangling aria-labelledby (the Hero R6 / PricingTable / FeatureGrid lesson).
  // A RENDERABILITY guard, not a nullish check: `title={showTitle && "Ship"}`
  // yields `title={false}` when the flag is off, and `title=""` is empty.
  // `false`, `null`, `undefined`, and `""` all mean "no title".
  const titleRenders = title != null && title !== false && title !== "";

  const body = (
    <Stack
      gap={4}
      align={align === "center" ? "center" : "start"}
      data-slot="cta-panel"
      data-variant={variant}
      className="zs-cta__panel"
    >
      {eyebrow != null ? (
        <p
          className="zs-section-eyebrow zs-cta__eyebrow"
          data-slot="cta-eyebrow"
        >
          {eyebrow}
        </p>
      ) : null}
      {titleRenders ? (
        <h2 id={titleId} data-slot="cta-title" className="zs-cta__title">
          {title}
        </h2>
      ) : null}
      {description != null ? (
        <p className="zs-cta__description" data-slot="cta-description">
          {description}
        </p>
      ) : null}
      {actions != null ? (
        <Cluster
          gap={3}
          justify={align === "center" ? "center" : "start"}
          data-slot="cta-actions"
          className="zs-cta__actions"
        >
          {actions}
        </Cluster>
      ) : null}
      {children}
    </Stack>
  );

  return (
    <section
      {...rest}
      ref={ref as Ref<HTMLElement>}
      data-slot={dataSlot}
      data-align={align}
      data-variant={variant}
      data-tone={tone}
      aria-labelledby={titleRenders ? titleId : undefined}
      className={composedClassName}
    >
      <Container size={size} data-slot="cta-container">
        {body}
      </Container>
    </section>
  );
});
Cta.displayName = "Cta";
