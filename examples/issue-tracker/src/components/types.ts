// Shared view-model types, all derived from the real RPC signatures in
// ../api so this file can never drift from what the server actually
// returns. No shape here is hand-declared independently of index.ts.
import type {
  currentUser,
  dependencyGraph,
  dependencyTree,
  getBug,
  getProduct,
  listAttachments,
  listCc,
  listComments,
  listComponents,
  listDuplicates,
  listFlagRequests,
  listKeywords,
  listMilestones,
  listNotifications,
  listProducts,
  listSavedSearches,
  listUsers,
  listVersions,
  quickSearch,
  reportByAssignee,
  reportByComponent,
  reportSummary,
  reportTimeToResolve,
  reportTrend,
  searchBugs,
} from "../api";

export type Bug = Awaited<ReturnType<typeof searchBugs>>[number];
export type BugDetail = Awaited<ReturnType<typeof getBug>>;
export type Activity = BugDetail["activities"][number];
export type Comment = Awaited<ReturnType<typeof listComments>>[number];
export type Attachment = Awaited<ReturnType<typeof listAttachments>>[number];
export type CcEntry = Awaited<ReturnType<typeof listCc>>[number];
export type DependencyGraph = Awaited<ReturnType<typeof dependencyGraph>>;
export type DependencyTreeNode = Awaited<ReturnType<typeof dependencyTree>>;
export type DuplicateBug = Awaited<ReturnType<typeof listDuplicates>>[number];
export type Keyword = Awaited<ReturnType<typeof listKeywords>>[number];
export type FlagRequests = Awaited<ReturnType<typeof listFlagRequests>>;
export type FlagRequestEntry = FlagRequests["setByMe"][number];
export type Product = Awaited<ReturnType<typeof listProducts>>[number];
export type ProductDetail = Awaited<ReturnType<typeof getProduct>>;
export type FlagType = ProductDetail["flagTypes"][number];
export type ComponentRow = Awaited<ReturnType<typeof listComponents>>[number];
export type VersionRow = Awaited<ReturnType<typeof listVersions>>[number];
export type MilestoneRow = Awaited<ReturnType<typeof listMilestones>>[number];
export type SavedSearch = Awaited<ReturnType<typeof listSavedSearches>>[number];
export type CurrentUser = Awaited<ReturnType<typeof currentUser>>;
export type UserRow = Awaited<ReturnType<typeof listUsers>>[number];
export type NotificationRow = Awaited<ReturnType<typeof listNotifications>>[number];
export type ReportSummary = Awaited<ReturnType<typeof reportSummary>>;
export type ReportByComponent = Awaited<ReturnType<typeof reportByComponent>>;
export type ReportByAssignee = Awaited<ReturnType<typeof reportByAssignee>>;
export type ReportTrend = Awaited<ReturnType<typeof reportTrend>>;
export type ReportTimeToResolve = Awaited<ReturnType<typeof reportTimeToResolve>>;
export type QuickSearchResult = Awaited<ReturnType<typeof quickSearch>>;
