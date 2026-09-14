/*
 * Faq — a frequently-asked-questions band.
 *
 * An optional header lead-in (eyebrow + title + description) above a
 * disclosure list of question → answer rows. A piece of the `sections/` layer
 * (after Hero, PricingTable, FeatureGrid): page-level bands that compose the
 * layout primitives + styled components.
 *
 * Faq COMPOSES THE EXISTING `Accordion` component (components/Accordion) — it
 * does NOT re-roll the disclosure machinery. A FAQ is exactly an Accordion of
 * question (the heading-button trigger) → answer (the panel region); the
 * Accordion already owns the disclosure a11y (button triggers, aria-expanded,
 * aria-controls, the labelled panel region, roving focus, reduced-motion).
 * Faq's job is the band chrome (header, measure, surface) + the dual-surface
 * authoring ergonomics around the Accordion.
 *
 * Like FeatureGrid / StatsBand, Faq exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case, the entries in one array:
 *        <Faq
 *          title="Questions"
 *          items={[
 *            { id: "cost", question: "How much does it cost?",
 *              answer: "The platform takes 15%; you keep the rest." },
 *          ]}
 *        />
 *
 *   2. Compound parts — full control over an entry's composition / order:
 *        <Faq title="Questions">
 *          <Faq.Item question="How much does it cost?">
 *            The platform takes 15%; you keep the rest.
 *          </Faq.Item>
 *        </Faq>
 *
 * The two surfaces are ADDITIVE — there is NO suppression (the Hero rule).
 * The `items` prop renders FIRST; any compound `<Faq.Item>` children then fall
 * through AFTER, in document order. The root walks the children with a
 * recursive `flattenChildren` (Fragments descended). Mirrors
 * sections/FeatureGrid/FeatureGrid.tsx + sections/StatsBand/StatsBand.tsx.
 *
 * Layout:
 *   - The root is a `<section data-slot="faq">` with generous vertical
 *     padding and NO heavy background (composable).
 *   - Inside sits a `Container` holding an optional header `Stack` above the
 *     composed `Accordion`. The Accordion is constrained to a readable measure
 *     (`max-inline-size`) so the Q/A list never runs edge-to-edge.
 *
 * a11y:
 *   - The section header title is an `<h2>`; the `<section>` is
 *     `aria-labelledby` it ONLY when the title renders — gated on the heading
 *     actually rendering, so no dangling reference headerless (the Hero R6 /
 *     FeatureGrid lesson).
 *   - The disclosure a11y is the composed Accordion's: each question is an
 *     `<h3>` heading-button (Accordion.Header wraps the Trigger in an `<h3>`,
 *     so the questions sit one level under the section `<h2>`), each answer is
 *     a labelled region. `multiple` maps to the Accordion's `multiple` mode
 *     (any number open); the default `single` mode opens one at a time and is
 *     `collapsible` so the open question can be re-clicked closed.
 *   - forced-colors + reduced-motion are handled by the composed Accordion; a
 *     parity reduced-motion gate is present in Faq's own CSS too.
 *
 * `data-slot` vocabulary:
 *   faq         — the <section> root (overridable)
 *   faq-header  — the optional eyebrow + title + desc lead-in
 *   faq-title   — the optional section lead-in <h2>
 *   faq-list    — the composed Accordion wrapper
 *   faq-item    — each Q/A row (the composed Accordion.Item)
 */
import {
  Children,
  Fragment,
  forwardRef,
  isValidElement,
  useId,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { classnames } from "../../components/_classnames";
import { Accordion } from "../../components/Accordion";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Stack } from "../../layouts/Stack";
import type { SectionTone } from "../_tone";

/* ─── entry ───────────────────────────────────────────────────────────── */

export interface FaqEntry {
  /** Stable id — the Accordion item `value` and React key (else the index). */
  id?: string;
  /** The question — renders as the Accordion trigger (an `<h3>` heading-button). */
  question: ReactNode;
  /** The answer — renders inside the Accordion panel (a labelled region). */
  answer: ReactNode;
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface FaqProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /** Optional small label above the section title (eyebrow). */
  eyebrow?: ReactNode;

