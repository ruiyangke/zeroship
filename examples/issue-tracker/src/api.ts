// Client-side RPC surface. The Vite plugin rewrites these imports into
// HTTP-RPC stubs when this module ends up in the browser bundle, so this
// file must stick to plain re-exports of the "use server" procedures in
// ./index -- never a hand-rolled fetch to /__zeroship/v1/<id>.
export {
  // Bugs
  createBug,
  getBug,
  searchBugs,
  updateBug,
  changeBugStatus,
  resolveBug,
  reopenBug,
  markBugDuplicate,
  reassignBug,
  setBugSeverity,
  setBugPriority,
  moveBug,
  // Comments
  addComment,
  listComments,
  editComment,
  setCommentPrivate,
  // Attachments
  uploadAttachment,
  listAttachments,
  getAttachment,
  setAttachmentObsolete,
  deleteAttachment,
  // Dependencies and duplicates
  addDependency,
  removeDependency,
  dependencyGraph,
  dependencyTree,
  listDuplicates,
  // Keywords, flags, and CC
  listKeywords,
  createKeyword,
  attachKeyword,
  detachKeyword,
  setFlag,
  clearFlag,
  listFlagRequests,
  listFlags,
  addCc,
  removeCc,
  listCc,
  listMyCc,
  // Products and administration
  listProducts,
  getProduct,
  createProduct,
  updateProduct,
  listComponents,
  createComponent,
  updateComponent,
  listVersions,
  createVersion,
  listMilestones,
  createMilestone,
  // Search and saved searches
  structuredSearch,
  quickSearch,
  listSavedSearches,
  saveSavedSearch,
  deleteSavedSearch,
  // Users and notifications
  currentUser,
  listUsers,
  getUser,
  updateUserPrefs,
  listNotifications,
  markNotificationRead,
  unreadNotificationCount,
  // Reports
  reportSummary,
  reportByComponent,
  reportByAssignee,
  reportTrend,
  reportTimeToResolve,

  // Access control -- groups, and product- and bug-level restriction
  createGroup,
  listGroups,
  addGroupMember,
  removeGroupMember,
  restrictProduct,
  unrestrictProduct,
  restrictBug,
  unrestrictBug,

  // Voting
  castVote,
  listMyVotes,

  // Watching
  addWatcher,
  removeWatcher,
  listWatchers,
  addSeeAlso,
  removeSeeAlso,
  listSeeAlso,
} from "./index";
