/*
 * sections/ — page-level marketing / content bands (PrimeBlocks /
 * Tailwind-UI "Marketing" taxonomy). Each section composes the layout
 * primitives + styled components into a full-width page band.
 *
 * Unlike `components/` (explicit allowlist on the root barrel), the
 * `sections/` and `layouts/` / `blocks/` layers are surfaced via
 * `export *` from the root `index.ts`, so a section reaches the public API
 * just by being exported here.
 */
export {
  Hero,
  type HeroProps,
  type HeroAlign,
  type HeroEyebrowProps,
  type HeroTitleProps,
  type HeroDescriptionProps,
  type HeroActionsProps,
  type HeroMediaProps,
} from "./Hero";

export {
  PricingTable,
  type PricingTableProps,
  type PricingTableTierProps,
  type PricingTableFeatureProps,
  type PricingTier,
  type PricingFeature,
  type PricingHeadingLevel,
} from "./PricingTable";

export {
  FeatureGrid,
  type FeatureGridProps,
  type FeatureGridItemProps,
  type FeatureItem,
  type FeatureGridAlign,
  type FeatureGridColumns,
} from "./FeatureGrid";
