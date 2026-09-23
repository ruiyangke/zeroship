import { describe, expect, test } from "vitest";
import {
  nextOpenCycle,
  planChange,
  preferencesSchema,
} from "@gather/meal-kit/account-domain";

describe("recurring plan dates", () => {
  const plan = { status: "paused", next_date: "2026-08-04", skipped: [] };
  const now = Date.parse("2026-09-11T10:00:00Z");
  test("resumes on the next future cycle with an open cutoff, preserving its weekday", () => {
    expect(planChange(plan, "resume", "us", now)).toEqual({
      status: "active",
      next_date: "2026-09-15",
      skipped: [],
    });
    expect(
      nextOpenCycle("2026-09-15", "us", Date.parse("2026-09-13T21:59:59Z")),
    ).toBe("2026-09-15");
    expect(
      nextOpenCycle("2026-09-15", "us", Date.parse("2026-09-13T22:00:00Z")),
    ).toBe("2026-09-22");
    expect(
      nextOpenCycle("2026-10-30", "us", Date.parse("2026-11-01T12:00:00Z")),
    ).toBe("2026-11-06");
    for (const date of ["invalid", "2026-02-30", "2026-13-01"])
      expect(() => nextOpenCycle(date, "us", now)).toThrow();
  });
  test("skips an eligible future cycle and refuses canceled or inactive plans", () => {
    expect(
      planChange({ ...plan, status: "active" }, "skip", "us", now),
    ).toEqual({
      status: "active",
      next_date: "2026-09-22",
      skipped: ["2026-09-15"],
    });
    expect(() => planChange(plan, "skip", "us", now)).toThrow(/Resume/);
    for (const action of ["resume", "pause", "skip"] as const)
      expect(() =>
        planChange({ ...plan, status: "canceled" }, action, "us", now),
      ).toThrow(/restart/);
    expect(
      planChange({ ...plan, status: "active" }, "cancel", "us", now).status,
    ).toBe("canceled");
  });
});

test("preference defaults do not opt into marketing and reject unknown food data", () => {
  expect(preferencesSchema.parse({})).toEqual({
    favorites: [],
    exclude: [],
    cuisines: [],
    equipment: [],
    units: "metric",
    marketing: false,
  });
  expect(
    preferencesSchema.parse({
      favorites: ["lemon-chicken"],
      exclude: ["milk"],
      units: "imperial",
    }).exclude,
  ).toEqual(["milk"]);
  for (const value of [
    { favorites: [""] },
    { exclude: ["unknown-allergen"] },
    { equipment: ["unknown-equipment"] },
    { units: "arbitrary" },
  ])
    expect(preferencesSchema.safeParse(value).success).toBe(false);
});
