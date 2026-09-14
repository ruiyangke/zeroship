/*
 * _tone.ts — the shared section tone type.
 *
 * Every section root (Hero / PricingTable / FeatureGrid / Cta / StatsBand /
 * Faq / Footer) accepts a `tone` prop and stamps BOTH the `data-section-band`
 * marker AND `data-tone` on its root. The full-bleed band treatment for each
 * tone lives in ONE shared stylesheet (`sections/_section-tone.css`), keyed off
 * `[data-section-band][data-tone="…"]` — the `data-section-band` marker scopes
 * the rules to the seven section roots so a bare `[data-tone]` elsewhere in the
 * package (e.g. AlertDialog's intent buttons) is never matched. This module is
 * the matching ONE source of truth for the TypeScript union so the seven
 * sections share an identical contract rather than re-declaring it.
 */

/**
 * Full-bleed band tone — the page-rhythm backbone shared across every
 * section.
 *
 * - `default` — transparent; the band inherits the page backdrop (the legacy
 *   behavior). This is the default for every section.
 * - `muted` — a subtle full-bleed surface fill so the band reads as its own
 *   panel, giving light/dark alternation against `default` bands.
 * - `accent` — an `--zs-accent` fill with the inner ink remapped to
 *   `--zs-accent-ink` (the bold contrast band — best for a closing CTA).
 *
 * See `sections/_section-tone.css` for the band treatments.
 */
export type SectionTone = "default" | "muted" | "accent";
