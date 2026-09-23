import { describe, expect, test } from "vitest";
import {
  accessFor, allows, canUseWorkspace, normalizedStaff, permissionNames,
  staffSettingsSchema, type StaffRole, type StaffSettings,
} from "@gather/meal-kit/staff-domain";

const settings = (role: StaffRole): StaffSettings => ({
  name: "Alex", active: true, recipeEditor: false,
  grants: [{ market: "us", roles: [role] }],
});

describe("staff authority", () => {
  test.each([
    ["manager", ["orders", "catalog", "inventory", "fulfillment", "support", "refund", "feedback", "preview"]],
    ["menu_editor", ["catalog"]],
    ["fulfillment", ["orders", "inventory", "fulfillment"]],
    ["support", ["orders", "support", "feedback"]],
  ] as const)("%s grants only its actions in its assigned country", (role, expected) => {
    const access = accessFor(settings(role));
    expect(permissionNames.filter(permission => allows(access, permission, "us"))).toEqual(expected);
    for (const permission of permissionNames)
      expect(allows(access, permission, "cn")).toBe(false);
    expect(canUseWorkspace(access, "us")).toBe(true);
    expect(canUseWorkspace(access, "cn")).toBe(false);
    expect(access?.recipeEditor).toBe(false);
  });
  test("revocation removes staff authority while global editing grants no country operations", () => {
    expect(accessFor({ ...settings("manager"), active: false })).toBeNull();
    expect(accessFor(null)).toBeNull();
    expect(accessFor({ ...settings("manager"), grants: [] })).toBeNull();
    const editor = accessFor({ ...settings("menu_editor"), grants: [], recipeEditor: true });
    expect(canUseWorkspace(editor, "cn")).toBe(true);
    for (const permission of permissionNames) expect(allows(editor, permission, "cn")).toBe(false);
    const admin = accessFor(null, true);
    expect(admin?.recipeEditor).toBe(true);
    for (const permission of permissionNames) expect(allows(admin, permission, "cn")).toBe(true);
  });
  test("rejects ambiguous grants and normalizes equivalent commands", () => {
    for (const grants of [
      [{ market: "us", roles: [] }],
      [{ market: "us", roles: ["owner"] }],
      [{ market: "us", roles: ["support", "support"] }],
      [{ market: "us", roles: ["support"] }, { market: "us", roles: ["manager"] }],
      [{ market: "unknown", roles: ["manager"] }],
    ]) expect(staffSettingsSchema.safeParse({ ...settings("support"), grants }).success).toBe(false);
    const command = { ...settings("support"), subject: "pws_alex", expectedVersion: null, requestKey: crypto.randomUUID(),
      grants: [{ market: "us" as const, roles: ["support", "fulfillment"] as StaffRole[] }, { market: "cn" as const, roles: ["menu_editor"] as StaffRole[] }] };
    expect(normalizedStaff(command)).toEqual(normalizedStaff({ ...command, grants: [...command.grants].reverse().map(grant => ({ ...grant, roles: [...grant.roles].reverse() })) }));
  });
});
