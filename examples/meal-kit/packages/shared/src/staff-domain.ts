import { z } from "zod";
import { marketSchema } from "@gather/meal-kit/domain";
import type { MarketId } from "@gather/meal-kit/catalog";

export const staffRoles = [
  "manager",
  "menu_editor",
  "fulfillment",
  "support",
] as const;
export const permissionNames = [
  "orders",
  "catalog",
  "inventory",
  "fulfillment",
  "support",
  "refund",
  "feedback",
  "preview",
] as const;
export type StaffRole = (typeof staffRoles)[number];
export type StaffPermission = (typeof permissionNames)[number];
const rolePermissions: Record<StaffRole, readonly StaffPermission[]> = {
  manager: permissionNames,
  menu_editor: ["catalog"],
  fulfillment: ["orders", "inventory", "fulfillment"],
  support: ["orders", "support", "feedback"],
};
export const staffGrantSchema = z.object({
  market: marketSchema,
  roles: z
    .array(z.enum(staffRoles))
    .min(1)
    .max(staffRoles.length)
    .refine((roles) => new Set(roles).size === roles.length),
});
export const staffSettingsSchema = z.object({
  name: z.string().trim().min(1).max(100),
  active: z.boolean(),
  recipeEditor: z.boolean(),
  grants: z
    .array(staffGrantSchema)
    .max(marketSchema.options.length)
    .refine(
      (grants) =>
        new Set(grants.map((grant) => grant.market)).size === grants.length,
    ),
});
export type StaffSettings = z.infer<typeof staffSettingsSchema>;
export type StaffAccess = Pick<StaffSettings, "recipeEditor" | "grants"> & {
  administrator: boolean;
};
export function accessFor(
  settings: StaffSettings | null,
  administrator = false,
): StaffAccess | null {
  if (administrator)
    return { administrator: true, recipeEditor: true, grants: [] };
  if (!settings?.active || (!settings.recipeEditor && !settings.grants.length))
    return null;
  return {
    administrator: false,
    recipeEditor: settings.recipeEditor,
    grants: settings.grants,
  };
}
export function allows(
  access: StaffAccess | null | undefined,
  permission: StaffPermission,
  market: MarketId,
) {
  return Boolean(
    access?.administrator ||
    access?.grants.some(
      (grant) =>
        grant.market === market &&
        grant.roles.some((role) => rolePermissions[role].includes(permission)),
    ),
  );
}
export function canUseWorkspace(
  access: StaffAccess | null | undefined,
  market: MarketId,
) {
  return Boolean(
    access?.administrator ||
    access?.recipeEditor ||
    access?.grants.some((grant) => grant.market === market),
  );
}
export const staffMemberSchema = staffSettingsSchema.extend({
  id: z.string(),
  subject: z.string(),
  version: z.number().int().positive(),
});
export type StaffMember = z.infer<typeof staffMemberSchema>;
export const saveStaffSchema = staffSettingsSchema.extend({
  subject: z
    .string()
    .trim()
    .min(1)
    .max(100)
    .regex(/^[A-Za-z0-9_.:@|-]+$/),
  expectedVersion: z.number().int().positive().nullable(),
  requestKey: z.string().uuid(),
});
export function normalizedStaff(input: z.infer<typeof saveStaffSchema>) {
  return {
    ...input,
    grants: input.grants
      .map((grant) => ({ ...grant, roles: [...grant.roles].sort() }))
      .sort((a, b) => a.market.localeCompare(b.market)),
  };
}
