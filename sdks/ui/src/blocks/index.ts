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

// Data block (Wave 3) — presentational, generic data grid with an opt-in
// managed (client-side) sort/filter/paginate engine (v2).
export {
  DataTable,
  type DataTableProps,
  type DataTableColumn,
  type DataTableColumnType,
  type DataTableRowAction,
  type DataTableSort,
  type DataTableColumnFilters,
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

// FilterBar block — governed search/filter toolbar for collections.
export {
  FilterBar,
  type FilterBarProps,
  type FilterBarActiveFilter,
} from "./FilterBar";

// ListView block — governed stacked list of item rows
// (leading · content · meta · trailing) with interactive-row a11y.
export {
  ListView,
  type ListViewProps,
  type ListViewItem,
  type ListViewDensity,
  type ListViewLeadingProps,
  type ListViewContentProps,
  type ListViewTitleProps,
  type ListViewDescriptionProps,
  type ListViewMetaProps,
  type ListViewTrailingProps,
} from "./ListView";

// FormSection block — governed settings/form section (header · body ·
// optional footer actions) in stacked|aside layout.
export {
  FormSection,
  type FormSectionProps,
  type FormSectionOrientation,
  type FormSectionHeaderProps,
  type FormSectionTitleProps,
  type FormSectionDescriptionProps,
  type FormSectionBodyProps,
  type FormSectionFooterProps,
  type FormSectionFooterAlign,
} from "./FormSection";

// AuthForm block — centered sign-in / sign-up form composing
// Card · Form · Field · Input · Button · Banner · Separator.
export {
  AuthForm,
  type AuthFormProps,
  type AuthFormMode,
  type AuthFormValues,
} from "./AuthForm";
