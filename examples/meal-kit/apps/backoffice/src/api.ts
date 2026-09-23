// Browser-facing imports, taken from the module that DEFINES each procedure.
// The client build replaces a `"use server"` module with stubs for the
// procedures declared in that file, so a re-export chain through `server.ts`
// resolves to nothing here.
export {
  getSession,
  getOperations,
  prepareMenu,
  advanceOrder,
  setInventory,
  resolveIssue,
  setPaymentScenario,
} from "./server";
export {
  getCatalogWorkspace,
  saveRecipeDraft,
  approveRecipe,
  archiveRecipe,
  saveMenuDraft,
  publishMenu,
  withdrawMenu,
} from "./server/catalog";
export { loadSampleMenus } from "./server/demo-catalog";
export { getRecipeFeedback } from "./server/cooking";
export { getStaffTeam, saveStaffMember } from "./server/staff";
export {
  settleDemoPayment,
  expireDemoCheckout,
  sweepCheckouts,
  refundRecoveredPayment,
} from "./server/checkout";
