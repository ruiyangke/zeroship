/**
 * Centralized agent env. Single source of truth for control plane URL +
 * key so individual tools don't drift.
 *
 * Defaults match the standard local dev setup (zeroship-control on
 * :9090 with the `dev-master-key` flag-of-shame). Production deploys
 * MUST set both env vars.
 */
export const CONTROL_URL =
  process.env.ZEROSHIP_CONTROL_URL ??
  process.env.ZEROSHIP_URL ?? // legacy alias
  "http://localhost:9090";

export const CONTROL_KEY =
  process.env.ZEROSHIP_MASTER_KEY ?? "dev-master-key";

export const GATEWAY_URL =
  process.env.ZEROSHIP_GATEWAY_URL ?? "http://localhost:8000";
