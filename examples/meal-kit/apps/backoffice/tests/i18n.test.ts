import { boxSizeMessage } from "@gather/meal-kit/box-copy";
import { recipes } from "../src/seed-catalog";
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { setupI18n } from "@lingui/core";
import { createCompiledCatalog } from "@lingui/cli/api";
import { formatter } from "@lingui/format-po";
import { messages as en } from "@gather/meal-kit/locales/en/messages.po";
import { messages as zh } from "@gather/meal-kit/locales/zh/messages.po";
import { locales, resolveLocale } from "@gather/meal-kit/locales";
import { deliveryLabel, money } from "@gather/meal-kit/catalog";

describe("Lingui catalogs", () => {
  it("compiles complete catalogs and rejects malformed ICU messages", () => {
    const format = formatter();
    const catalogs = Object.keys(locales).map((locale) => ({
      locale,
      messages: format.parse(
        readFileSync(
          new URL(`../../../packages/shared/locales/${locale}/messages.po`, import.meta.url),
          "utf8",
        ),
        {
          locale,
          sourceLocale: "en",
          filename: `locales/${locale}/messages.po`,
        },
      ),
    }));
    const ids = Object.keys(catalogs[0].messages).sort();
    expect(ids.length).toBeGreaterThan(0);
    for (const catalog of catalogs) {
      expect(Object.keys(catalog.messages).sort()).toEqual(ids);
      const translations = Object.fromEntries(
        Object.entries(catalog.messages).map(([id, entry]) => {
          expect(entry.translation.trim(), `${catalog.locale}:${id}`).not.toBe(
            "",
          );
          return [id, entry.translation];
        }),
      );
      expect(
        createCompiledCatalog(catalog.locale, translations, { strict: true })
          .errors,
      ).toEqual([]);
    }
    expect(
      createCompiledCatalog(
        "en",
        { broken: "{count, plural, one {meal}" },
        { strict: true },
      ).errors,
    ).not.toEqual([]);
  });

  it("translates recipe data, plurals and reordered interpolation", () => {
    const i18n = setupI18n({ locale: "en", messages: { en, zh } });
    expect(i18n._(recipes[0].name)).toBe("Lemon & herb chicken");
    const catalog = formatter().parse(
      readFileSync(
        new URL("../../../packages/shared/locales/en/messages.po", import.meta.url),
        "utf8",
      ),
      { locale: "en", sourceLocale: "en", filename: "locales/en/messages.po" },
    );
    const remainingId = Object.entries(catalog).find(
      ([, entry]) =>
        entry.message ===
        "{0, plural, one {Choose # more meal} other {Choose # more meals}}",
    )?.[0];
    expect(remainingId).toBeTruthy();
    const remaining = (count: number) => i18n._(remainingId!, { 0: count });
    expect(remaining(1)).toBe("Choose 1 more meal");
    expect(remaining(2)).toBe("Choose 2 more meals");
    expect(i18n._(boxSizeMessage(3, 1))).toBe("3 meals for 1 person");
    expect(i18n._(boxSizeMessage(3, 2))).toBe("3 meals for 2 people");
    i18n.activate("zh");
    expect(i18n._(recipes[0].name)).toBe("柠檬香草烤鸡");
    expect(remaining(1)).toBe("再选 1 道菜");
    expect(remaining(2)).toBe("再选 2 道菜");
    expect(i18n._(boxSizeMessage(3, 2))).toBe("3 道菜，每道 2 人份");
    expect(i18n._(boxSizeMessage(3, 1))).toBe("3 道菜，每道 1 人份");
    expect(i18n._("Order not found.")).toBe("未找到订单。");
  });

  it("resolves supported locales and formats independently of market", () => {
    expect(resolveLocale("zh-CN")).toBe("zh");
    expect(resolveLocale("en-GB")).toBe("en");
    expect(resolveLocale("unsupported")).toBe("en");
    expect(resolveLocale("__proto__")).toBe("en");
    expect(money(1099, "us", "zh")).toContain("US$");
    expect(money(3900, "cn", "en")).toContain("CN¥");
    expect(deliveryLabel("2026-10-09", "cn", "zh")).not.toBe(
      deliveryLabel("2026-10-09", "cn", "en"),
    );
  });
});
