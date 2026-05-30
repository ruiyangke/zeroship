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

// Pagination block — controlled page navigation (standalone + the footer
// control DataTable v2 composes).
export {
  Pagination,
  buildPageItems,
  type PaginationProps,
  type PaginationSize,
} from "./Pagination";
