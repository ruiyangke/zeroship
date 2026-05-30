// Shared status-intent vocabulary — Badge/Banner/ErrorState derive from it.
export type { Intent } from "./_intent";

// Composed blocks — empty/error/loading state family (Wave 2a).
export {
  EmptyState,
  type EmptyStateProps,
  type EmptyStateIconProps,
  type EmptyStateTitleProps,
  type EmptyStateDescriptionProps,
  type EmptyStateActionsProps,
} from "./EmptyState";
export {
  ErrorState,
  type ErrorStateProps,
  type ErrorStateIntent,
  type ErrorStateTitleProps,
  type ErrorStateDescriptionProps,
  type ErrorStateActionsProps,
} from "./ErrorState";
export { Skeleton, type SkeletonProps, type SkeletonVariant } from "./Skeleton";
export { Spinner, type SpinnerProps, type SpinnerSize } from "./Spinner";

// Chip blocks (Wave 2b) — static status/label chip + interactive chip.
export {
  Badge,
  type BadgeProps,
  type BadgeIntent,
  type BadgeVariant,
  type BadgeSize,
} from "./Badge";
export { Tag, type TagProps, type TagSize } from "./Tag";

// Content blocks (Wave 2c) — composed metric / message / key-value blocks.
export {
  StatCard,
  type StatCardProps,
  type StatCardDelta,
  type StatCardDeltaDirection,
} from "./StatCard";
export {
  Banner,
  type BannerProps,
  type BannerIntent,
  type BannerTitleProps,
  type BannerDescriptionProps,
  type BannerActionsProps,
} from "./Banner";
export {
  DescriptionList,
  type DescriptionListProps,
  type DescriptionListOrientation,
  type DescriptionListItemProps,
  type DescriptionListTermProps,
  type DescriptionListDetailProps,
} from "./DescriptionList";

// Navigation block — breadcrumb trail (dual ergonomic + compound surface).
export {
  Breadcrumbs,
  type BreadcrumbsProps,
  type BreadcrumbItem,
  type BreadcrumbsItemProps,
  type BreadcrumbsLinkProps,
  type BreadcrumbsPageProps,
  type BreadcrumbsSeparatorProps,
} from "./Breadcrumbs";

// Data block (Wave 3) — presentational, generic data table.
export {
  DataTable,
  type DataTableProps,
  type DataTableColumn,
  type DataTableSort,
  type DataTableSelectionMode,
  type DataTableAlign,
  type DataTableDensity,
} from "./DataTable";
