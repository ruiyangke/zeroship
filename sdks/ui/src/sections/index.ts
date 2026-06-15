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

/* The shared section tone — every section's `tone` prop is this union. */
export type { SectionTone } from "./_tone";

export {
  SectionFrame,
  SectionHeader,
  type SectionFrameProps,
  type SectionFrameSpacing,
  type SectionHeaderAlign,
  type SectionHeaderLayout,
  type SectionHeaderProps,
  type SectionHeaderTitleSize,
} from "./SectionFrame";

export {
  ScrollRail,
  type ScrollRailProps,
  type ScrollRailSnap,
} from "./ScrollRail";

export {
  ResponsivePicture,
  type ResponsivePictureFit,
  type ResponsivePictureFrame,
  type ResponsivePicturePosition,
  type ResponsivePictureProps,
  type ResponsivePictureRadius,
  type ResponsivePictureRatio,
  type ResponsivePictureSource,
} from "./ResponsivePicture";

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

export {
  Cta,
  type CtaProps,
  type CtaAlign,
  type CtaVariant,
} from "./Cta";

export {
  StatsBand,
  type StatsBandProps,
  type StatsBandStatProps,
  type StatItem,
  type StatsBandAlign,
  type StatsBandColumns,
} from "./StatsBand";

export {
  Faq,
  type FaqProps,
  type FaqItemProps,
  type FaqEntry,
} from "./Faq";

export {
  Footer,
  type FooterProps,
  type FooterColumnProps,
  type FooterColumnData,
  type FooterLink,
} from "./Footer";
