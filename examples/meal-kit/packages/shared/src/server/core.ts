// The four things every server module in this workspace needs from the
// platform, shared by both apps because both apps bind the same database.

import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import { fail } from "../domain";

export function must<T>(result: { data: T; error?: unknown }): NonNullable<T> {
  if (result.error) throw result.error;
  return result.data as NonNullable<T>;
}

export function user() {
  const current = auth.getUser();
  if (!current) fail(/* i18n */ "Sign in to continue.", "UNAUTHENTICATED", 401);
  return current;
}

export type Tx = Parameters<Parameters<typeof env.db.transaction>[0]>[0];

export async function transact<T>(fn: (tx: Tx) => Promise<T>): Promise<T> {
  return must(await env.db.transaction(fn, { isolationLevel: "serializable" }));
}

export function changed<T>(row: T | null): T {
  if (!row)
    fail(
      /* i18n */ "These details have changed. Refresh the page and try again.",
      "CONFLICT",
      409,
    );
  return row;
}

export function demo() {
  if (env.GATHER_MODE && env.GATHER_MODE !== "demo")
    fail(
      /* i18n */ "Checkout is unavailable right now. Please try again later.",
      "PROVIDER_UNAVAILABLE",
      503,
    );
}