  /**
   * Optional section title lead-in. When a NON-EMPTY `title` is supplied it
   * renders as an `<h2>` AND becomes the section's `aria-labelledby` target —
   * gated on the heading actually rendering, so an absent, `false`, or empty
   * (`""`) `title` never leaves a dangling reference (the Hero R6 /
   * FeatureGrid lesson).
   */
  title?: ReactNode;

  /** Optional supporting paragraph under the title (muted). */
  description?: ReactNode;

  /**
   * Ergonomic mode: the Q/A entries, in order. Rendered FIRST; any compound
   * `<Faq.Item>` children fall through AFTER (the surfaces are additive — no
   * suppression).
   */
  items?: FaqEntry[];

  /**
   * Allow multiple answers open at once. Maps to the composed Accordion's
   * `multiple` mode. Default `false` — `single` mode opens one answer at a
   * time (and is `collapsible`, so the open question can be re-clicked closed).
   */
  multiple?: boolean;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `md` — a FAQ reads best on a narrow, readable measure.
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

  /** Compound `<Faq.Item>` parts (additive after `items`). */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"faq"`. A composing section can
   * override it so consumers target the outer element via its own slot
   * vocabulary. Mirrors Card / Hero / FeatureGrid / Container.
   */
  "data-slot"?: string;
}

/* ─── compound part props ─────────────────────────────────────────────── */

export interface FaqItemProps {
  /** Stable id — the Accordion item `value` and React key (else the index). */
  id?: string;
  /** The question — renders as the Accordion trigger (an `<h3>` heading-button). */
  question: ReactNode;
  /**
   * The answer. Supplied via the `answer` prop OR as the part's `children`
   * (the prop wins when both are present).
   */
  answer?: ReactNode;
  /** Answer as children (alternative to `answer`). */
  children?: ReactNode;
}

/* ─── child walk ──────────────────────────────────────────────────────── */

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><Faq.Item/></>`) are visible to the normalizer. Mirrors
 * the walk in sections/FeatureGrid + sections/StatsBand. */
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

/* ─── resolved entry ──────────────────────────────────────────────────── */

/* The single source of truth for a Q/A row — both the `items` prop path and
 * the compound `<Faq.Item>` path funnel through here. The `value` is the
 * Accordion item key (namespaced by source surface + a band-stable prefix so
 * two FAQs on a page never collide), the `question` is the trigger heading,
 * and the `answer` is the panel content. */
interface ResolvedEntry {
  value: string;
  question: ReactNode;
  answer: ReactNode;
}

/* ─── root ────────────────────────────────────────────────────────────── */

