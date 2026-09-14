export { verifyWebhook } from "./webhook";
export type { VerifyOpts, VerifyResult } from "./webhook";

export {
  createPaymentsClient,
  PaymentsClient,
  PaymentsError,
} from "./connect";
export type {
  PaymentsClientOptions,
  CheckoutInput,
  CheckoutResult,
  OnboardingResult,
} from "./connect";
