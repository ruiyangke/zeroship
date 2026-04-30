// Re-export the server functions under the same names the dashboard
// React code imports (apps/api.ts shape). The vite-plugin transforms
// the `"use server"` modules into RPC stubs that call across the
// runtime's wire, so from the browser side these look like plain
// async functions.

export {
  listApps,
  getApp,
  createApp,
  deleteApp,
  deployApp,
  updatePlan,
  getAppLogs,
  listVars,
  setVar,
  deleteVar,
  listSecrets,
  setSecret,
  deleteSecret,
  type AppRecord,
  type EnvVar,
} from "../../server/apps";

export { appPreviewUrl } from "../lib/preview-url";

/** Dev-mode auto-auth shim — kept for compatibility with dashboard
 *  components that import this. In zeroship-builder we always have
 *  an auth context so this is effectively unused. */
export function isDevAutoAuth(): boolean {
  return import.meta.env.DEV;
}