const FaqRoot = forwardRef<HTMLElement, FaqProps>(function FaqRoot(
  {
    eyebrow,
    title,
    description,
    items,
    multiple = false,
    size = "md",
    tone = "default",
    className,
    children,
    "data-slot": dataSlot = "faq",
    ...rest
  },
  ref,
) {
  const composedClassName = classnames("zs-faq", className);

  // Stable id the section uses for aria-labelledby when an optional `title`
  // lead-in renders, AND a band-unique prefix for the Accordion item values
  // (so two FAQs on a page never collide on a `value`). useId is SSR-safe.
  const titleId = useId();
  const valuePrefix = useId();

  // ── Resolve the `items` prop path ─────────────────────────────────────
  // Values are NAMESPACED by source surface (`prop:` / `compound:`) + the
  // band prefix so a prop entry id and a compound entry key can never collide.
  const propEntries: ResolvedEntry[] = (items ?? []).map((entry, index) => ({
    value: `${valuePrefix}-prop-${entry.id ?? index}`,
    question: entry.question,
    answer: entry.answer,
  }));

  // ── Resolve the compound `<Faq.Item>` path (ADDITIVE) ─────────────────
  const compoundEntries: ResolvedEntry[] = [];
  flattenChildren(children).forEach((child, index) => {
    if (!isValidElement(child) || child.type !== FaqItem) return;
    const p = (child as ReactElement<FaqItemProps>).props;
    compoundEntries.push({
      value: `${valuePrefix}-compound-${(child as ReactElement).key ?? p.id ?? index}`,
      question: p.question,
      answer: p.answer ?? p.children,
    });
  });

  const allEntries = [...propEntries, ...compoundEntries];

  // The section is labelled ONLY when a real `title` heading renders — never a
  // dangling aria-labelledby (the Hero R6 / FeatureGrid lesson).
  const titleRenders = title != null && title !== false && title !== "";
  const hasHeader = titleRenders || eyebrow != null || description != null;

  // The composed Accordion. `multiple` → Accordion `multiple` mode (any number
  // open); the default single mode is `collapsible` so the open question can
  // be re-clicked closed (a FAQ should always be fully-collapsible). Each row
  // is an Accordion.Item whose Header→Trigger is the question (an <h3>
  // heading-button) and whose Panel is the answer (a labelled region).
  const accordion = multiple ? (
    <Accordion
      type="multiple"
      data-slot="faq-list"
      className="zs-faq__list"
    >
      {allEntries.map((entry) => (
        <Accordion.Item
          key={entry.value}
          value={entry.value}
          data-slot="faq-item"
        >
          <Accordion.Header>
            <Accordion.Trigger>{entry.question}</Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel>{entry.answer}</Accordion.Panel>
        </Accordion.Item>
      ))}
    </Accordion>
  ) : (
    <Accordion
      type="single"
      collapsible
      data-slot="faq-list"
      className="zs-faq__list"
    >
      {allEntries.map((entry) => (
        <Accordion.Item
          key={entry.value}
          value={entry.value}
          data-slot="faq-item"
        >
          <Accordion.Header>
            <Accordion.Trigger>{entry.question}</Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel>{entry.answer}</Accordion.Panel>
        </Accordion.Item>
      ))}
    </Accordion>
  );

  return (
    <section
      {...rest}
      ref={ref as Ref<HTMLElement>}
      data-slot={dataSlot}
      data-section-band=""
      data-tone={tone}
      aria-labelledby={titleRenders ? titleId : undefined}
      className={composedClassName}
    >
      <Container size={size} data-slot="faq-container">
        {hasHeader ? (
          <Stack
            gap={3}
            align="start"
            data-slot="faq-header"
            className="zs-faq__header"
          >
            {eyebrow != null ? (
              <p
                data-slot="faq-eyebrow"
                className="zs-section-eyebrow zs-faq__eyebrow"
              >
                {eyebrow}
              </p>
            ) : null}
            {titleRenders ? (
              <h2
                id={titleId}
                data-slot="faq-title"
                className="zs-faq__title"
              >
                {title}
              </h2>
            ) : null}
            {description != null ? (
              <p className="zs-faq__description">{description}</p>
            ) : null}
          </Stack>
        ) : null}

        {accordion}
      </Container>
    </section>
  );
});
FaqRoot.displayName = "Faq";

/* ─── compound part ───────────────────────────────────────────────────── */

/* Faq.Item is a MARKER: the root reads its props during the child walk and
 * renders the real Accordion.Item itself so the prop path and the compound
 * path produce identical markup. Rendering it standalone (outside a `<Faq>`)
 * is a misuse — it dev-warns and renders nothing rather than emit an orphan
 * Accordion item with no Accordion root around it. */
function FaqItem(_props: FaqItemProps): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "Faq.Item must be a direct (or Fragment-wrapped) child of <Faq>; the " +
        "root reads its props to render the Accordion row. Rendered standalone " +
        "it produces nothing.",
    );
  }
  return null;
}
(FaqItem as { displayName?: string }).displayName = "Faq.Item";

/* ─── public Faq namespace ────────────────────────────────────────────── */

type FaqComponent = typeof FaqRoot & {
  Item: typeof FaqItem;
};

export const Faq = FaqRoot as FaqComponent;
Faq.Item = FaqItem;
