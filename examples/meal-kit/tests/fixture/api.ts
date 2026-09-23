// The two apps' procedure surfaces in one namespace, for the browser specs.
//
// A journey here crosses both apps - a customer buys in the storefront and an
// operator moves the box in the back office - so a spec asserting on the
// shapes needs both. `getSession` exists in both and returns the same shape;
// the storefront's stands for it.

export {
  getSession,
  getCatalog,
  getRecipe,
  joinWaitlist,
  getQuote,
  checkout,
  getAccount,
  getOrder,
  payOrder,
  cancelOrder,
  editOrder,
  updatePlan,
  previewRenewal,
  renewPlan,
  reportIssue,
  downloadReceipt,
  saveAddress,
  deleteAddress,
  savePreferences,
  requestPrivacy,
  cancelPrivacyRequest,
  downloadPrivacyExport,
  getCookingRecipe,
  saveRecipeFeedback,
} from "../../apps/storefront/src/api";

export {
  getOperations,
  prepareMenu,
  advanceOrder,
  setInventory,
  resolveIssue,
  setPaymentScenario,
  getCatalogWorkspace,
  saveRecipeDraft,
  approveRecipe,
  archiveRecipe,
  saveMenuDraft,
  publishMenu,
  withdrawMenu,
  loadSampleMenus,
  getRecipeFeedback,
  getStaffTeam,
  saveStaffMember,
  settleDemoPayment,
  expireDemoCheckout,
  sweepCheckouts,
  refundRecoveredPayment,
} from "../../apps/backoffice/src/api";
