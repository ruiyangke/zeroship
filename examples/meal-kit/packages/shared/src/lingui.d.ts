declare module "*.po" {
  import type { Messages } from "@lingui/core";
  export const messages: Messages;
}

declare module "*messages.mjs" {
  import type { Messages } from "@lingui/core";
  export const messages: Messages;
}
