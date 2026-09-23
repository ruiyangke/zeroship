import { env } from "zeroship";
import { fail } from "../domain";
import type { MarketId } from "../catalog";
import {
  accessFor,
  allows,
  staffSettingsSchema,
  type StaffPermission,
} from "../staff-domain";
import { must, user, type Tx } from "./core";

export function administratorIds() {
  return String(env.GATHER_ADMIN_IDS ?? "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
}
export function requireAdministrator() {
  const actor = user();
  if (!administratorIds().includes(actor.id)) forbidden();
  return actor;
}
export function forbidden(): never {
  fail(
    /* i18n */ "Your staff account does not have access to this action in this country.",
    "FORBIDDEN",
    403,
  );
}
export async function staffAccess(subject: string, tx?: Tx) {
  if (administratorIds().includes(subject)) return accessFor(null, true)!;
  const member = tx
    ? await tx.meal_staff_members.get({ subject })
    : must(await env.db.meal_staff_members.get({ subject }));
  return accessFor(member ? staffSettingsSchema.parse(member.settings) : null);
}
export async function requirePermission(
  permission: StaffPermission,
  market: MarketId,
  tx?: Tx,
) {
  const actor = user();
  if (!allows(await staffAccess(actor.id, tx), permission, market)) forbidden();
  return actor;
}
export async function requireRecipeEditor(tx?: Tx) {
  const actor = user();
  if (!(await staffAccess(actor.id, tx))?.recipeEditor) forbidden();
  return actor;
}
