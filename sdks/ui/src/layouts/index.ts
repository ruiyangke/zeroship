// Layout primitives + compositions. Filled per slice.
export type { Gap, Pad, Align, Justify, Side } from "./_layout-primitives";

export { Stack } from "./Stack";
export type { StackProps } from "./Stack";

export { Grid } from "./Grid";
export type { GridProps, GridColumns } from "./Grid";

export { Cluster } from "./Cluster";
export type { ClusterProps } from "./Cluster";

export { Container } from "./Container";
export type { ContainerProps, ContainerSize } from "./Container";

export { Split } from "./Split";
export type {
  SplitProps,
  SplitSideProps,
  SplitMainProps,
  SplitSide,
  SplitCollapse,
} from "./Split";

export { Center } from "./Center";
export type { CenterProps } from "./Center";

/* ─── Layout compositions — built FROM the primitives above. */
export { AppShell, useAppShellSidebar } from "./AppShell";
export type {
  AppShellProps,
  AppShellHeaderProps,
  AppShellSidebarProps,
  AppShellBodyProps,
  AppShellMainProps,
  AppShellFooterProps,
  AppShellSidebarSide,
} from "./AppShell";

export { PageHeader } from "./PageHeader";
export type {
  PageHeaderProps,
  PageHeaderBreadcrumbsProps,
  PageHeaderTitleProps,
  PageHeaderDescriptionProps,
  PageHeaderActionsProps,
  PageHeaderTextProps,
} from "./PageHeader";
