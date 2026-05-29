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
